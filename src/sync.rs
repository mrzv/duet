use super::scan::{Change, DirEntryWithMeta as Entry};
use color_eyre::eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use crate::actions::Action;

use crate::rustsync::{compare, compare_stream, restore_seek, signature, DeltaOp};
pub use crate::rustsync::{Delta, Signature};

const WINDOW: usize = 1024; // TODO: figure out appropriate window size

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureWithPath(PathBuf, Signature);

pub fn get_signatures(base: &PathBuf, actions: &Vec<Action>) -> Result<Vec<SignatureWithPath>> {
    let mut signatures: Vec<SignatureWithPath> = Vec::new();
    for action in actions {
        match action {
            Action::Local(Change::Modified(e1, e2))
            | Action::ResolvedLocal((_, _), Change::Modified(e1, e2)) => {
                if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                    let f = fs::File::open(base.join(e1.path()))?;
                    let block = [0; WINDOW];
                    let sig = signature(f, block)?;
                    signatures.push(SignatureWithPath(e1.path().clone(), sig));
                }
            }
            _ => {}
        }
    }
    Ok(signatures)
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ChangeDetails {
    Contents(Vec<u8>),
    Diff(Delta),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DetailStreamId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApplyStreamId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailFrame {
    pub action_index: u32,
    pub payload: DetailPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DetailPayload {
    FileBegin,
    FileBytes(Vec<u8>),
    FileEnd,
    DiffBegin,
    DiffCopy { offset: u64, len: u64 },
    DiffBytes(Vec<u8>),
    DiffEnd,
}

pub fn detail_transfer_bytes(actions: &[Action]) -> u64 {
    actions.iter().map(action_detail_bytes).sum()
}

pub fn detail_frame_transfer_bytes(frame: &DetailFrame) -> u64 {
    match &frame.payload {
        DetailPayload::FileBytes(bytes) | DetailPayload::DiffBytes(bytes) => bytes.len() as u64,
        DetailPayload::DiffCopy { len, .. } => *len,
        DetailPayload::FileBegin
        | DetailPayload::FileEnd
        | DetailPayload::DiffBegin
        | DetailPayload::DiffEnd => 0,
    }
}

pub fn detail_frames_transfer_bytes(frames: &[DetailFrame]) -> u64 {
    frames.iter().map(detail_frame_transfer_bytes).sum()
}

fn action_detail_bytes(action: &Action) -> u64 {
    let change = match action {
        Action::Local(change)
        | Action::Remote(change)
        | Action::ResolvedLocal((_, _), change)
        | Action::ResolvedRemote((_, _), change) => change,
        Action::Conflict(_, _) | Action::Identical(_, _) => return 0,
    };

    match change {
        Change::Removed(_) => 0,
        Change::Added(entry) => entry.is_file().then_some(entry.size()).unwrap_or(0),
        Change::Modified(old, new) => {
            if new.is_file() && (!old.is_file() || !old.same_contents(new)) {
                new.size()
            } else {
                0
            }
        }
    }
}

pub fn can_stream_details(actions: &[Action]) -> bool {
    actions.iter().all(|action| {
        let change = match action {
            Action::Local(change)
            | Action::Remote(change)
            | Action::ResolvedLocal((_, _), change)
            | Action::ResolvedRemote((_, _), change) => change,
            Action::Conflict(_, _) | Action::Identical(_, _) => return true,
        };

        !matches!(change, Change::Modified(old, new) if old.is_dir() && new.is_file())
    })
}

pub fn get_detailed_changes(
    base: &PathBuf,
    actions: &Vec<Action>,
    signatures: &Vec<SignatureWithPath>,
) -> Result<Vec<ChangeDetails>> {
    let mut sig_iter = signatures.iter();
    let mut details: Vec<ChangeDetails> = Vec::new();

    for action in actions {
        match action {
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => {
                match change {
                    Change::Removed(_) => {}
                    Change::Added(e) => {
                        if e.is_file() {
                            log::debug!("Getting detail for adding {}", e.path().display());
                            let v = fs::read(base.join(e.path()))?;
                            details.push(ChangeDetails::Contents(v));
                        }
                    }
                    Change::Modified(e1, e2) => {
                        if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                            let block = [0; WINDOW];
                            let f = fs::File::open(base.join(e1.path()))?;
                            let sig = &sig_iter.next().unwrap().1;
                            let delta = compare(sig, f, block)?;
                            details.push(ChangeDetails::Diff(delta))
                        } else if !e1.is_file() && e2.is_file() {
                            let v = fs::read(base.join(e2.path()))?;
                            details.push(ChangeDetails::Contents(v));
                        } // else: permissions or target change
                    }
                }
            }
            _ => {}
        }
    }
    Ok(details)
}

enum ProducerState {
    File {
        action_index: u32,
        file: fs::File,
    },
    Diff {
        receiver: mpsc::Receiver<Result<DetailFrame>>,
        handle: thread::JoinHandle<()>,
    },
}

pub struct DetailProducer {
    base: PathBuf,
    actions: Vec<Action>,
    signatures: Vec<SignatureWithPath>,
    max_chunk_bytes: usize,
    action_index: usize,
    signature_index: usize,
    pending: VecDeque<DetailFrame>,
    state: Option<ProducerState>,
}

impl DetailProducer {
    pub fn new(
        base: PathBuf,
        actions: Vec<Action>,
        signatures: Vec<SignatureWithPath>,
        max_chunk_bytes: usize,
    ) -> Self {
        DetailProducer {
            base,
            actions,
            signatures,
            max_chunk_bytes: max_chunk_bytes.max(1),
            action_index: 0,
            signature_index: 0,
            pending: VecDeque::new(),
            state: None,
        }
    }

    pub fn next_frame(&mut self) -> Result<Option<DetailFrame>> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }

        if let Some(state) = self.state.take() {
            match state {
                ProducerState::File {
                    action_index,
                    mut file,
                } => {
                    let mut buf = vec![0; self.max_chunk_bytes];
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        return Ok(Some(DetailFrame {
                            action_index,
                            payload: DetailPayload::FileEnd,
                        }));
                    }

                    buf.truncate(n);
                    self.state = Some(ProducerState::File { action_index, file });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::FileBytes(buf),
                    }));
                }
                ProducerState::Diff { receiver, handle } => match receiver.recv() {
                    Ok(frame) => {
                        let frame = frame?;
                        let done = matches!(frame.payload, DetailPayload::DiffEnd);
                        if done {
                            let _ = handle.join();
                        } else {
                            self.state = Some(ProducerState::Diff { receiver, handle });
                        }
                        return Ok(Some(frame));
                    }
                    Err(_) => {
                        let _ = handle.join();
                        return Err(eyre!("detail diff stream ended without DiffEnd"));
                    }
                },
            }
        }

        while self.action_index < self.actions.len() {
            let index = self.action_index;
            self.action_index += 1;
            let action_index = index as u32;
            let Some(kind) = source_detail_kind(&self.actions[index]) else {
                continue;
            };

            match kind {
                SourceDetailKind::File(path) => {
                    let file = fs::File::open(self.base.join(path))?;
                    self.state = Some(ProducerState::File { action_index, file });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::FileBegin,
                    }));
                }
                SourceDetailKind::Diff(path) => {
                    let signature = self
                        .signatures
                        .get(self.signature_index)
                        .ok_or_else(|| eyre!("missing signature for {}", path.display()))?
                        .1
                        .clone();
                    self.signature_index += 1;

                    let file_path = self.base.join(path);
                    let max_chunk_bytes = self.max_chunk_bytes;
                    let (sender, receiver) = mpsc::sync_channel(4);
                    let handle = thread::spawn(move || {
                        let result = stream_diff_frames(
                            file_path,
                            action_index,
                            signature,
                            max_chunk_bytes,
                            sender.clone(),
                        );
                        if let Err(error) = result {
                            let _ = sender.send(Err(error));
                        }
                    });
                    self.state = Some(ProducerState::Diff { receiver, handle });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::DiffBegin,
                    }));
                }
            }
        }

        Ok(None)
    }

    pub fn next_frames(
        &mut self,
        max_frames: usize,
        max_payload_bytes: usize,
    ) -> Result<Vec<DetailFrame>> {
        let max_frames = max_frames.max(1);
        let max_payload_bytes = max_payload_bytes.max(1);
        let mut frames = Vec::new();
        let mut payload_bytes = 0;

        while frames.len() < max_frames {
            let Some(frame) = self.next_frame()? else {
                break;
            };

            let frame_payload_bytes = detail_payload_bytes(&frame.payload);
            if !frames.is_empty() && payload_bytes + frame_payload_bytes > max_payload_bytes {
                self.pending.push_front(frame);
                break;
            }

            payload_bytes += frame_payload_bytes;
            frames.push(frame);
        }

        Ok(frames)
    }
}

fn detail_payload_bytes(payload: &DetailPayload) -> usize {
    match payload {
        DetailPayload::FileBytes(bytes) | DetailPayload::DiffBytes(bytes) => bytes.len(),
        DetailPayload::FileBegin
        | DetailPayload::FileEnd
        | DetailPayload::DiffBegin
        | DetailPayload::DiffCopy { .. }
        | DetailPayload::DiffEnd => 0,
    }
}

enum SourceDetailKind<'a> {
    File(&'a PathBuf),
    Diff(&'a PathBuf),
}

fn source_detail_kind(action: &Action) -> Option<SourceDetailKind<'_>> {
    let change = match action {
        Action::Remote(change) | Action::ResolvedRemote((_, _), change) => change,
        _ => return None,
    };

    match change {
        Change::Removed(_) => None,
        Change::Added(e) => e.is_file().then(|| SourceDetailKind::File(e.path())),
        Change::Modified(e1, e2) => {
            if e1.is_file() && e2.is_file() && !e1.same_contents(e2) {
                Some(SourceDetailKind::Diff(e1.path()))
            } else if !e1.is_file() && e2.is_file() {
                Some(SourceDetailKind::File(e2.path()))
            } else {
                None
            }
        }
    }
}

fn stream_diff_frames(
    file_path: PathBuf,
    action_index: u32,
    signature: Signature,
    max_chunk_bytes: usize,
    sender: mpsc::SyncSender<Result<DetailFrame>>,
) -> Result<()> {
    let file = fs::File::open(file_path)?;
    let block = vec![0; signature.window];
    let mut pending_copy: Option<(u64, u64)> = None;

    let send_frame = |payload| {
        sender
            .send(Ok(DetailFrame {
                action_index,
                payload,
            }))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "detail stream closed")
            })
    };

    let flush_copy = |pending_copy: &mut Option<(u64, u64)>| {
        if let Some((offset, len)) = pending_copy.take() {
            send_frame(DetailPayload::DiffCopy { offset, len })?;
        }
        Ok::<(), std::io::Error>(())
    };

    compare_stream(&signature, file, block, max_chunk_bytes, |op| {
        match op {
            DeltaOp::FromSource(offset) => {
                let copy_len = signature.window as u64;
                match &mut pending_copy {
                    Some((start, len)) if *start + *len == offset => *len += copy_len,
                    Some(_) => {
                        flush_copy(&mut pending_copy)?;
                        pending_copy = Some((offset, copy_len));
                    }
                    None => pending_copy = Some((offset, copy_len)),
                }
            }
            DeltaOp::Literal(bytes) => {
                flush_copy(&mut pending_copy)?;
                send_frame(DetailPayload::DiffBytes(bytes))?;
            }
        }
        Ok(())
    })?;
    flush_copy(&mut pending_copy)?;
    send_frame(DetailPayload::DiffEnd)?;
    Ok(())
}

struct TempOutput {
    final_path: PathBuf,
    temp_path: PathBuf,
    file: Option<fs::File>,
}

impl TempOutput {
    fn new(final_path: PathBuf) -> Result<Self> {
        let mut temp_path = final_path.clone();
        let file_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("duet-output");
        temp_path.set_file_name(format!(
            ".{}.duet-part-{}-{:?}",
            file_name,
            std::process::id(),
            thread::current().id()
        ));
        let file = fs::File::create(&temp_path)?;
        Ok(TempOutput {
            final_path,
            temp_path,
            file: Some(file),
        })
    }

    fn finish(mut self) -> Result<()> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| eyre!("temporary output is closed"))?;
        file.flush()?;
        drop(file);
        fs::rename(&self.temp_path, &self.final_path)?;
        Ok(())
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp_path);
    }
}

enum ApplyState {
    File {
        action_index: usize,
        output: TempOutput,
    },
    Diff {
        action_index: usize,
        source: fs::File,
        output: TempOutput,
    },
}

pub struct DetailApplier {
    base: PathBuf,
    actions: Vec<Action>,
    all_old: Vec<Entry>,
    old_index: usize,
    action_index: usize,
    new_entries: Vec<Entry>,
    state: Option<ApplyState>,
}

impl DetailApplier {
    pub fn new(base: PathBuf, actions: Vec<Action>, all_old: Vec<Entry>) -> Self {
        DetailApplier {
            base,
            actions,
            all_old,
            old_index: 0,
            action_index: 0,
            new_entries: Vec::new(),
            state: None,
        }
    }

    pub fn apply_frame(&mut self, frame: DetailFrame) -> Result<()> {
        let frame_index = frame.action_index as usize;
        if frame_index >= self.actions.len() {
            return Err(eyre!(
                "detail frame references missing action {}",
                frame_index
            ));
        }

        match &mut self.state {
            Some(ApplyState::File {
                action_index,
                output,
            }) => {
                if *action_index != frame_index {
                    return Err(eyre!(
                        "detail frame for action {} arrived while applying action {}",
                        frame_index,
                        action_index
                    ));
                }
                match frame.payload {
                    DetailPayload::FileBytes(bytes) => output
                        .file
                        .as_mut()
                        .ok_or_else(|| eyre!("temporary output is closed"))?
                        .write_all(&bytes)?,
                    DetailPayload::FileEnd => self.finish_file_detail()?,
                    _ => return Err(eyre!("unexpected file detail frame")),
                }
                return Ok(());
            }
            Some(ApplyState::Diff {
                action_index,
                source,
                output,
            }) => {
                if *action_index != frame_index {
                    return Err(eyre!(
                        "detail frame for action {} arrived while applying action {}",
                        frame_index,
                        action_index
                    ));
                }
                match frame.payload {
                    DetailPayload::DiffCopy { offset, len } => {
                        let output_file = output
                            .file
                            .as_mut()
                            .ok_or_else(|| eyre!("temporary output is closed"))?;
                        copy_from_source(source, output_file, offset, len)?;
                    }
                    DetailPayload::DiffBytes(bytes) => output
                        .file
                        .as_mut()
                        .ok_or_else(|| eyre!("temporary output is closed"))?
                        .write_all(&bytes)?,
                    DetailPayload::DiffEnd => self.finish_file_detail()?,
                    _ => return Err(eyre!("unexpected diff detail frame")),
                }
                return Ok(());
            }
            None => {}
        }

        self.advance_to_action(frame_index)?;
        match frame.payload {
            DetailPayload::FileBegin => self.begin_file_detail(frame_index),
            DetailPayload::DiffBegin => self.begin_diff_detail(frame_index),
            _ => Err(eyre!(
                "detail stream for action {} did not begin with a begin frame",
                frame_index
            )),
        }
    }

    pub fn finish(mut self) -> Result<Vec<Entry>> {
        if self.state.is_some() {
            return Err(eyre!("detail stream ended with an unfinished file"));
        }
        self.advance_to_action(self.actions.len())?;
        self.apply_directory_second_pass()?;

        for e in self.all_old.iter().skip(self.old_index) {
            self.new_entries.push(e.clone());
        }
        self.new_entries.sort();
        Ok(self.new_entries)
    }

    fn advance_to_action(&mut self, target_index: usize) -> Result<()> {
        while self.action_index < target_index {
            if apply_detail_kind(&self.actions[self.action_index]).is_some() {
                return Err(eyre!(
                    "missing detail frames for action {}",
                    self.action_index
                ));
            }
            self.apply_action_without_detail(self.action_index)?;
            self.action_index += 1;
        }
        Ok(())
    }

    fn prepare_action(&mut self, action_index: usize) {
        let path = self.actions[action_index].path();
        loop {
            let oe = self.all_old.get(self.old_index);
            if let Some(e) = oe {
                match e.path().cmp(path) {
                    Ordering::Less => {
                        self.new_entries.push(e.clone());
                        self.old_index += 1;
                    }
                    Ordering::Equal => {
                        let e = e.clone();
                        self.old_index += 1;
                        if self.actions[action_index].is_unresolved_conflict() {
                            self.new_entries.push(e);
                        }
                        continue;
                    }
                    Ordering::Greater => break,
                }
            } else {
                break;
            }
        }
    }

    fn apply_action_without_detail(&mut self, action_index: usize) -> Result<()> {
        self.prepare_action(action_index);
        match &self.actions[action_index] {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => match change {
                Change::Removed(e) => {
                    let filename = self.base.join(e.path());
                    if !e.is_dir() {
                        fs::remove_file(&filename)?;
                    }
                }
                Change::Added(e) => {
                    let filename = self.base.join(e.path());
                    if let Some(p) = e.target() {
                        std::os::unix::fs::symlink(p, &filename)?;
                        self.new_entries.push(update_meta(&filename, e)?);
                    } else if e.is_dir() {
                        fs::create_dir(&filename)?;
                    } else {
                        return Err(eyre!("missing file detail for {}", e.path().display()));
                    }
                }
                Change::Modified(e1, e2) => {
                    let filename = self.base.join(e2.path());
                    if e1.is_file() {
                        if e2.is_file() {
                            if !e1.same_contents(e2) {
                                return Err(eyre!(
                                    "missing diff detail for {}",
                                    e2.path().display()
                                ));
                            }
                            self.new_entries.push(update_meta(&filename, e2)?);
                        } else {
                            fs::remove_file(&filename)?;
                            if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &filename)?;
                                self.new_entries.push(update_meta(&filename, e2)?);
                            } else if e2.is_dir() {
                                fs::create_dir(&filename)?;
                            } else {
                                return Err(eyre!(
                                    "unsupported new entry for {}",
                                    e2.path().display()
                                ));
                            }
                        }
                    } else if e1.is_symlink() {
                        if e2.is_file() {
                            return Err(eyre!("missing file detail for {}", e2.path().display()));
                        }
                        fs::remove_file(&filename)?;
                        if let Some(p) = e2.target() {
                            std::os::unix::fs::symlink(p, &filename)?;
                            self.new_entries.push(update_meta(&filename, e2)?);
                        } else if e2.is_dir() {
                            fs::create_dir(&filename)?;
                        }
                    } else if e1.is_dir() {
                        if e2.is_file() {
                            return Err(eyre!(
                                "streaming directory-to-file changes is not supported"
                            ));
                        }
                    } else {
                        return Err(eyre!("unsupported old entry for {}", e1.path().display()));
                    }
                }
            },
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => match change {
                Change::Removed(_) => {}
                Change::Added(e) | Change::Modified(_, e) => self.new_entries.push(e.clone()),
            },
            Action::Identical(change, _) => match change {
                Change::Removed(_) => {}
                Change::Added(e) | Change::Modified(_, e) => self.new_entries.push(e.clone()),
            },
            Action::Conflict(_, _) => {}
        }
        Ok(())
    }

    fn begin_file_detail(&mut self, action_index: usize) -> Result<()> {
        self.prepare_action(action_index);
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        let output = TempOutput::new(filename)?;
        self.state = Some(ApplyState::File {
            action_index,
            output,
        });
        Ok(())
    }

    fn begin_diff_detail(&mut self, action_index: usize) -> Result<()> {
        self.prepare_action(action_index);
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        let source = fs::File::open(&filename)?;
        let output = TempOutput::new(filename)?;
        self.state = Some(ApplyState::Diff {
            action_index,
            source,
            output,
        });
        Ok(())
    }

    fn finish_file_detail(&mut self) -> Result<()> {
        let state = self
            .state
            .take()
            .ok_or_else(|| eyre!("no file detail in progress"))?;
        let (action_index, output) = match state {
            ApplyState::File {
                action_index,
                output,
            } => (action_index, output),
            ApplyState::Diff {
                action_index,
                output,
                ..
            } => (action_index, output),
        };
        output.finish()?;

        let entry = match &self.actions[action_index] {
            Action::Local(Change::Added(e))
            | Action::ResolvedLocal((_, _), Change::Added(e))
            | Action::Local(Change::Modified(_, e))
            | Action::ResolvedLocal((_, _), Change::Modified(_, e)) => e,
            _ => return Err(eyre!("file detail finished for non-file action")),
        };
        let filename = self.base.join(entry.path());
        self.new_entries.push(update_meta(&filename, entry)?);
        self.action_index = action_index + 1;
        Ok(())
    }

    fn apply_directory_second_pass(&mut self) -> Result<()> {
        for action in self.actions.iter().rev() {
            match action {
                Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                    if !change.is_dir() {
                        continue;
                    }
                    match change {
                        Change::Removed(e) => {
                            let dirname = self.base.join(e.path());
                            fs::remove_dir(&dirname)?;
                        }
                        Change::Added(e) => {
                            let dirname = self.base.join(e.path());
                            self.new_entries.push(update_meta(&dirname, e)?);
                        }
                        Change::Modified(e1, e2) => {
                            let dirname = self.base.join(e2.path());
                            if e1.is_dir() && !e2.is_dir() {
                                return Err(eyre!(
                                    "streaming directory-to-file changes is not supported"
                                ));
                            }
                            self.new_entries.push(update_meta(&dirname, e2)?);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

enum ApplyDetailKind {
    File,
    Diff,
}

fn apply_detail_kind(action: &Action) -> Option<ApplyDetailKind> {
    let change = match action {
        Action::Local(change) | Action::ResolvedLocal((_, _), change) => change,
        _ => return None,
    };

    match change {
        Change::Removed(_) => None,
        Change::Added(e) => e.is_file().then_some(ApplyDetailKind::File),
        Change::Modified(e1, e2) => {
            if e1.is_file() && e2.is_file() && !e1.same_contents(e2) {
                Some(ApplyDetailKind::Diff)
            } else if !e1.is_file() && e2.is_file() {
                Some(ApplyDetailKind::File)
            } else {
                None
            }
        }
    }
}

fn detail_filename(base: &Path, action: &Action) -> Result<PathBuf> {
    match action {
        Action::Local(Change::Added(e))
        | Action::ResolvedLocal((_, _), Change::Added(e))
        | Action::Local(Change::Modified(_, e))
        | Action::ResolvedLocal((_, _), Change::Modified(_, e)) => Ok(base.join(e.path())),
        _ => Err(eyre!("action has no detail filename")),
    }
}

fn copy_from_source(
    source: &mut fs::File,
    output: &mut fs::File,
    offset: u64,
    len: u64,
) -> Result<()> {
    source.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    let mut buf = [0; WINDOW];
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, buf.len());
        let n = source.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

pub fn apply_detailed_changes(
    base: &PathBuf,
    actions: &Vec<Action>,
    details: &Vec<ChangeDetails>,
    all_old: &mut Vec<Entry>,
) -> Result<()> {
    log::debug!("details.len() = {}", details.len());
    let mut details_iter = details.iter();
    let mut new_entries: Vec<Entry> = Vec::new();
    let mut old_iter = all_old.iter().peekable();
    let mut leftover_details: Vec<&ChangeDetails> = Vec::new();

    for action in actions {
        let path = action.path();
        loop {
            let oe = old_iter.peek();
            if let Some(e) = oe {
                match e.path().cmp(path) {
                    Ordering::Less => {
                        new_entries.push(old_iter.next().unwrap().clone());
                    }
                    Ordering::Equal => {
                        let e = old_iter.next().unwrap();
                        if action.is_unresolved_conflict() {
                            new_entries.push(e.clone()); // preserve the original
                        }
                        continue;
                    } // action will deal with this
                    Ordering::Greater => {
                        break;
                    }
                }
            } else {
                break;
            }
        }

        match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                log::debug!("applying detailed change to {}", action.path().display());
                match change {
                    Change::Removed(e) => {
                        let filename = base.join(e.path());
                        log::debug!("Removing {:?}", filename);
                        if !e.is_dir() {
                            fs::remove_file(&filename)
                                .expect(format!("failed to remove file {:?}", filename).as_str());
                        } // else: removing directory;
                          //   must happen after all the files have been removed, which will happen
                          //   in the second pass
                          // nothing gets copied into new_entries
                    }
                    Change::Added(e) => {
                        let filename = base.join(e.path());
                        if let Some(p) = e.target() {
                            std::os::unix::fs::symlink(p, &filename).expect(
                                format!("failed to create symlink {:?} {:?}", p, filename).as_str(),
                            );
                            new_entries.push(update_meta(&filename, e).expect(
                                format!("failed to update metadata for {:?}", filename).as_str(),
                            ));
                        } else if e.is_dir() {
                            fs::create_dir(&filename).expect(
                                format!("failed to create directory {:?}", filename).as_str(),
                            );
                            // new entry gets updated in the second pass, after all the updates in
                            // the directory are finished
                        } else {
                            log::debug!("Adding {}", e.path().display());
                            let detail = &details_iter.next().unwrap();
                            create_file(&filename, &detail)
                                .expect(format!("failed to create file {:?}", filename).as_str());
                            new_entries.push(update_meta(&filename, e).expect(
                                format!("failed to update metadata for {:?}", filename).as_str(),
                            ));
                        }
                    }
                    Change::Modified(e1, e2) => {
                        let filename = base.join(e2.path());
                        if e1.is_file() {
                            if e2.is_file() {
                                if !e1.same_contents(&e2) {
                                    let detail = &details_iter.next().unwrap();
                                    match detail {
                                        ChangeDetails::Diff(delta) => {
                                            let block = [0; WINDOW];
                                            let mut updated = Vec::new();
                                            restore_seek(
                                                &mut updated,
                                                fs::File::open(&filename)?,
                                                block,
                                                &delta,
                                            )?;
                                            create_file_with_contents(&filename, &updated)?;
                                        }
                                        _ => {
                                            return Err(eyre!(
                                            "mismatch when adding {}, expected Diff, but not found",
                                            e1.path().display()
                                        ))
                                        }
                                    }
                                }
                                new_entries.push(update_meta(&filename, e2)?);
                            } else {
                                // e2 not a file
                                // remove the file
                                fs::remove_file(&filename).expect(
                                    format!("failed to remove file {:?}", filename).as_str(),
                                );
                                if let Some(p) = e2.target() {
                                    std::os::unix::fs::symlink(p, &filename).expect(
                                        format!("failed to create symlink {:?} {:?}", p, filename)
                                            .as_str(),
                                    );
                                    new_entries.push(update_meta(&filename, e2)?);
                                } else if e2.is_dir() {
                                    fs::create_dir(&filename).expect(
                                        format!("failed to create directory {:?}", filename)
                                            .as_str(),
                                    );
                                } else {
                                    panic!("Exhausted possibilities for the new entry");
                                }
                            }
                        } else if e1.is_symlink() {
                            // remove the symlink
                            fs::remove_file(&filename)
                                .expect(format!("failed to remove file {:?}", filename).as_str());
                            if e2.is_file() {
                                let detail = &details_iter.next().unwrap();
                                create_file(&filename, &detail).expect(
                                    format!("failed to create file {:?}", filename).as_str(),
                                );
                                new_entries.push(update_meta(&filename, e2)?);
                            } else if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &filename).expect(
                                    format!("failed to create symlink {:?} {:?}", p, filename)
                                        .as_str(),
                                );
                                new_entries.push(update_meta(&filename, e2)?);
                            } else if e2.is_dir() {
                                fs::create_dir(&filename).expect(
                                    format!("failed to create directory {:?}", filename).as_str(),
                                );
                                // new entry gets updated in the second pass, after all the updates in
                                // the directory are finished
                            }
                        } else if e1.is_dir() {
                            if e2.is_file() {
                                // need to save the file contents for after we remove the directory
                                let detail = &details_iter.next().unwrap();
                                leftover_details.push(detail);
                            }
                        } else {
                            panic!("Exhausted possibilities for the old entry");
                        }
                    }
                }
            }
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => match change {
                Change::Removed(_) => {}
                Change::Added(e) => {
                    new_entries.push(e.clone());
                }
                Change::Modified(_, e) => {
                    new_entries.push(e.clone());
                }
            },
            Action::Identical(change, _) => match change {
                Change::Removed(_) => {}
                Change::Added(e) => {
                    new_entries.push(e.clone());
                }
                Change::Modified(_, e) => {
                    new_entries.push(e.clone());
                }
            },
            Action::Conflict(_, _) => {} // skip conflicts; only way we get here with them, if we are in the batch force mode
        }
    }

    // TODO: think how directory removal interacts with "ignore", if we ever implement it

    // second pass, in reverse order, to remove directories and update their metadata
    let mut details_iter = leftover_details.iter().rev();
    for action in actions.iter().rev() {
        match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                if !change.is_dir() {
                    continue;
                }
                match change {
                    Change::Removed(e) => {
                        let dirname = base.join(e.path());
                        fs::remove_dir(&dirname)
                            .expect(format!("failed to remove directory {:?}", dirname).as_str());
                    }
                    Change::Added(e) => {
                        let dirname = base.join(e.path());
                        new_entries.push(update_meta(&dirname, e)?);
                    }
                    Change::Modified(e1, e2) => {
                        let dirname = base.join(e2.path());
                        if e1.is_dir() && !e2.is_dir() {
                            fs::remove_dir(&dirname).expect(
                                format!("failed to remove directory {:?}", dirname).as_str(),
                            );
                            if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &dirname).expect(
                                    format!("failed to create symlink {:?} {:?}", p, dirname)
                                        .as_str(),
                                );
                            } else if e2.is_file() {
                                let detail = details_iter.next().unwrap();
                                create_file(&dirname, &detail).expect(
                                    format!("failed to create file {:?}", dirname).as_str(),
                                );
                            }
                        }
                        new_entries.push(update_meta(&dirname, e2)?);
                    }
                }
            }
            _ => {}
        }
    }

    // copy remaining entries from all_old
    for e in old_iter {
        new_entries.push(e.clone());
    }
    new_entries.sort(); // directory -> file or symlink will be out of order, so need to sort them

    std::mem::swap(all_old, &mut new_entries);

    Ok(())
}

fn create_file(filename: &Path, detail: &ChangeDetails) -> Result<()> {
    match detail {
        ChangeDetails::Contents(v) => create_file_with_contents(filename, v),
        _ => Err(eyre!(
            "mismatch when adding {}, expected Contents, but not found",
            filename.display()
        )),
    }
}

fn create_file_with_contents(filename: &Path, data: &Vec<u8>) -> Result<()> {
    use atomicwrites::{AllowOverwrite, AtomicFile};
    let af = AtomicFile::new(filename, AllowOverwrite);
    let result = af.write(|f| f.write_all(data));
    match result {
        Ok(()) => Ok(()),
        Err(e) => Err(eyre!("unable to save {}: {}", filename.display(), e)),
    }
}

fn update_meta(path: &PathBuf, e: &Entry) -> Result<Entry> {
    let meta = fs::symlink_metadata(path)
        .expect(format!("failed to acquire metadata for {:?}", path).as_str());
    if !e.is_symlink() {
        let mut perms = meta.permissions();
        perms.set_mode(e.mode());
        fs::set_permissions(path, perms)
            .expect(format!("failed to set permissions for {:?}", path).as_str());
    }
    filetime::set_symlink_file_times(
        path,
        filetime::FileTime::from_unix_time(meta.atime(), 0),
        filetime::FileTime::from_unix_time(e.mtime(), 0),
    )
    .expect(format!("failed to set time for {:?}", path).as_str());
    let mut new_entry = e.clone();
    new_entry.set_ino(meta.ino());
    Ok(new_entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngCore, SeedableRng};

    #[test]
    fn stream_diff_frames_coalesces_adjacent_copy_ops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let mut contents = vec![0; WINDOW * 8];
        rand::rngs::StdRng::seed_from_u64(1).fill_bytes(&mut contents);
        fs::write(&path, &contents).unwrap();

        let sig = signature(fs::File::open(&path).unwrap(), [0; WINDOW]).unwrap();
        let (sender, receiver) = mpsc::sync_channel(16);

        stream_diff_frames(path, 0, sig, 1024 * 1024, sender).unwrap();

        let frames = receiver
            .into_iter()
            .map(|frame| frame.unwrap())
            .collect::<Vec<_>>();

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].action_index, 0);
        assert!(matches!(
            frames[0].payload,
            DetailPayload::DiffCopy { offset: 0, len }
                if len >= contents.len() as u64 && len <= (contents.len() + WINDOW) as u64
        ));
        assert!(matches!(frames[1].payload, DetailPayload::DiffEnd));
    }

    #[test]
    fn detail_frames_transfer_bytes_counts_reconstructed_bytes() {
        let frames = vec![
            DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            },
            DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(vec![0; 7]),
            },
            DetailFrame {
                action_index: 1,
                payload: DetailPayload::DiffCopy { offset: 0, len: 11 },
            },
            DetailFrame {
                action_index: 1,
                payload: DetailPayload::DiffBytes(vec![0; 13]),
            },
            DetailFrame {
                action_index: 1,
                payload: DetailPayload::DiffEnd,
            },
        ];

        assert_eq!(detail_frames_transfer_bytes(&frames), 31);
    }
}
