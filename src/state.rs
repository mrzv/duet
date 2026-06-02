use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;

use bincode::serde::decode_from_std_read as deserialize_from;
use bincode::serde::encode_into_std_write as serialize_into;
use color_eyre::eyre::Result;
use tokio::sync::mpsc;

use crate::profile;
use crate::scan::location::Locations;
use crate::scan::{self, Change, DirEntryWithMeta};

pub type Entries = Vec<DirEntryWithMeta>;
pub type Changes = Vec<Change>;

pub fn load_entries(statefile: &PathBuf) -> Result<Entries> {
    let entries = if statefile.exists() {
        log::debug!("Loading: {}", statefile.display());
        let mut f = BufReader::new(File::open(statefile)?);
        deserialize_from(&mut f, bincode::config::legacy())?
    } else {
        Vec::new()
    };

    Ok(entries)
}

pub fn save_entries(statefile: &PathBuf, entries: &Entries) -> Result<()> {
    let mut f = BufWriter::new(File::create(statefile)?);
    serialize_into(entries, &mut f, bincode::config::legacy())?;
    Ok(())
}

pub async fn scan_entries(
    base: &PathBuf,
    path: &PathBuf,
    locations: &Locations,
    ignore: &profile::Ignore,
) -> Result<Entries> {
    let base = base.clone();
    let path = path.clone();
    let locations = locations.clone();
    let ignore = ignore.clone();

    let mut entries = async move {
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move { scan::scan(&base, &path, &locations, &ignore, tx).await });

        let pb = indicatif::ProgressBar::new(1);
        pb.set_style(
            indicatif::ProgressStyle::default_spinner()
                .template("[{elapsed_precise}] {wide_msg}")
                .expect("Failed to set style for a progress bar"),
        );
        let mut entries: Entries = Entries::new();
        while let Some(e) = rx.recv().await {
            pb.set_message(e.path().display().to_string());
            entries.push(e);
        }
        pb.finish_and_clear();

        entries
    }
    .await;
    log::debug!("Done scanning");

    entries.sort();

    Ok(entries)
}

pub async fn old_and_changes(
    base: &PathBuf,
    restrict: &PathBuf,
    locations: &Locations,
    ignore: &profile::Ignore,
    statefile: Option<&PathBuf>,
) -> Result<(Entries, Changes)> {
    let restricted_current_scan = scan_entries(base, restrict, locations, ignore);

    use tokio::fs::File;
    use tokio::io::AsyncReadExt;
    let all_old_entries = async {
        let all_old_entries: Entries = if let Some(f) = statefile {
            if f.exists() {
                log::debug!("Loading: {}", f.display());
                let mut f = File::open(f).await?;
                let mut contents = vec![];
                f.read_to_end(&mut contents).await?;
                log::debug!("Done loading");
                let mut contents = contents.as_slice();
                deserialize_from(&mut contents, bincode::config::legacy())?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        log::debug!("Done reading out entries");
        Ok::<Entries, color_eyre::eyre::Report>(all_old_entries)
    };

    use tokio::join;
    let (all_old_entries, restricted_current_scan) =
        join!(all_old_entries, restricted_current_scan);
    let all_old_entries = all_old_entries?;
    let restricted_current_scan = restricted_current_scan?;
    let restricted_old_entries_iter = all_old_entries
        .iter()
        .filter(move |dir: &&scan::DirEntryWithMeta| dir.starts_with(restrict));

    let mut changes: Vec<_> =
        scan::changes(restricted_old_entries_iter, restricted_current_scan.iter()).collect();

    log::debug!("Computing checksums for {} changes", changes.len());
    let pb = indicatif::ProgressBar::new(changes.len() as u64);
    let style = indicatif::ProgressStyle::default_bar()
        .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")?
        .progress_chars("##-");
    pb.set_style(style);
    pb.set_message("computing checksums");
    let base = PathBuf::from(base);
    for change in &mut changes {
        pb.inc(1);
        match change {
            Change::Added(n) => {
                n.compute_checksum(&base).await?;
            }
            Change::Modified(_, n) => {
                n.compute_checksum(&base).await?;
            }
            Change::Removed(_) => {}
        }
    }
    pb.finish_and_clear();

    Ok((all_old_entries, changes))
}
