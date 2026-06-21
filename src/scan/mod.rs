use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use std::os::unix::fs::{FileTypeExt, MetadataExt};

use serde::{Deserialize, Serialize};

use color_eyre::eyre::{eyre, Result, WrapErr};

use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

use async_recursion::async_recursion;

use log;

use crate::profile::Ignore;
use regex::Regex;
pub type Regexes = Vec<Regex>;
fn is_match(regexes: &Regexes, p: &Path) -> bool {
    if let Some(s) = p.file_name() {
        if let Some(s) = s.to_str() {
            for r in regexes {
                if r.is_match(s) {
                    return true;
                }
            }
        }
    }
    false
}

pub mod change;
pub mod location;

pub use change::{changes, Change};
use location::{Location, Locations};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntryWithMeta {
    path: PathBuf,
    size: u64,
    mtime: i64,
    ino: u64,
    mode: u32,
    target: Option<PathBuf>,
    is_dir: bool,
    checksum: u32,
    // TODO: uid and gid
}

impl DirEntryWithMeta {
    #[cfg(test)]
    pub(crate) fn test_file(path: PathBuf, checksum: u32) -> Self {
        Self {
            path,
            size: 0,
            mtime: 0,
            ino: 0,
            mode: 0o100644,
            target: None,
            is_dir: false,
            checksum,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_file_with_size(path: PathBuf, size: u64, checksum: u32) -> Self {
        Self {
            path,
            size,
            mtime: 0,
            ino: 0,
            mode: 0o100644,
            target: None,
            is_dir: false,
            checksum,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_dir(path: PathBuf) -> Self {
        Self {
            path,
            size: 0,
            mtime: 0,
            ino: 0,
            mode: 0o40755,
            target: None,
            is_dir: true,
            checksum: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_symlink(path: PathBuf, target: PathBuf) -> Self {
        Self::test_symlink_with_mode(path, target, 0o120777)
    }

    #[cfg(test)]
    pub(crate) fn test_symlink_with_mode(path: PathBuf, target: PathBuf, mode: u32) -> Self {
        Self::test_symlink_with_mode_and_mtime(path, target, mode, 0)
    }

    #[cfg(test)]
    pub(crate) fn test_symlink_with_mode_and_mtime(
        path: PathBuf,
        target: PathBuf,
        mode: u32,
        mtime: i64,
    ) -> Self {
        Self {
            path,
            size: 0,
            mtime,
            ino: 0,
            mode,
            target: Some(target),
            is_dir: false,
            checksum: 0,
        }
    }

    fn same(&self, other: &Self) -> bool {
        assert_eq!(self.path, other.path);
        (self.is_symlink() || self.mode == other.mode)
            && self.target == other.target
            && self.is_dir == other.is_dir
            && (self.is_dir || self.same_contents(other))
    }

    pub fn starts_with<P: AsRef<Path>>(&self, path: P) -> bool {
        self.path.starts_with(path)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn target(&self) -> &Option<PathBuf> {
        &self.target
    }

    pub fn same_contents(&self, other: &Self) -> bool {
        self.size == other.size && self.mtime == other.mtime && self.ino == other.ino
    }

    pub fn is_symlink(&self) -> bool {
        self.target.is_some()
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn mtime(&self) -> i64 {
        self.mtime
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn checksum(&self) -> u32 {
        self.checksum
    }

    pub fn set_ino(&mut self, ino: u64) {
        self.ino = ino;
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        !(self.is_dir || self.is_symlink())
    }

    pub async fn compute_checksum(&mut self, base: &PathBuf) -> Result<()> {
        if !self.is_file() {
            return Ok(());
        }

        let filename = base.join(&self.path);
        log::trace!("Computing checksum for {}", filename.display());

        use adler32::RollingAdler32;
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&filename)
            .await
            .wrap_err_with(|| format!("unable to open {} for checksum", filename.display()))?;
        let mut hash = RollingAdler32::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .wrap_err_with(|| format!("unable to read {} for checksum", filename.display()))?;
            if read == 0 {
                break;
            }
            hash.update_buffer(&buffer[..read]);
        }
        self.checksum = hash.hash();

        Ok(())
    }
}

impl PartialEq for DirEntryWithMeta {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for DirEntryWithMeta {}

impl PartialOrd for DirEntryWithMeta {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DirEntryWithMeta {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path.cmp(&other.path)
    }
}

#[derive(Debug, Clone)]
struct ParentFromTo {
    parent: usize,
    from: usize,
    to: usize,
}

fn narrow_parent_from_to(pft: ParentFromTo, path: &PathBuf, locations: &Locations) -> ParentFromTo {
    let mut parent = pft.parent;
    let mut from = pft.from;
    let mut to = pft.to;

    // update descendants
    while from <= to && !locations[from].path().starts_with(path) {
        from += 1;
    }
    if from <= to {
        let parent_to = to;
        to = from;
        while to < parent_to && locations[to + 1].path().starts_with(path) {
            to += 1;
        }
    }

    // update parent
    if from <= to && locations[from].path() == path {
        parent = from;
    }

    ParentFromTo { parent, from, to }
}

pub fn relative<'a>(base: &PathBuf, path: &'a PathBuf) -> &'a Path {
    path.strip_prefix(&base).unwrap()
}

fn find_parent<'a>(path: &PathBuf, locations: &'a Locations, pft: &ParentFromTo) -> &'a Location {
    let parent = pft.parent;
    let mut from = pft.from;
    let to = pft.to;

    while from <= to && from < locations.len() {
        if locations[from].path() == path {
            return &locations[from];
        }
        from += 1;
    }
    &locations[parent]
}

fn is_relevant_to_restrict(path: &Path, restrict: &Path) -> bool {
    path.starts_with(restrict) || restrict.starts_with(path)
}

#[async_recursion]
async fn scan_dir(
    path: PathBuf,
    locations: Arc<Locations>,
    restrict: Arc<PathBuf>,
    base: Arc<PathBuf>,
    ignore: Arc<Regexes>,
    pft: ParentFromTo,
    dev: u64,
    tx: mpsc::Sender<DirEntryWithMeta>,
    s: Arc<Semaphore>,
) -> Result<()> {
    log::trace!("Scanning: {}", path.display());

    // check the restriction
    if !path.starts_with(&*restrict) && !restrict.starts_with(&path) {
        log::trace!("Skipping (restriction): {:?} vs {:?}", path, restrict);
        return Ok(());
    }

    let pft = narrow_parent_from_to(pft, &path, &locations);

    // no need to descend if we are in the exclude regime and there are no descendants
    if locations[pft.parent].is_exclude() && pft.from > pft.to {
        log::trace!("Skipping excluded: {:?}", path);
        return Ok(());
    }

    // read the directory
    use tokio::fs;
    let mut child_dirs = Vec::new();

    let _sp = s.acquire().await.wrap_err("scanner semaphore closed")?;
    let mut dir = fs::read_dir(&path)
        .await
        .wrap_err_with(|| format!("unable to read directory {}", path.display()))?;
    while let Some(child) = dir
        .next_entry()
        .await
        .wrap_err_with(|| format!("unable to read next directory entry in {}", path.display()))?
    {
        let path = child.path();

        if is_match(&ignore, &path) {
            log::trace!("Skipping (ignored): {:?}", path);
            continue;
        }

        let meta = fs::symlink_metadata(&path)
            .await
            .wrap_err_with(|| format!("unable to read metadata for {}", path.display()))?;

        let file_type = meta.file_type();
        let location = find_parent(&path, &locations, &pft);
        let child_pft = narrow_parent_from_to(pft.clone(), &path, &locations);
        let has_descendant_includes = child_pft.from <= child_pft.to
            && (child_pft.from..=child_pft.to)
                .any(|i| locations[i].is_include() && locations[i].path() != &path);

        if location.is_exclude() && !has_descendant_includes {
            log::trace!("Not reporting (excluded): {:?}", path);
            continue;
        }

        if file_type.is_block_device()
            || file_type.is_char_device()
            || file_type.is_fifo()
            || file_type.is_socket()
        {
            if path.starts_with(&*restrict) {
                return Err(eyre!(
                    "unsupported special file in sync tree: {}",
                    path.display()
                ));
            }
            log::trace!("Skipping special file outside restriction: {:?}", path);
            continue;
        }

        if meta.is_dir() && dev != meta.dev() && is_relevant_to_restrict(&path, &restrict) {
            return Err(eyre!(
                "refusing to cross filesystem boundary at {}",
                path.display()
            ));
        }

        if meta.is_dir() && dev == meta.dev() {
            let path = path.clone();
            child_dirs.push(path);
        }

        if location.is_exclude() {
            log::trace!("Not reporting (excluded): {:?}", path);
            continue;
        }

        // check restriction and crossing the filesystem boundary
        if path.starts_with(&*restrict) && dev == meta.dev() {
            log::trace!("Reporting: {:?}", path);
            let target = if file_type.is_symlink() {
                Some(fs::read_link(&path).await.wrap_err_with(|| {
                    format!("unable to read symlink target for {}", path.display())
                })?)
            } else {
                None
            };

            tx.send(DirEntryWithMeta {
                path: relative(&*base, &path).to_path_buf(),
                target,
                size: meta.size(),
                mtime: meta.mtime(),
                ino: meta.ino(),
                mode: meta.mode(),
                is_dir: meta.is_dir(),
                checksum: 0,
            })
            .await
            .map_err(|_| eyre!("unable to send scan result for {}", path.display()))?
        }
    }
    drop(dir);
    drop(_sp);

    scan_children(
        child_dirs,
        locations,
        restrict,
        base,
        ignore,
        pft,
        dev,
        tx,
        s.clone(),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[tokio::test]
    async fn scan_rejects_included_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        let err = scan(
            temp.path(),
            "",
            &vec![Location::Include(PathBuf::new())],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("unsupported special file"));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn scan_ignores_excluded_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "",
            &vec![
                Location::Include(PathBuf::new()),
                Location::Exclude(PathBuf::from("socket")),
            ],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn scan_ignores_excluded_special_files_with_descendant_excludes() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "",
            &vec![
                Location::Include(PathBuf::new()),
                Location::Exclude(PathBuf::from("socket")),
                Location::Exclude(PathBuf::from("socket/child")),
            ],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn restricted_scan_ignores_special_file_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        tokio::fs::create_dir_all(temp.path().join("wanted"))
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("wanted/file.txt"), b"data")
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "wanted",
            &vec![Location::Include(PathBuf::new())],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        let mut paths = Vec::new();
        while let Some(entry) = rx.recv().await {
            paths.push(entry.path().clone());
        }

        assert!(paths.contains(&PathBuf::from("wanted")));
        assert!(paths.contains(&PathBuf::from("wanted/file.txt")));
        assert!(!paths.contains(&PathBuf::from("socket")));
    }

    #[tokio::test]
    async fn scan_still_descends_into_excluded_dir_for_included_child() {
        let temp = tempfile::tempdir().unwrap();
        let nested_dir = temp.path().join("dir").join("nested");
        tokio::fs::create_dir_all(&nested_dir).await.unwrap();
        tokio::fs::write(nested_dir.join("file.txt"), b"data")
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "",
            &vec![
                Location::Include(PathBuf::new()),
                Location::Exclude(PathBuf::from("dir")),
                Location::Include(PathBuf::from("dir/nested")),
            ],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        let mut paths = Vec::new();
        while let Some(entry) = rx.recv().await {
            paths.push(entry.path().clone());
        }

        assert!(paths.contains(&PathBuf::from("dir/nested")));
        assert!(paths.contains(&PathBuf::from("dir/nested/file.txt")));
        assert!(!paths.contains(&PathBuf::from("dir")));
    }

    #[tokio::test]
    async fn scan_handles_deep_directory_trees() {
        let temp = tempfile::tempdir().unwrap();
        let mut nested_dir = temp.path().to_path_buf();
        let mut deepest_relative = PathBuf::new();

        for i in 0..70 {
            let component = format!("dir{i}");
            nested_dir.push(&component);
            deepest_relative.push(component);
        }

        tokio::fs::create_dir_all(&nested_dir).await.unwrap();
        tokio::fs::write(nested_dir.join("file.txt"), b"data")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel(128);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            scan(
                temp.path(),
                "",
                &vec![Location::Include(PathBuf::new())],
                &Vec::new(),
                tx,
            ),
        )
        .await
        .unwrap()
        .unwrap();

        let mut paths = Vec::new();
        while let Some(entry) = rx.recv().await {
            paths.push(entry.path().clone());
        }

        assert!(paths.contains(&deepest_relative.join("file.txt")));
    }

    #[tokio::test]
    async fn compute_checksum_streams_file_contents() {
        let temp = tempfile::tempdir().unwrap();
        let contents = (0..100_000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        tokio::fs::write(temp.path().join("file.bin"), &contents)
            .await
            .unwrap();
        let mut entry = DirEntryWithMeta::test_file_with_size(
            PathBuf::from("file.bin"),
            contents.len() as u64,
            0,
        );

        entry
            .compute_checksum(&temp.path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(entry.checksum(), adler32::adler32(&contents[..]).unwrap());
    }

    #[test]
    fn filesystem_boundary_relevance_includes_restrict_ancestors() {
        let restrict = Path::new("/base/mount/wanted");

        assert!(is_relevant_to_restrict(Path::new("/base/mount"), restrict));
        assert!(is_relevant_to_restrict(
            Path::new("/base/mount/wanted"),
            restrict
        ));
        assert!(is_relevant_to_restrict(
            Path::new("/base/mount/wanted/child"),
            restrict
        ));
        assert!(!is_relevant_to_restrict(Path::new("/base/other"), restrict));
    }
}

async fn scan_children(
    children: Vec<PathBuf>,
    locations: Arc<Locations>,
    restrict: Arc<PathBuf>,
    base: Arc<PathBuf>,
    ignore: Arc<Regexes>,
    pft: ParentFromTo,
    dev: u64,
    tx: mpsc::Sender<DirEntryWithMeta>,
    s: Arc<Semaphore>,
) -> Result<()> {
    for path in children {
        scan_dir(
            path,
            locations.clone(),
            restrict.clone(),
            base.clone(),
            ignore.clone(),
            pft.clone(),
            dev,
            tx.clone(),
            s.clone(),
        )
        .await?;
    }

    Ok(())
}

/// Send all [directory entries](DirEntryWithMeta) into the channel, given via its [Sender](mpsc::Sender) `tx`.
///
/// # Arguments
///
/// * `base` - root path of the scan, `locations` are specified relative to this path
/// * `path` - restriction under base, which should be scanned
/// * `locations` - [locations](Locations) to scan
/// * `tx` - [Sender](mpsc::Sender) of the channel, where to send the [directory entries](DirEntryWithMeta)
pub async fn scan<P: AsRef<Path>, Q: AsRef<Path>>(
    base: P,
    path: Q,
    locations: &Locations,
    ignore: &Ignore,
    tx: mpsc::Sender<DirEntryWithMeta>,
) -> Result<()> {
    let base = PathBuf::from(base.as_ref());
    let mut restrict = Arc::new(PathBuf::from(&base));
    (*Arc::get_mut(&mut restrict).unwrap()).push(path);
    let base = Arc::new(base);

    log::info!("Going to scan: {}", restrict.display());

    let dev = base
        .symlink_metadata()
        .wrap_err_with(|| format!("unable to read metadata for scan base {}", base.display()))?
        .dev();
    let mut locations: Arc<Locations> =
        Arc::new(locations.iter().map(|l| l.prefix(&base)).collect());
    (*Arc::get_mut(&mut locations).unwrap()).sort();

    // build ignore regex
    use fnmatch_regex::glob_to_regex;
    let mut ignore_regex: Regexes = Vec::new();
    for p in ignore {
        ignore_regex
            .push(glob_to_regex(p).wrap_err_with(|| format!("invalid ignore pattern {p}"))?);
    }
    let ignore = Arc::new(ignore_regex.clone());

    let s = Arc::new(Semaphore::new(64));

    let path = (*base).clone();
    let to = locations.len() - 1;
    scan_dir(
        path,
        locations,
        restrict,
        base,
        ignore,
        ParentFromTo {
            parent: 0,
            from: 0,
            to: to,
        },
        dev,
        tx,
        s,
    )
    .await?;

    Ok(())
}
