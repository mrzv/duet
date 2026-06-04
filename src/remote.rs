use std::path::Path;
use std::process::Stdio;

use color_eyre::eyre::{eyre, Result};
use essrpc::transports::BincodeAsyncClientTransport;
use essrpc::AsyncRPCClient;
use openssh::{RemoteChild, Session};
use readwrite::ReadWriteTokio;
use tokio::io::{BufReader as AsyncBufReader, BufWriter as AsyncBufWriter};
use tokio::process::{Child, Command as TokioCommand};

use crate::io_wrappers::{StdinWrapper, StdoutWrapper};

pub(crate) fn parse_remote(remote: &String) -> Result<(String, Option<String>, String)> {
    let elements: Vec<&str> = remote.split_whitespace().collect();
    let (remote_server, i) = if elements[0] == "ssh" {
        (Some(elements[1].to_string()), 2)
    } else {
        (None, 0)
    };
    let (remote_cmd, remote_base, i) = if i == elements.len() - 1 {
        ("duet".to_string(), elements[i].to_string(), i + 1)
    } else {
        (elements[i].to_string(), elements[i + 1].to_string(), i + 2)
    };
    if i < elements.len() {
        Err(eyre!("Couldn't parse remote, elements remaining"))
    } else {
        Ok((remote_base, remote_server, remote_cmd))
    }
}

pub(crate) enum Server<'a> {
    Local(Child),
    Remote(RemoteChild<'a>),
}

pub(crate) async fn launch_server<'a>(
    session: &'a Option<Session>,
    cmd: String,
    server_log: &Path,
) -> Result<Server<'a>> {
    if let Some(session) = session {
        let server = session
            .command(cmd)
            .arg("--server")
            .stdin(openssh::process::Stdio::piped())
            .stdout(openssh::process::Stdio::piped())
            .stderr(openssh::process::Stdio::inherit())
            .spawn()
            .await?;

        log::trace!("launched remote server");

        Ok(Server::Remote(server))
    } else {
        let cmd = crate::full(&cmd)
            .map_err(|e| eyre!("failed to expand local server command {}: {}", cmd, e))?
            .to_string_lossy()
            .to_string();
        let server = TokioCommand::new(cmd)
            .arg("--server")
            .env(crate::rpc::SERVER_LOG_ENV, server_log)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        log::trace!("launched local server");

        Ok(Server::Local(server))
    }
}

pub(crate) fn get_remote<'a>(
    server: &'a mut Server,
) -> Result<
    crate::rpc::DuetServerAsyncRPCClient<
        BincodeAsyncClientTransport<
            ReadWriteTokio<AsyncBufReader<StdoutWrapper>, AsyncBufWriter<StdinWrapper>>,
        >,
    >,
> {
    let (server_in, server_out) = match server {
        Server::Local(server) => {
            let server_in = server
                .stdin
                .take()
                .ok_or_else(|| eyre!("failed to open local server stdin"))?;
            let server_out = server
                .stdout
                .take()
                .ok_or_else(|| eyre!("failed to open local server stdout"))?;
            (
                StdinWrapper::TokioStdin(server_in),
                StdoutWrapper::TokioStdout(server_out),
            )
        }
        Server::Remote(server) => {
            let server_in = server
                .stdin()
                .take()
                .ok_or_else(|| eyre!("failed to open remote server stdin"))?;
            let server_out = server
                .stdout()
                .take()
                .ok_or_else(|| eyre!("failed to open remote server stdout"))?;
            (
                StdinWrapper::OpensshStdin(server_in),
                StdoutWrapper::OpensshStdout(server_out),
            )
        }
    };

    let server_io = ReadWriteTokio::new(
        AsyncBufReader::new(server_out),
        AsyncBufWriter::new(server_in),
    );

    Ok(DuetServerAsyncRPCClient::new(
        BincodeAsyncClientTransport::new(server_io),
    ))
}

use crate::rpc::DuetServerAsyncRPCClient;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_base_with_default_command() {
        let (base, server, cmd) = parse_remote(&"/remote/base".to_string()).unwrap();

        assert_eq!(base, "/remote/base");
        assert_eq!(server, None);
        assert_eq!(cmd, "duet");
    }

    #[test]
    fn parses_remote_command_and_base() {
        let (base, server, cmd) = parse_remote(&"/bin/duet /remote/base".to_string()).unwrap();

        assert_eq!(base, "/remote/base");
        assert_eq!(server, None);
        assert_eq!(cmd, "/bin/duet");
    }

    #[test]
    fn parses_ssh_remote() {
        let (base, server, cmd) =
            parse_remote(&"ssh example.com /bin/duet /remote/base".to_string()).unwrap();

        assert_eq!(base, "/remote/base");
        assert_eq!(server, Some("example.com".to_string()));
        assert_eq!(cmd, "/bin/duet");
    }
}
