//! Private, bounded details behind the public error reference.
use crate::config::EngineConfig;
use crate::db::{Database, DbError, now_ms};
use regex::Regex;
use shade_protocol::{Diagnostic, DiagnosticOrigin};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::sync::OnceLock;

const MAX_MESSAGE_BYTES: usize = 8192;

pub(crate) fn new(
    origin: DiagnosticOrigin,
    code: &str,
    error: impl std::fmt::Display,
) -> Diagnostic {
    let message = error.to_string();
    static CREDENTIAL_CONTEXT: OnceLock<Regex> = OnceLock::new();
    let credential_context = CREDENTIAL_CONTEXT.get_or_init(|| {
        Regex::new(r#"(?ix)
            (?:[\w.-]*(?:password|passwd|token|secret|api[_-]?key|private[_-]?key|_auth)[\w.-]*)["']?\s*[:=]\s*\S
            |(?:authorization|proxy-authorization|cookie|set-cookie)\s*:\s*\S
            |(?:bearer|basic)\s+[a-z0-9+/=_-]+
            |[a-z][a-z0-9+.-]*://[^\s/@:]+:[^\s/@]+@
            |--(?:password|passwd|token|secret|api-key)\s+\S
        "#).expect("static diagnostic credential pattern")
    });
    // Scan before truncation, including multiline private keys. Omitting the
    // entire detail avoids leaking pieces across lines or truncation boundaries.
    let redacted = crate::secret_policy::contains_secret(message.as_bytes())
        || credential_context.is_match(&message);
    let mut message = if redacted {
        "<REDACTED: diagnostic contains credentials>".to_owned()
    } else {
        message
    };
    let truncated = message.len() > MAX_MESSAGE_BYTES;
    if truncated {
        let mut boundary = MAX_MESSAGE_BYTES;
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        message.truncate(boundary);
    }
    Diagnostic {
        id: format!("diag_{}", ulid::Ulid::new()),
        origin,
        operation: None,
        code: code.to_owned(),
        message,
        redacted,
        truncated,
        created_at_ms: now_ms(),
    }
}

pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("diag_")
        .is_some_and(|value| value.len() == 26 && value.parse::<ulid::Ulid>().is_ok())
}

/// No daemon is needed to record a local transport, startup or argument failure.
pub fn record_cli_error(
    config: &EngineConfig,
    error: impl std::fmt::Display,
) -> Result<Diagnostic, DbError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&config.root)?;
    std::fs::set_permissions(&config.root, std::fs::Permissions::from_mode(0o700))?;
    let diagnostic = new(DiagnosticOrigin::Cli, "CLI_FAILED", error);
    Database::open(config.database_path())?.record_diagnostic(&diagnostic)?;
    Ok(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_before_bounding_or_persisting_the_message() {
        for secret in [
            "TOKEN=short",
            "Authorization: Bearer opaque",
            "Cookie: sid=small",
            "https://user:pw@host.invalid/path",
            "--password short",
            "-----BEGIN RSA PRIVATE KEY-----\nprivate bytes\n-----END RSA PRIVATE KEY-----",
        ] {
            let input = format!("{}\n{secret}", "é".repeat(MAX_MESSAGE_BYTES));
            let diagnostic = new(DiagnosticOrigin::Daemon, "INTERNAL", &input);
            assert!(diagnostic.redacted, "credential case was retained");
            assert!(!diagnostic.truncated);
            assert_eq!(
                diagnostic.message,
                "<REDACTED: diagnostic contains credentials>"
            );
        }
        let diagnostic = new(
            DiagnosticOrigin::Daemon,
            "INTERNAL",
            "⚠".repeat(MAX_MESSAGE_BYTES),
        );
        assert!(diagnostic.truncated);
        assert!(!diagnostic.redacted);
        assert!(diagnostic.message.len() <= MAX_MESSAGE_BYTES);
        assert!(valid_id(&diagnostic.id));
        assert!(!valid_id("../../private"));
    }
}
