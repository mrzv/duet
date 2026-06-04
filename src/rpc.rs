use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use bincode::serde::encode_into_std_write as serialize_into;
use color_eyre::eyre::{Result, WrapErr};
use essrpc::essrpc;
use essrpc::transports::{BincodeTransport, ReadWrite};
use essrpc::{RPCError, RPCErrorKind, RPCServer};
use serde::{Deserialize, Serialize};

use crate::actions::Actions;
use crate::profile;
use crate::scan::location::Locations;
use crate::state::{Changes, Entries};
use crate::sync::{
    self, ApplyStreamId, ChangeDetails, DetailFrame, DetailProducer, DetailStreamId,
    SignatureWithPath,
};

pub(crate) const SERVER_LOG_ENV: &str = "DUET_SERVER_LOG";
pub(crate) const PROTOCOL_VERSION: u32 = 2;
pub(crate) const CAPABILITY_PROFILE_FILE_STATE_DIR: &str = "profile-file-state-dir";
pub(crate) const CAPABILITY_STREAMED_DETAILS: &str = "streamed-details-v1";
pub(crate) const CAPABILITY_STREAMED_DETAIL_BATCHES: &str = "streamed-detail-batches-v1";
const CLIENT_CAPABILITIES: &[&str] = &[
    CAPABILITY_PROFILE_FILE_STATE_DIR,
    CAPABILITY_STREAMED_DETAILS,
    CAPABILITY_STREAMED_DETAIL_BATCHES,
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
}

struct DuetServerImpl {
    base: PathBuf,
    remote_id: String,
    remote_state_dir: PathBuf,
    all_old: Entries,
    actions: Actions,
    detail_streams: HashMap<DetailStreamId, DetailProducer>,
    apply_streams: HashMap<ApplyStreamId, sync::DetailApplier>,
    next_stream_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSyncError {
    pub version: u8,
    pub side: String,
    pub operation: String,
    pub path: Option<PathBuf>,
    pub kind: String,
    pub message: String,
}

impl RemoteSyncError {
    fn new(operation: &str, path: Option<PathBuf>, error: impl fmt::Debug) -> Self {
        let message = format!("{:?}", error);
        Self {
            version: 1,
            side: "remote".to_string(),
            operation: operation.to_string(),
            path,
            kind: classify_error_message(&message).to_string(),
            message,
        }
    }
}

impl fmt::Display for RemoteSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "duet-sync-error-v{}", self.version)?;
        writeln!(f, "side: {}", self.side)?;
        writeln!(f, "operation: {}", self.operation)?;
        if let Some(path) = &self.path {
            writeln!(f, "path: {}", path.display())?;
        }
        writeln!(f, "kind: {}", self.kind)?;
        write!(f, "message: {}", self.message)
    }
}

fn classify_error_message(message: &str) -> &'static str {
    let message = message.to_lowercase();
    if message.contains("permission denied")
        || message.contains("permissiondenied")
        || message.contains("os error 13")
    {
        "permission_denied"
    } else if message.contains("no such file or directory") || message.contains("os error 2") {
        "not_found"
    } else if message.contains("not a directory") || message.contains("os error 20") {
        "not_directory"
    } else {
        "other"
    }
}

impl DuetServerImpl {
    fn new() -> Result<Self> {
        Ok(DuetServerImpl {
            base: PathBuf::from(""),
            remote_id: "".to_string(),
            remote_state_dir: profile::remote_state_dir()?,
            all_old: Vec::new(),
            actions: Vec::new(),
            detail_streams: HashMap::new(),
            apply_streams: HashMap::new(),
            next_stream_id: 1,
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
}

fn rpc_error(operation: &str, path: Option<&Path>, error: impl fmt::Debug) -> RPCError {
    RPCError::new(
        RPCErrorKind::Other,
        RemoteSyncError::new(operation, path.map(Path::to_path_buf), error).to_string(),
    )
}

impl DuetServer for DuetServerImpl {
    fn set_base(&mut self, base: String) -> Result<(), RPCError> {
        self.base = match crate::full(&base) {
            Ok(s) => s,
            Err(e) => return Err(rpc_error("set base", Some(Path::new(&base)), e)),
        };
        log::debug!("Set base {}", self.base.display());
        Ok(())
    }

    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError> {
        log::debug!("Setting {} actions", actions.len());
        self.actions = actions;
        let remote_state = profile::remote_state_in(&self.remote_state_dir, &self.remote_id);
        sync::preflight_state_save(&remote_state)
            .map_err(|e| rpc_error("preflight state save", Some(&remote_state), e))?;
        sync::preflight_apply(&self.base, &self.actions)
            .map_err(|e| rpc_error("preflight apply", Some(&self.base), e))?;
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

        let handle = tokio::runtime::Handle::current();
        let result = handle.block_on(async {
            crate::state::old_and_changes(
                &self.base,
                &path,
                &locations,
                &ignore,
                Some(&profile::remote_state_in(
                    &self.remote_state_dir,
                    &self.remote_id,
                )),
            )
            .await
        });

        match result {
            Ok((all_old, changes)) => {
                self.all_old = all_old;
                Ok(changes)
            }
            Err(e) => Err(rpc_error("scan changes", Some(&self.base.join(path)), e)),
        }
    }

    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError> {
        log::debug!("Getting signatures");
        let result = sync::get_signatures(&self.base, &self.actions);
        match result {
            Ok(signatures) => Ok(signatures),
            Err(e) => Err(rpc_error("read signatures", Some(&self.base), e)),
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
            Err(e) => Err(rpc_error("read detailed changes", Some(&self.base), e)),
        }
    }

    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError> {
        log::debug!("Appling detailed changes, with {} details", details.len());
        sync::preflight_apply(&self.base, &self.actions)
            .map_err(|e| rpc_error("preflight apply details", Some(&self.base), e))?;
        let result =
            sync::apply_detailed_changes(&self.base, &self.actions, &details, &mut self.all_old);
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(rpc_error("apply details", Some(&self.base), e)),
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
            let mut f = BufWriter::new(f);
            serialize_into(&self.all_old, &mut f, bincode::config::legacy())
        });
        match result {
            Ok(_) => Ok(()),
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
        let producer = sync::DetailProducer::new(
            self.base.clone(),
            self.actions.clone(),
            signatures,
            max_chunk_bytes as usize,
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
            Err(e) => Err(rpc_error("read detail stream", Some(&self.base), e)),
        }
    }

    fn end_detail_stream(&mut self, stream_id: DetailStreamId) -> Result<(), RPCError> {
        self.detail_streams.remove(&stream_id);
        Ok(())
    }

    fn begin_apply_stream(&mut self) -> Result<ApplyStreamId, RPCError> {
        sync::preflight_apply(&self.base, &self.actions)
            .map_err(|e| rpc_error("preflight apply stream", Some(&self.base), e))?;
        let id = self.next_apply_stream_id();
        let applier = sync::DetailApplier::new(
            self.base.clone(),
            self.actions.clone(),
            self.all_old.clone(),
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
            .map_err(|e| rpc_error("apply detail stream", Some(&base), e))
    }

    fn finish_apply_stream(&mut self, stream_id: ApplyStreamId) -> Result<(), RPCError> {
        let applier = self
            .apply_streams
            .remove(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "apply stream does not exist"))?;
        self.all_old = applier
            .finish()
            .map_err(|e| rpc_error("finish apply stream", Some(&self.base), e))?;
        Ok(())
    }

    fn next_detail_chunks(
        &mut self,
        stream_id: DetailStreamId,
        max_frames: u32,
        max_payload_bytes: u32,
    ) -> Result<Vec<DetailFrame>, RPCError> {
        let producer = self
            .detail_streams
            .get_mut(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "detail stream does not exist"))?;
        match producer.next_frames(max_frames as usize, max_payload_bytes as usize) {
            Ok(frames) => {
                if frames.is_empty() {
                    self.detail_streams.remove(&stream_id);
                }
                Ok(frames)
            }
            Err(e) => Err(rpc_error("read detail stream", Some(&self.base), e)),
        }
    }

    fn apply_detail_chunks(
        &mut self,
        stream_id: ApplyStreamId,
        frames: Vec<DetailFrame>,
    ) -> Result<(), RPCError> {
        let base = self.base.clone();
        let applier = self
            .apply_streams
            .get_mut(&stream_id)
            .ok_or_else(|| RPCError::new(RPCErrorKind::Other, "apply stream does not exist"))?;
        for frame in frames {
            applier
                .apply_frame(frame)
                .map_err(|e| rpc_error("apply detail stream", Some(&base), e))?;
        }
        Ok(())
    }
}

pub async fn server() -> Result<()> {
    let log_path = if let Some(path) = std::env::var_os(SERVER_LOG_ENV) {
        PathBuf::from(path)
    } else {
        crate::full(&"~/.config/duet/remote.log".to_string())?
    };
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).wrap_err_with(|| {
            format!(
                "unable to create remote server log directory {}",
                parent.display()
            )
        })?;
    }
    use log::LevelFilter;
    simple_logging::log_to_file(&log_path, LevelFilter::Debug)
        .wrap_err_with(|| format!("unable to open remote server log {}", log_path.display()))?;

    let stdin = io::stdin();
    let stdout = io::stdout();

    let stdio = ReadWrite::new(stdin, stdout);

    log::debug!("in server()");

    let server_impl = DuetServerImpl::new()?;

    tokio::task::spawn_blocking(move || {
        let mut serve = DuetServerRPCServer::new(server_impl, BincodeTransport::new(stdio));
        if let Err(e) = serve.serve() {
            if e.kind != RPCErrorKind::TransportEOF {
                log::error!("RPC server stopped with error: {:?}", e);
            }
        }
    })
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                CAPABILITY_STREAMED_DETAIL_BATCHES.to_string()
            ]
        );
    }

    #[test]
    fn remote_sync_error_formats_structured_permission_context() {
        let error = RemoteSyncError::new(
            "save remote state",
            Some(PathBuf::from("state.snp")),
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        let formatted = error.to_string();

        assert_eq!(error.side, "remote");
        assert_eq!(error.operation, "save remote state");
        assert_eq!(error.path, Some(PathBuf::from("state.snp")));
        assert_eq!(error.kind, "permission_denied");
        assert!(formatted.contains("duet-sync-error-v1"));
        assert!(formatted.contains("side: remote"));
        assert!(formatted.contains("operation: save remote state"));
        assert!(formatted.contains("path: state.snp"));
        assert!(formatted.contains("kind: permission_denied"));
    }
}
