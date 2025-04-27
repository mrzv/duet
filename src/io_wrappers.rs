// Modified from the initial version provided by Kagi's Code Assistant

use std::io::{self};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ===== STDIN WRAPPERS =====

/// A wrapper that can hold either tokio::process::ChildStdin or openssh::ChildStdin
pub enum StdinWrapper {
    OpensshStdin(openssh::ChildStdin),
    TokioStdin(tokio::process::ChildStdin),
}

// Asynchronous Write implementation
impl AsyncWrite for StdinWrapper {
    fn poll_write(
        self: Pin<&mut Self>, 
        _cx: &mut Context<'_>, 
        buf: &[u8]
    ) -> Poll<io::Result<usize>> {
        // We need to get a mutable reference to self
        let this = self.get_mut();

        match this {
            StdinWrapper::OpensshStdin(stdin) => {
                // Similar to std stdin
                Pin::new(stdin).poll_write(_cx, buf)
            },
            StdinWrapper::TokioStdin(stdin) => {
                // Similar to std stdin
                Pin::new(stdin).poll_write(_cx, buf)
            },
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>, 
        _cx: &mut Context<'_>
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        match this {
            StdinWrapper::OpensshStdin(stdin) => {
                //stdin.poll_flush()
                Pin::new(stdin).poll_flush(_cx)
            },
            StdinWrapper::TokioStdin(stdin) => {
                //stdin.poll_flush()
                Pin::new(stdin).poll_flush(_cx)
            },
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>, 
        _cx: &mut Context<'_>
    ) -> Poll<io::Result<()>> {
        // Standard stdin doesn't have a shutdown method, so we just return Ok
        let this = self.get_mut();

        match this {
            StdinWrapper::OpensshStdin(stdin) => {
                //stdin.poll_flush()
                Pin::new(stdin).poll_shutdown(_cx)
            },
            StdinWrapper::TokioStdin(stdin) => {
                //stdin.poll_flush()
                Pin::new(stdin).poll_shutdown(_cx)
            },
        }
    }
}

// From implementations
impl From<openssh::ChildStdin> for StdinWrapper {
    fn from(stdin: openssh::ChildStdin) -> Self {
        StdinWrapper::OpensshStdin(stdin)
    }
}

impl From<tokio::process::ChildStdin> for StdinWrapper {
    fn from(stdin: tokio::process::ChildStdin) -> Self {
        StdinWrapper::TokioStdin(stdin)
    }
}

// ===== STDOUT WRAPPERS =====

/// A wrapper that can hold either tokio::process::ChildStdout or openssh::ChildStdout
pub enum StdoutWrapper {
    OpensshStdout(openssh::ChildStdout),
    TokioStdout(tokio::process::ChildStdout),
}

// Asynchronous Read implementation
impl AsyncRead for StdoutWrapper {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        match this {
            StdoutWrapper::OpensshStdout(stdout) => {
                // Similar to std stdout
                Pin::new(stdout).poll_read(_cx, buf)
            },
            StdoutWrapper::TokioStdout(stdout) => {
                // Similar to std stdout
                Pin::new(stdout).poll_read(_cx, buf)
            },
        }
    }
}

// From implementations
impl From<openssh::ChildStdout> for StdoutWrapper {
    fn from(stdout: openssh::ChildStdout) -> Self {
        StdoutWrapper::OpensshStdout(stdout)
    }
}

impl From<tokio::process::ChildStdout> for StdoutWrapper {
    fn from(stdout: tokio::process::ChildStdout) -> Self {
        StdoutWrapper::TokioStdout(stdout)
    }
}
