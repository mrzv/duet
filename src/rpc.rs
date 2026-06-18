use std::collections::HashMap;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Instant;

use bincode::serde::encode_into_std_write as serialize_into;
use color_eyre::eyre::{eyre, Report, Result};
use essrpc::essrpc;
use essrpc::transports::{BincodeTransport, ReadWrite};
use essrpc::{RPCError, RPCErrorKind, RPCServer};
use serde::{Deserialize, Serialize};

use crate::actions::Actions;
use crate::performance::{duration_ms, RemoteStreamProfile};
use crate::profile;
use crate::scan::location::Locations;
use crate::state::{Changes, Entries};
use crate::sync::{
    self, ApplyStreamId, ChangeDetails, DetailFrame, DetailProducer, DetailStreamId,
    SignatureWithPath,
};
use crate::sync_error::{self, StructuredSyncError};

pub(crate) const SERVER_LOG_ENV: &str = "DUET_SERVER_LOG";
pub(crate) const PROTOCOL_VERSION: u32 = 2;
pub(crate) const CAPABILITY_PROFILE_FILE_STATE_DIR: &str = "profile-file-state-dir";
pub(crate) const CAPABILITY_STREAMED_DETAILS: &str = "streamed-details-v1";
pub(crate) const CAPABILITY_STREAMED_DETAIL_BATCHES: &str = "streamed-detail-batches-v1";
pub(crate) const CAPABILITY_APPLY_ATTEMPT_PREPARE: &str = "apply-attempt-prepare-v1";
pub(crate) const CAPABILITY_APPLY_ATTEMPT_ID: &str = "apply-attempt-id-v1";
pub(crate) const CAPABILITY_CREATABLE_ADDED_PARENTS: &str = "creatable-added-parents-v1";
pub(crate) const CAPABILITY_SYNC_TUNING: &str = "sync-tuning-v1";
pub(crate) const CAPABILITY_STREAM_PERFORMANCE: &str = "stream-performance-v1";
pub(crate) const CAPABILITY_FILE_BYTE_CHUNKS: &str = "file-byte-chunks-v1";
const CLIENT_CAPABILITIES: &[&str] = &[
    CAPABILITY_PROFILE_FILE_STATE_DIR,
    CAPABILITY_STREAMED_DETAILS,
    CAPABILITY_STREAMED_DETAIL_BATCHES,
    CAPABILITY_APPLY_ATTEMPT_PREPARE,
    CAPABILITY_APPLY_ATTEMPT_ID,
    CAPABILITY_CREATABLE_ADDED_PARENTS,
    CAPABILITY_SYNC_TUNING,
    CAPABILITY_STREAM_PERFORMANCE,
    CAPABILITY_FILE_BYTE_CHUNKS,
];

pub(crate) fn client_capabilities() -> &'static [&'static str] {
    CLIENT_CAPABILITIES
}

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub protocol_version: u32,
    pub duet_version: String,
    pub capabilities: Vec<String>,
}

#[essrpc(sync, async)]
pub trait DuetServer {
    // This trait is the wire protocol. To preserve compatibility with older
    // servers, only append methods; never reorder, remove, or change signatures.
    fn set_base(&mut self, base: String) -> Result<(), RPCError>;
    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError>;
    fn changes(
        &mut self,
        path: PathBuf,
        locations: Locations,
        ignore: profile::Ignore,
        remote_id: String,
    ) -> Result<Changes, RPCError>;
    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError>;
    fn get_detailed_changes(
        &self,
        signatures: Vec<SignatureWithPath>,
    ) -> Result<Vec<sync::ChangeDetails>, RPCError>;
    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError>;
    fn save_state(&self) -> Result<(), RPCError>;
    fn set_remote_state_dir(&mut self, remote_state_dir: PathBuf) -> Result<(), RPCError>;
    fn server_info(&self) -> Result<ServerInfo, RPCError>;
    fn begin_detail_stream(
        &mut self,
        signatures: Vec<SignatureWithPath>,
        max_chunk_bytes: u32,
    ) -> Result<DetailStreamId, RPCError>;
    fn next_detail_chunk(
        &mut self,
        stream_id: DetailStreamId,
    ) -> Result<Option<DetailFrame>, RPCError>;
    fn end_detail_stream(&mut self, stream_id: DetailStreamId) -> Result<(), RPCError>;
    fn begin_apply_stream(&mut self) -> Result<ApplyStreamId, RPCError>;
    fn apply_detail_chunk(
        &mut self,
        stream_id: ApplyStreamId,
        frame: DetailFrame,
    ) -> Result<(), RPCError>;
    fn finish_apply_stream(&mut self, stream_id: ApplyStreamId) -> Result<(), RPCError>;
    fn next_detail_chunks(
        &mut self,
        stream_id: DetailStreamId,
        max_frames: u32,
        max_payload_bytes: u32,
    ) -> Result<Vec<DetailFrame>, RPCError>;
    fn apply_detail_chunks(
        &mut self,
        stream_id: ApplyStreamId,
        frames: Vec<DetailFrame>,
    ) -> Result<(), RPCError>;
    fn prepare_apply_attempt(&mut self) -> Result<(), RPCError>;
    fn prepare_apply_attempt_with_id(&mut self, attempt_id: String) -> Result<(), RPCError>;
    fn negotiate_sync_tuning(
        &mut self,
        request: sync::SyncTuningRequest,
    ) -> Result<sync::SyncTuning, RPCError>;
    fn stream_performance(&self) -> Result<RemoteStreamProfile, RPCError>;
    fn apply_file_byte_chunk(
        &mut self,
        stream_id: ApplyStreamId,
        chunk: sync::FileByteChunk,
    ) -> Result<(), RPCError>;
}

struct DuetServerImpl {
    base: PathBuf,
    remote_id: String,
    remote_state_dir: PathBuf,
    all_old: Entries,
    actions: Actions,
    apply_attempt_id: Option<String>,
    detail_streams: HashMap<DetailStreamId, DetailProducer>,
    apply_streams: HashMap<ApplyStreamId, sync::DetailApplier>,
    next_stream_id: u64,
    tuning: sync::SyncTuning,
    stream_performance: RemoteStreamProfile,
}

impl DuetServerImpl {
    fn new() -> Result<Self> {
        Ok(DuetServerImpl {
            base: PathBuf::from(""),
            remote_id: "".to_string(),
            remote_state_dir: profile::remote_state_dir()?,
            all_old: Vec::new(),
            actions: Vec::new(),
            apply_attempt_id: None,
            detail_streams: HashMap::new(),
            apply_streams: HashMap::new(),
            next_stream_id: 1,
            tuning: sync::SyncTuning::legacy(),
            stream_performance: RemoteStreamProfile::default(),
        })
    }

    fn next_detail_stream_id(&mut self) -> DetailStreamId {
        let id = DetailStreamId(self.next_stream_id);
        self.next_stream_id += 1;
        id
    }

    fn next_apply_stream_id(&mut self) -> ApplyStreamId {
        let id = ApplyStreamId(self.next_stream_id);
        self.next_stream_id += 1;
        id
    }

    fn apply_attempt_id(&self) -> Option<&str> {
        self.apply_attempt_id.as_deref()
    }
}

fn rpc_error(operation: &str, path: Option<&Path>, error: impl std::fmt::Debug) -> RPCError {
    RPCError::new(
        RPCErrorKind::Other,
        StructuredSyncError::remote(operation, path.map(Path::to_path_buf), error).to_string(),
    )
}

fn rpc_report_error(operation: &str, path: Option<&Path>, error: Report) -> RPCError {
    RPCError::new(
        RPCErrorKind::Other,
        StructuredSyncError::from_report("remote", operation, path.map(Path::to_path_buf), error)
            .to_string(),
    )
}

fn clamp_rpc_limit(requested: u32, max: usize) -> usize {
    requested.clamp(1, max.min(u32::MAX as usize) as u32) as usize
}

impl DuetServer for DuetServerImpl {
    fn set_base(&mut self, base: String) -> Result<(), RPCError> {
        self.base = match crate::full(&base) {
            Ok(s) => s,
            Err(e) => return Err(rpc_report_error("set base", Some(Path::new(&base)), e)),
        };
        log::debug!("Set base {}", self.base.display());
        Ok(())
    }

    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError> {
        log::debug!("Setting {} actions", actions.len());
        self.actions = actions;
        self.stream_performance = RemoteStreamProfile::default();
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::preflight_state_save(&remote_state)
            .map_err(|e| rpc_report_error("preflight state save", Some(&remote_state), e))?;
        sync::preflight_apply(&self.base, &self.actions)
            .map_err(|e| rpc_report_error("preflight apply", Some(&self.base), e))?;
        Ok(())
    }

    fn changes(
        &mut self,
        path: PathBuf,
        locations: Locations,
        ignore: profile::Ignore,
        remote_id: String,
    ) -> Result<Changes, RPCError> {
        log::debug!("remote id = {}", remote_id);
        self.remote_id = remote_id;
        self.apply_attempt_id = None;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::check_apply_attempt_clear(&remote_state)
            .map_err(|e| rpc_report_error("check apply recovery", Some(&remote_state), e))?;

        let handle = tokio::runtime::Handle::current();
        let result = handle.block_on(async {
            crate::state::old_and_changes(
                &self.base,
                &path,
                &locations,
                &ignore,
                Some(&remote_state),
            )
            .await
        });

        match result {
            Ok((all_old, changes)) => {
                self.all_old = all_old;
                Ok(changes)
            }
            Err(e) => Err(rpc_report_error(
                "scan changes",
                Some(&self.base.join(path)),
                e,
            )),
        }
    }

    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError> {
        log::debug!("Getting signatures");
        let result = sync::get_signatures_with_config(
            &self.base,
            &self.actions,
            self.tuning.signature_window_config(),
        );
        match result {
            Ok(signatures) => Ok(signatures),
            Err(e) => Err(rpc_report_error("read signatures", Some(&self.base), e)),
        }
    }

    fn get_detailed_changes(
        &self,
        signatures: Vec<SignatureWithPath>,
    ) -> Result<Vec<sync::ChangeDetails>, RPCError> {
        log::debug!(
            "Getting detailed changes for {} signatures",
            signatures.len()
        );
        let result = sync::get_detailed_changes(&self.base, &self.actions, &signatures);
        match result {
            Ok(details) => Ok(details),
            Err(e) => Err(rpc_report_error(
                "read detailed changes",
                Some(&self.base),
                e,
            )),
        }
    }

    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError> {
        log::debug!("Appling detailed changes, with {} details", details.len());
        sync::preflight_apply(&self.base, &self.actions)
            .map_err(|e| rpc_report_error("preflight apply details", Some(&self.base), e))?;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::start_apply_attempt(
            "remote",
            &remote_state,
            &self.base,
            &self.actions,
            self.apply_attempt_id(),
        )
        .map_err(|e| rpc_report_error("start apply recovery", Some(&remote_state), e))?;
        let result = sync::apply_detailed_changes(
            &self.base,
            &self.actions,
            &details,
            &mut self.all_old,
            Some(&remote_state),
        );
        match result {
            Ok(()) => {
                sync::mark_apply_attempt_state_save(
                    "remote",
                    &remote_state,
                    &self.base,
                    &self.actions,
                    self.apply_attempt_id(),
                )
                .map_err(|e| {
                    rpc_report_error("mark apply recovery state-save", Some(&remote_state), e)
                })?;
                Ok(())
            }
            Err(e) => Err(rpc_report_error("apply details", Some(&self.base), e)),
        }
    }

    fn save_state(&self) -> Result<(), RPCError> {
        log::debug!("Saving state");
        std::fs::create_dir_all(&self.remote_state_dir).map_err(|e| {
            rpc_error(
                "create remote state directory",
                Some(&self.remote_state_dir),
                e,
            )
        })?;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        log::info!(
            "Saving remote state {} with {} entries",
            remote_state.display(),
            &self.all_old.len()
        );
        use atomicwrites::{AllowOverwrite, AtomicFile};
        let af = AtomicFile::new(&remote_state, AllowOverwrite);
        let result = af.write(|f| {
            use std::io::{self, Write};
            let mut f = BufWriter::new(f);
            serialize_into(&self.all_old, &mut f, bincode::config::legacy())
                .map_err(io::Error::other)?;
            f.flush()?;
            Ok::<(), io::Error>(())
        });
        match result {
            Ok(_) => {
                sync::finish_apply_attempt(&remote_state).map_err(|e| {
                    rpc_report_error("finish apply recovery", Some(&remote_state), e)
                })?;
                Ok(())
            }
            Err(e) => Err(rpc_error("save remote state", Some(&remote_state), e)),
        }
    }

    fn set_remote_state_dir(&mut self, remote_state_dir: PathBuf) -> Result<(), RPCError> {
        log::debug!("Set remote state dir {}", remote_state_dir.display());
        self.remote_state_dir = remote_state_dir;
        Ok(())
    }

    fn server_info(&self) -> Result<ServerInfo, RPCError> {
        Ok(ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            duet_version: built_info::PKG_VERSION.to_string(),
            capabilities: client_capabilities()
                .iter()
                .map(|c| c.to_string())
                .collect(),
        })
    }

    fn begin_detail_stream(
        &mut self,
        signatures: Vec<SignatureWithPath>,
        max_chunk_bytes: u32,
    ) -> Result<DetailStreamId, RPCError> {
        let id = self.next_detail_stream_id();
        let max_chunk_bytes = clamp_rpc_limit(max_chunk_bytes, self.tuning.detail_chunk_bytes());
        let producer = sync::DetailProducer::new(
            self.base.clone(),
            self.actions.clone(),
            signatures,
            max_chunk_bytes,
        );
        self.detail_streams.insert(id, producer);
        Ok(id)
    }

    fn next_detail_chunk(
        &mut self,
        stream_id: DetailStreamId,
    ) -> Result<Option<DetailFrame>, RPCError> {
        let producer = self
            .detail_streams
            .get_mut(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "detail stream does not exist"))?;
        match producer.next_frame() {
            Ok(frame) => {
                if frame.is_none() {
                    self.detail_streams.remove(&stream_id);
                }
                Ok(frame)
            }
            Err(e) => Err(rpc_report_error("read detail stream", Some(&self.base), e)),
        }
    }

    fn end_detail_stream(&mut self, stream_id: DetailStreamId) -> Result<(), RPCError> {
        self.detail_streams.remove(&stream_id);
        Ok(())
    }

    fn begin_apply_stream(&mut self) -> Result<ApplyStreamId, RPCError> {
        sync::preflight_apply(&self.base, &self.actions)
            .map_err(|e| rpc_report_error("preflight apply stream", Some(&self.base), e))?;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::start_apply_attempt(
            "remote",
            &remote_state,
            &self.base,
            &self.actions,
            self.apply_attempt_id(),
        )
        .map_err(|e| rpc_report_error("start apply recovery", Some(&remote_state), e))?;
        let id = self.next_apply_stream_id();
        let applier = sync::DetailApplier::new_with_attempt(
            self.base.clone(),
            self.actions.clone(),
            self.all_old.clone(),
            Some(remote_state.clone()),
        );
        self.apply_streams.insert(id, applier);
        Ok(id)
    }

    fn apply_detail_chunk(
        &mut self,
        stream_id: ApplyStreamId,
        frame: DetailFrame,
    ) -> Result<(), RPCError> {
        let base = self.base.clone();
        let applier = self
            .apply_streams
            .get_mut(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "apply stream does not exist"))?;
        applier
            .apply_frame(frame)
            .map_err(|e| rpc_report_error("apply detail stream", Some(&base), e))
    }

    fn finish_apply_stream(&mut self, stream_id: ApplyStreamId) -> Result<(), RPCError> {
        let start = Instant::now();
        let applier = self
            .apply_streams
            .remove(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "apply stream does not exist"))?;
        self.all_old = applier
            .finish()
            .map_err(|e| rpc_report_error("finish apply stream", Some(&self.base), e))?;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::mark_apply_attempt_state_save(
            "remote",
            &remote_state,
            &self.base,
            &self.actions,
            self.apply_attempt_id(),
        )
        .map_err(|e| rpc_report_error("mark apply recovery state-save", Some(&remote_state), e))?;
        self.stream_performance.apply_finish_ms += duration_ms(start.elapsed());
        Ok(())
    }

    fn next_detail_chunks(
        &mut self,
        stream_id: DetailStreamId,
        max_frames: u32,
        max_payload_bytes: u32,
    ) -> Result<Vec<DetailFrame>, RPCError> {
        let start = Instant::now();
        let max_frames = clamp_rpc_limit(max_frames, self.tuning.detail_batch_frames());
        let max_payload_bytes =
            clamp_rpc_limit(max_payload_bytes, self.tuning.detail_batch_payload_bytes());
        let result = {
            let producer = self.detail_streams.get_mut(&stream_id).ok_or_else(|| {
                RPCError::new(RPCErrorKind::Other, "detail stream does not exist")
            })?;
            producer.next_frames(max_frames, max_payload_bytes)
        };
        match result {
            Ok(frames) => {
                self.stream_performance.detail_generate_ms += duration_ms(start.elapsed());
                self.stream_performance.detail_batches += 1;
                self.stream_performance
                    .detail_transfer
                    .record_batch(&frames);
                if frames.is_empty() {
                    self.detail_streams.remove(&stream_id);
                }
                Ok(frames)
            }
            Err(e) => Err(rpc_report_error("read detail stream", Some(&self.base), e)),
        }
    }

    fn apply_detail_chunks(
        &mut self,
        stream_id: ApplyStreamId,
        frames: Vec<DetailFrame>,
    ) -> Result<(), RPCError> {
        let base = self.base.clone();
        self.stream_performance.apply_batches += 1;
        self.stream_performance.apply_transfer.record_batch(&frames);
        let start = Instant::now();
        let applier = self
            .apply_streams
            .get_mut(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "apply stream does not exist"))?;
        for frame in frames {
            applier
                .apply_frame(frame)
                .map_err(|e| rpc_report_error("apply detail stream", Some(&base), e))?;
        }
        self.stream_performance.apply_frames_ms += duration_ms(start.elapsed());
        Ok(())
    }

    fn prepare_apply_attempt(&mut self) -> Result<(), RPCError> {
        self.apply_attempt_id = None;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::start_apply_attempt("remote", &remote_state, &self.base, &self.actions, None)
            .map_err(|e| rpc_report_error("prepare apply recovery", Some(&remote_state), e))
    }

    fn prepare_apply_attempt_with_id(&mut self, attempt_id: String) -> Result<(), RPCError> {
        if attempt_id.is_empty() {
            return Err(rpc_error(
                "prepare apply recovery",
                None,
                "apply attempt id is empty",
            ));
        }
        self.apply_attempt_id = Some(attempt_id);
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::start_apply_attempt(
            "remote",
            &remote_state,
            &self.base,
            &self.actions,
            self.apply_attempt_id(),
        )
        .map_err(|e| rpc_report_error("prepare apply recovery", Some(&remote_state), e))
    }

    fn negotiate_sync_tuning(
        &mut self,
        request: sync::SyncTuningRequest,
    ) -> Result<sync::SyncTuning, RPCError> {
        let tuning = sync::SyncTuning::preferred_with_env().negotiate(request.preferred);
        self.tuning = tuning;
        Ok(tuning)
    }

    fn stream_performance(&self) -> Result<RemoteStreamProfile, RPCError> {
        Ok(self.stream_performance.clone())
    }

    fn apply_file_byte_chunk(
        &mut self,
        stream_id: ApplyStreamId,
        chunk: sync::FileByteChunk,
    ) -> Result<(), RPCError> {
        let base = self.base.clone();
        self.stream_performance.apply_batches += 1;
        self.stream_performance
            .apply_transfer
            .record_file_byte_chunk(chunk.len() as u64);
        let start = Instant::now();
        let applier = self
            .apply_streams
            .get_mut(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "apply stream does not exist"))?;
        applier
            .apply_file_byte_chunk(chunk)
            .map_err(|e| rpc_report_error("apply file byte stream", Some(&base), e))?;
        self.stream_performance.apply_frames_ms += duration_ms(start.elapsed());
        Ok(())
    }
}

pub async fn server() -> Result<()> {
    let log_path = if let Some(path) = std::env::var_os(SERVER_LOG_ENV) {
        PathBuf::from(path)
    } else {
        let default_log = "~/.config/duet/remote.log".to_string();
        crate::full(&default_log).map_err(|e| {
            eyre!(
                "{}",
                sync_error::render_report(
                    "setup",
                    "resolve remote server log",
                    Some(PathBuf::from(default_log)),
                    e,
                )
            )
        })?
    };
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            eyre!(
                "{}",
                sync_error::render_error(
                    "setup",
                    "create remote server log directory",
                    Some(parent.to_path_buf()),
                    e,
                )
            )
        })?;
    }
    use log::LevelFilter;
    simple_logging::log_to_file(&log_path, LevelFilter::Debug).map_err(|e| {
        eyre!(
            "{}",
            sync_error::render_error("setup", "open remote server log", Some(log_path.clone()), e,)
        )
    })?;

    let stdin = io::stdin();
    let stdout = io::stdout();

    let stdio = ReadWrite::new(stdin, stdout);

    log::debug!("in server()");

    let server_impl = DuetServerImpl::new().map_err(|e| {
        eyre!(
            "{}",
            sync_error::render_report("setup", "initialize remote server", None, e)
        )
    })?;

    tokio::task::spawn_blocking(move || {
        let mut serve = DuetServerRPCServer::new(server_impl, BincodeTransport::new(stdio));
        if let Err(e) = serve.serve() {
            if e.kind != RPCErrorKind::TransportEOF {
                log::error!("RPC server stopped with error: {:?}", e);
            }
        }
    })
    .await
    .map_err(|e| {
        eyre!(
            "{}",
            sync_error::render_error("setup", "run remote server task", None, e)
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Action;
    use essrpc::RPCClient;
    use std::sync::{Arc, Mutex};

    struct RecordingTransport {
        calls: Arc<Mutex<Vec<(&'static str, u32)>>>,
    }

    impl RecordingTransport {
        fn new(calls: Arc<Mutex<Vec<(&'static str, u32)>>>) -> Self {
            Self { calls }
        }
    }

    impl essrpc::ClientTransport for RecordingTransport {
        type TXState = ();
        type FinalState = ();

        fn tx_begin_call(
            &mut self,
            method: essrpc::MethodId,
        ) -> std::result::Result<Self::TXState, RPCError> {
            self.calls.lock().unwrap().push((method.name, method.num));
            Ok(())
        }

        fn tx_add_param(
            &mut self,
            _name: &'static str,
            _value: impl serde::Serialize,
            _state: &mut Self::TXState,
        ) -> std::result::Result<(), RPCError> {
            Ok(())
        }

        fn tx_finalize(
            &mut self,
            _state: Self::TXState,
        ) -> std::result::Result<Self::FinalState, RPCError> {
            Ok(())
        }

        fn rx_response<T>(&mut self, _state: Self::FinalState) -> std::result::Result<T, RPCError>
        where
            for<'de> T: serde::Deserialize<'de>,
        {
            Err(RPCError::new(RPCErrorKind::Other, "recorded call"))
        }
    }

    #[test]
    fn generated_rpc_method_ids_append_new_methods() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut client = DuetServerRPCClient::new(RecordingTransport::new(calls.clone()));

        assert!(client.prepare_apply_attempt().is_err());
        assert!(client
            .prepare_apply_attempt_with_id("attempt".to_string())
            .is_err());
        assert!(client
            .negotiate_sync_tuning(sync::SyncTuningRequest::preferred())
            .is_err());
        assert!(client.stream_performance().is_err());
        assert!(client
            .apply_file_byte_chunk(ApplyStreamId(1), sync::FileByteChunk::new(0, Vec::new()))
            .is_err());

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                ("prepare_apply_attempt", 17),
                ("prepare_apply_attempt_with_id", 18),
                ("negotiate_sync_tuning", 19),
                ("stream_performance", 20),
                ("apply_file_byte_chunk", 21),
            ]
        );
    }

    #[test]
    fn rpc_size_limits_are_clamped() {
        assert_eq!(clamp_rpc_limit(0, 1024), 1);
        assert_eq!(clamp_rpc_limit(512, 1024), 512);
        assert_eq!(clamp_rpc_limit(u32::MAX, 1024), 1024);
    }

    #[test]
    fn server_info_advertises_protocol_and_capabilities() {
        let info = DuetServerImpl::new().unwrap().server_info().unwrap();

        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert_eq!(info.duet_version, built_info::PKG_VERSION);
        assert_eq!(
            info.capabilities,
            vec![
                CAPABILITY_PROFILE_FILE_STATE_DIR.to_string(),
                CAPABILITY_STREAMED_DETAILS.to_string(),
                CAPABILITY_STREAMED_DETAIL_BATCHES.to_string(),
                CAPABILITY_APPLY_ATTEMPT_PREPARE.to_string(),
                CAPABILITY_APPLY_ATTEMPT_ID.to_string(),
                CAPABILITY_CREATABLE_ADDED_PARENTS.to_string(),
                CAPABILITY_SYNC_TUNING.to_string(),
                CAPABILITY_STREAM_PERFORMANCE.to_string(),
                CAPABILITY_FILE_BYTE_CHUNKS.to_string()
            ]
        );
    }

    #[test]
    fn negotiate_sync_tuning_stores_clamped_intersection() {
        let mut server = DuetServerImpl::new().unwrap();
        let request = sync::SyncTuningRequest {
            preferred: sync::SyncTuning {
                signature_window_min: 4096,
                signature_window_max: 8 * 1024 * 1024,
                detail_chunk_bytes: 128 * 1024 * 1024,
                detail_batch_frames: 512,
                detail_batch_payload_bytes: 128 * 1024 * 1024,
            },
        };

        let tuning = server.negotiate_sync_tuning(request).unwrap();

        assert_eq!(tuning.signature_window_min, 4096);
        assert_eq!(
            tuning.signature_window_max,
            sync::DEFAULT_SIGNATURE_WINDOW_MAX as u32
        );
        assert_eq!(
            tuning.detail_chunk_bytes,
            sync::DEFAULT_DETAIL_CHUNK_BYTES as u32
        );
        assert_eq!(
            tuning.detail_batch_frames,
            sync::DEFAULT_DETAIL_BATCH_FRAMES as u32
        );
        assert_eq!(
            tuning.detail_batch_payload_bytes,
            sync::DEFAULT_DETAIL_BATCH_PAYLOAD_BYTES as u32
        );
        assert_eq!(server.tuning, tuning);
    }

    #[test]
    fn prepare_apply_attempt_with_id_writes_marker_id() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir(&base).unwrap();

        let mut server = DuetServerImpl::new().unwrap();
        server.base = base;
        server.remote_id = "remote-peer".to_string();
        server.remote_state_dir = dir.path().join("state");
        server.actions = Vec::<Action>::new();

        server
            .prepare_apply_attempt_with_id("attempt-1".to_string())
            .unwrap();
        let remote_state = profile::remote_state_in(&server.remote_state_dir, &server.remote_id);
        let marker = remote_state.with_file_name(format!(
            ".{}.duet-apply",
            remote_state.file_name().unwrap().to_string_lossy()
        ));
        let marker_contents = std::fs::read_to_string(&marker).unwrap();
        assert!(marker_contents.contains("attempt-id: attempt-1"));

        server.begin_apply_stream().unwrap();
    }
}
