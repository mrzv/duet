use std::fmt;
use std::path::PathBuf;

use color_eyre::eyre::Report;
use essrpc::RPCError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredSyncError {
    pub version: u8,
    pub side: String,
    pub operation: String,
    pub path: Option<PathBuf>,
    pub kind: String,
    pub sources: Vec<String>,
    pub message: String,
}

impl StructuredSyncError {
    pub fn new(
        side: impl Into<String>,
        operation: impl Into<String>,
        path: Option<PathBuf>,
        error: impl fmt::Debug,
    ) -> Self {
        let message = format!("{:?}", error);
        Self::from_message(side, operation, path, message)
    }

    pub fn from_message(
        side: impl Into<String>,
        operation: impl Into<String>,
        path: Option<PathBuf>,
        message: impl Into<String>,
    ) -> Self {
        Self::from_message_and_sources(side, operation, path, message, Vec::new())
    }

    fn from_message_and_sources(
        side: impl Into<String>,
        operation: impl Into<String>,
        path: Option<PathBuf>,
        message: impl Into<String>,
        sources: Vec<String>,
    ) -> Self {
        let message = message.into();
        let classification_text = if sources.is_empty() {
            message.clone()
        } else {
            format!("{}\n{}", message, sources.join("\n"))
        };
        Self {
            version: 1,
            side: side.into(),
            operation: operation.into(),
            path,
            kind: classify_error_message(&classification_text).to_string(),
            sources,
            message,
        }
    }

    pub fn remote(operation: &str, path: Option<PathBuf>, error: impl fmt::Debug) -> Self {
        Self::new("remote", operation, path, error)
    }

    pub fn from_report(
        side: impl Into<String>,
        operation: impl Into<String>,
        path: Option<PathBuf>,
        report: Report,
    ) -> Self {
        let message = report.to_string();
        let sources = report.chain().skip(1).map(ToString::to_string).collect();
        Self::from_message_and_sources(side, operation, path, message, sources)
    }

    pub fn parse(message: &str) -> Option<Self> {
        let mut lines = message.lines();
        let version = lines
            .next()?
            .strip_prefix("duet-sync-error-v")?
            .parse()
            .ok()?;
        let mut side = None;
        let mut operation = None;
        let mut path = None;
        let mut kind = None;
        let mut sources = Vec::new();
        let mut error_message = None;

        while let Some(line) = lines.next() {
            if let Some(value) = line.strip_prefix("side: ") {
                side = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("operation: ") {
                operation = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("path: ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("kind: ") {
                kind = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("source: ") {
                sources.push(value.to_string());
            } else if let Some(value) = line.strip_prefix("message: ") {
                let mut full_message = value.to_string();
                for continuation in lines {
                    full_message.push('\n');
                    full_message.push_str(continuation);
                }
                error_message = Some(full_message);
                break;
            }
        }

        Some(Self {
            version,
            side: side?,
            operation: operation?,
            path,
            kind: kind?,
            sources,
            message: error_message?,
        })
    }

    pub fn render_for_user(&self) -> String {
        let mut rendered = format!("{} {} failed", self.side, self.operation);
        if let Some(path) = &self.path {
            rendered.push_str(&format!(" at {}", path.display()));
        }
        if self.kind != "other" {
            rendered.push_str(&format!(" ({})", self.kind));
        }
        if let Some(summary) = first_error_line(&self.message) {
            rendered.push_str(&format!(": {}", summary));
        }
        if let Some(source) = self.sources.first() {
            rendered.push_str(&format!("; caused by: {}", source));
        }
        if let Some(recovery) = recovery_line(&self.message) {
            rendered.push('\n');
            rendered.push_str(recovery);
        }
        rendered
    }
}

impl fmt::Display for StructuredSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "duet-sync-error-v{}", self.version)?;
        writeln!(f, "side: {}", self.side)?;
        writeln!(f, "operation: {}", self.operation)?;
        if let Some(path) = &self.path {
            writeln!(f, "path: {}", path.display())?;
        }
        writeln!(f, "kind: {}", self.kind)?;
        for source in &self.sources {
            writeln!(f, "source: {}", single_line(source))?;
        }
        write!(f, "message: {}", self.message)
    }
}

pub fn render_rpc_error(error: &RPCError) -> String {
    let message = error.to_string();
    if let Some(sync_error) = StructuredSyncError::parse(&message) {
        sync_error.render_for_user()
    } else {
        format!("{:?}", error)
    }
}

pub fn render_error(
    side: impl Into<String>,
    operation: impl Into<String>,
    path: Option<PathBuf>,
    error: impl fmt::Debug,
) -> String {
    StructuredSyncError::new(side, operation, path, error).render_for_user()
}

pub fn render_report(
    side: impl Into<String>,
    operation: impl Into<String>,
    path: Option<PathBuf>,
    report: Report,
) -> String {
    StructuredSyncError::from_report(side, operation, path, report).render_for_user()
}

pub fn render_message(
    side: impl Into<String>,
    operation: impl Into<String>,
    path: Option<PathBuf>,
    message: impl Into<String>,
) -> String {
    StructuredSyncError::from_message(side, operation, path, message).render_for_user()
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

fn single_line(value: &str) -> String {
    value.replace('\n', " ")
}

fn first_error_line(message: &str) -> Option<&str> {
    message.lines().map(str::trim).find(|line| !line.is_empty())
}

fn recovery_line(message: &str) -> Option<&str> {
    message.lines().find_map(|line| {
        let line = line.trim();
        line.find("Recovery:").map(|index| &line[index..])
    })
}

#[cfg(test)]
mod tests {
    use std::io;

    use color_eyre::eyre::{eyre, WrapErr};
    use essrpc::{RPCError, RPCErrorKind};

    use super::*;

    #[test]
    fn structured_sync_error_formats_permission_context() {
        let error = StructuredSyncError::remote(
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

    #[test]
    fn structured_sync_error_parses_and_renders_for_users() {
        let rpc_error = RPCError::new(
            RPCErrorKind::Other,
            StructuredSyncError::remote(
                "apply details",
                Some(PathBuf::from("blocked/file.txt")),
                io::Error::from(io::ErrorKind::PermissionDenied),
            )
            .to_string(),
        );

        let rendered = render_rpc_error(&rpc_error);

        assert!(rendered.contains("remote apply details failed"));
        assert!(rendered.contains("blocked/file.txt"));
        assert!(rendered.contains("permission_denied"));
    }

    #[test]
    fn structured_sync_error_rendering_preserves_recovery_advice() {
        let error = StructuredSyncError {
            version: 1,
            side: "remote".to_string(),
            operation: "check apply recovery".to_string(),
            path: Some(PathBuf::from("profile.remotes/state")),
            kind: "other".to_string(),
            sources: Vec::new(),
            message: "previous Duet apply attempt did not finish\nRecovery: filesystem changes were applied, but Duet state may not have been saved on this side."
                .to_string(),
        };
        let rpc_error = RPCError::new(RPCErrorKind::Other, error.to_string());

        let rendered = render_rpc_error(&rpc_error);

        assert!(rendered.contains("remote check apply recovery failed"));
        assert!(rendered.contains("Recovery: filesystem changes were applied"));
        assert!(rendered.contains("state may not have been saved"));
    }

    #[test]
    fn setup_message_rendering_keeps_human_hint() {
        let rendered = render_message(
            "setup",
            "open SSH session",
            None,
            "Permission denied (publickey). Try chmod 600 ~/.ssh/id_ed25519.",
        );

        assert!(rendered.contains("setup open SSH session failed"));
        assert!(rendered.contains("permission_denied"));
        assert!(rendered.contains("Try chmod 600"));
        assert!(!rendered.contains("\"Permission denied"));
    }

    #[test]
    fn structured_sync_error_preserves_source_chain() {
        let report = Err::<(), _>(eyre!("inner permission denied"))
            .wrap_err("outer setup failed")
            .unwrap_err();
        let error = StructuredSyncError::from_report(
            "setup",
            "launch server",
            Some(PathBuf::from("remote.log")),
            report,
        );
        let formatted = error.to_string();
        let parsed = StructuredSyncError::parse(&formatted).unwrap();
        let rendered = parsed.render_for_user();

        assert_eq!(parsed.kind, "permission_denied");
        assert_eq!(parsed.sources, vec!["inner permission denied"]);
        assert!(formatted.contains("source: inner permission denied"));
        assert!(rendered.contains("caused by: inner permission denied"));
    }
}
