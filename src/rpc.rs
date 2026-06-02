use std::io::{self, BufWriter};
use std::path::PathBuf;

use bincode::serde::encode_into_std_write as serialize_into;
use color_eyre::eyre::Result;
use essrpc::essrpc;
use essrpc::transports::{BincodeTransport, ReadWrite};
use essrpc::{RPCError, RPCErrorKind, RPCServer};

use crate::actions::Actions;
use crate::profile;
use crate::scan::location::Locations;
use crate::state::{Changes, Entries};
use crate::sync::{self, ChangeDetails, SignatureWithPath};

#[essrpc(sync, async)]
pub trait DuetServer {
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
}

struct DuetServerImpl {
    base: PathBuf,
    remote_id: String,
    all_old: Entries,
    actions: Actions,
}

impl DuetServerImpl {
    fn new() -> Self {
        DuetServerImpl {
            base: PathBuf::from(""),
            remote_id: "".to_string(),
            all_old: Vec::new(),
            actions: Vec::new(),
        }
    }
}

impl DuetServer for DuetServerImpl {
    fn set_base(&mut self, base: String) -> Result<(), RPCError> {
        self.base = match crate::full(&base) {
            Ok(s) => s,
            Err(_) => {
                return Err(RPCError::new(
                    RPCErrorKind::Other,
                    "cannot expand base path, when setting remote base",
                ));
            }
        };
        log::debug!("Set base {}", self.base.display());
        Ok(())
    }

    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError> {
        log::debug!("Setting {} actions", actions.len());
        self.actions = actions;
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
                Some(&profile::remote_state(&self.remote_id)),
            )
            .await
        });

        match result {
            Ok((all_old, changes)) => {
                self.all_old = all_old;
                Ok(changes)
            }
            Err(_) => Err(RPCError::new(
                RPCErrorKind::Other,
                "error in getting changes from the server",
            )),
        }
    }

    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError> {
        log::debug!("Getting signatures");
        let result = sync::get_signatures(&self.base, &self.actions);
        match result {
            Ok(signatures) => Ok(signatures),
            Err(_) => Err(RPCError::new(
                RPCErrorKind::Other,
                "error in getting signatures from the server",
            )),
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
            Err(_) => Err(RPCError::new(
                RPCErrorKind::Other,
                "error in getting detailed changes from the server",
            )),
        }
    }

    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError> {
        log::debug!("Appling detailed changes, with {} details", details.len());
        let result =
            sync::apply_detailed_changes(&self.base, &self.actions, &details, &mut self.all_old);
        match result {
            Ok(()) => Ok(()),
            Err(_) => Err(RPCError::new(
                RPCErrorKind::Other,
                "error in applying detailed changes on the server",
            )),
        }
    }

    fn save_state(&self) -> Result<(), RPCError> {
        log::debug!("Saving state");
        std::fs::create_dir_all(profile::remote_state_dir())?;
        let remote_state = profile::remote_state(&self.remote_id);
        log::info!(
            "Saving remote state {} with {} entries",
            remote_state.to_str().unwrap(),
            &self.all_old.len()
        );
        use atomicwrites::{AllowOverwrite, AtomicFile};
        let af = AtomicFile::new(remote_state, AllowOverwrite);
        let result = af.write(|f| {
            let mut f = BufWriter::new(f);
            serialize_into(&self.all_old, &mut f, bincode::config::legacy())
        });
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(RPCError::new(
                RPCErrorKind::Other,
                "error in saving remote state on the server",
            )),
        }
    }
}

pub async fn server() -> Result<()> {
    std::fs::create_dir_all(crate::full(&"~/.config/duet".to_string())?)?;
    use log::LevelFilter;
    simple_logging::log_to_file(
        crate::full(&"~/.config/duet/remote.log".to_string())?,
        LevelFilter::Debug,
    )?;

    let stdin = io::stdin();
    let stdout = io::stdout();

    let stdio = ReadWrite::new(stdin, stdout);

    log::debug!("in server()");

    tokio::task::spawn_blocking(|| {
        let mut serve =
            DuetServerRPCServer::new(DuetServerImpl::new(), BincodeTransport::new(stdio));
        match serve.serve() {
            Ok(_) => panic!("Expected EOF error"),
            Err(e) => assert_eq!(e.kind, RPCErrorKind::TransportEOF),
        };
    })
    .await?;

    Ok(())
}
