//! Resolve the Codex CLI OAuth credential source exactly once per request.
//!
//! Codex defaults `cli_auth_credentials_store` to `file`. On macOS the direct
//! keyring entry is scoped by both service (`Codex Auth`) and a stable account
//! derived from the canonical `CODEX_HOME` path. Keeping this logic in one
//! module prevents quota and analytics requests from silently selecting
//! different accounts during a refresh.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "Codex Auth";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AuthCredentialsStoreMode {
    #[default]
    File,
    Keyring,
    Auto,
    Ephemeral,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCredentialSource {
    File,
    Keyring,
}

#[derive(Clone)]
pub(crate) struct CodexCredentialSnapshot {
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
    pub(crate) account_scope: String,
    pub(crate) source: CodexCredentialSource,
    pub(crate) last_refresh: Option<String>,
}

impl fmt::Debug for CodexCredentialSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexCredentialSnapshot")
            .field("access_token", &"[REDACTED]")
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field("account_scope", &self.account_scope)
            .field("source", &self.source)
            .field("last_refresh", &self.last_refresh)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexCredentialError {
    NotFound(Option<String>),
    Parse(String),
    Unsupported(String),
}

impl CodexCredentialError {
    pub(crate) fn message(&self) -> Option<String> {
        match self {
            Self::NotFound(message) => message.clone(),
            Self::Parse(message) | Self::Unsupported(message) => Some(message.clone()),
        }
    }
}

#[derive(Deserialize)]
struct CodexAuthJson {
    auth_mode: Option<String>,
    tokens: Option<CodexTokens>,
    last_refresh: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

fn parse_store_mode(config_path: &Path) -> Result<AuthCredentialsStoreMode, CodexCredentialError> {
    if !config_path.exists() {
        return Ok(AuthCredentialsStoreMode::File);
    }

    let content = std::fs::read_to_string(config_path).map_err(|error| {
        CodexCredentialError::Parse(format!("Failed to read Codex config.toml: {error}"))
    })?;
    parse_store_mode_from_toml(&content)
}

fn parse_store_mode_from_toml(
    content: &str,
) -> Result<AuthCredentialsStoreMode, CodexCredentialError> {
    let config: toml::Value = toml::from_str(content).map_err(|error| {
        CodexCredentialError::Parse(format!("Failed to parse Codex config.toml: {error}"))
    })?;
    let Some(value) = config.get("cli_auth_credentials_store") else {
        return Ok(AuthCredentialsStoreMode::File);
    };
    let Some(value) = value.as_str() else {
        return Err(CodexCredentialError::Parse(
            "Codex cli_auth_credentials_store must be a string".to_string(),
        ));
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "file" => Ok(AuthCredentialsStoreMode::File),
        "keyring" => Ok(AuthCredentialsStoreMode::Keyring),
        "auto" => Ok(AuthCredentialsStoreMode::Auto),
        "ephemeral" => Ok(AuthCredentialsStoreMode::Ephemeral),
        other => Err(CodexCredentialError::Parse(format!(
            "Unsupported Codex cli_auth_credentials_store value: {other}"
        ))),
    }
}

pub(crate) fn keyring_account_for_codex_home(codex_home: &Path) -> String {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hex = format!("{digest:x}");
    format!("cli|{}", hex.get(..16).unwrap_or(&hex))
}

fn load_file_payload(auth_path: &Path) -> Result<Option<String>, String> {
    if !auth_path.exists() {
        return Ok(None);
    }
    std::fs::read_to_string(auth_path)
        .map(Some)
        .map_err(|error| format!("Failed to read Codex auth file: {error}"))
}

#[cfg(target_os = "macos")]
fn load_keyring_payload(codex_home: &Path) -> Result<Option<String>, String> {
    let account = keyring_account_for_codex_home(codex_home);
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYRING_SERVICE,
            "-a",
            &account,
            "-w",
        ])
        .output()
        .map_err(|error| format!("Failed to read Codex keyring credentials: {error}"))?;

    if !output.status.success() {
        if output.status.code() == Some(44) {
            return Ok(None);
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!(
                "Failed to read Codex keyring credentials (exit {})",
                output.status
            )
        } else {
            format!("Failed to read Codex keyring credentials: {detail}")
        });
    }

    let payload = String::from_utf8(output.stdout)
        .map_err(|error| format!("Codex keyring credentials are not UTF-8: {error}"))?;
    let payload = payload.trim().to_string();
    if payload.is_empty() {
        Ok(None)
    } else {
        Ok(Some(payload))
    }
}

#[cfg(not(target_os = "macos"))]
fn load_keyring_payload(_codex_home: &Path) -> Result<Option<String>, String> {
    Err("Codex keyring credential reading is not implemented on this platform".to_string())
}

fn load_selected_payload<K, F>(
    mode: AuthCredentialsStoreMode,
    mut keyring_loader: K,
    mut file_loader: F,
) -> Result<(String, CodexCredentialSource), CodexCredentialError>
where
    K: FnMut() -> Result<Option<String>, String>,
    F: FnMut() -> Result<Option<String>, String>,
{
    let file = |loader: &mut F| {
        loader()
            .map_err(CodexCredentialError::Parse)?
            .map(|payload| (payload, CodexCredentialSource::File))
            .ok_or(CodexCredentialError::NotFound(None))
    };
    let keyring = |loader: &mut K| {
        loader()
            .map_err(CodexCredentialError::Parse)?
            .map(|payload| (payload, CodexCredentialSource::Keyring))
            .ok_or(CodexCredentialError::NotFound(None))
    };

    match mode {
        AuthCredentialsStoreMode::File => file(&mut file_loader),
        AuthCredentialsStoreMode::Keyring => keyring(&mut keyring_loader),
        AuthCredentialsStoreMode::Auto => match keyring_loader() {
            Ok(Some(payload)) => Ok((payload, CodexCredentialSource::Keyring)),
            Ok(None) | Err(_) => file(&mut file_loader),
        },
        AuthCredentialsStoreMode::Ephemeral => Err(CodexCredentialError::Unsupported(
            "Codex uses ephemeral in-process credentials; CC Switch cannot read them".to_string(),
        )),
    }
}

fn account_scope(account_id: Option<&str>, access_token: &str) -> String {
    let (kind, identity) = account_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| ("account", value))
        .unwrap_or(("token", access_token));
    let mut hasher = Sha256::new();
    hasher.update(b"cc-switch:codex-credential-scope:v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(identity.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    format!("codex:{}", hex.get(..32).unwrap_or(&hex))
}

fn parse_snapshot(
    payload: &str,
    source: CodexCredentialSource,
) -> Result<CodexCredentialSnapshot, CodexCredentialError> {
    let auth: CodexAuthJson = serde_json::from_str(payload).map_err(|error| {
        CodexCredentialError::Parse(format!("Failed to parse Codex auth JSON: {error}"))
    })?;
    if auth.auth_mode.as_deref() != Some("chatgpt") {
        return Err(CodexCredentialError::NotFound(Some(
            "Codex is not using ChatGPT OAuth mode".to_string(),
        )));
    }
    let tokens = auth.tokens.ok_or_else(|| {
        CodexCredentialError::Parse("No tokens in Codex auth credentials".to_string())
    })?;
    let access_token = tokens
        .access_token
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CodexCredentialError::Parse("Codex access_token is empty or missing".to_string())
        })?;
    let account_id = tokens.account_id.filter(|value| !value.trim().is_empty());

    Ok(CodexCredentialSnapshot {
        account_scope: account_scope(account_id.as_deref(), &access_token),
        access_token,
        account_id,
        source,
        last_refresh: auth.last_refresh,
    })
}

pub(crate) fn read_codex_credential_snapshot(
) -> Result<CodexCredentialSnapshot, CodexCredentialError> {
    let codex_home: PathBuf = crate::codex_config::get_codex_config_dir();
    let config_path = crate::codex_config::get_codex_config_path();
    let auth_path = crate::codex_config::get_codex_auth_path();
    let mode = parse_store_mode(&config_path)?;
    let (payload, source) = load_selected_payload(
        mode,
        || load_keyring_payload(&codex_home),
        || load_file_payload(&auth_path),
    )?;
    parse_snapshot(&payload, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_AUTH: &str = r#"{
      "auth_mode":"chatgpt",
      "tokens":{"access_token":"secret","account_id":"acct-a"},
      "last_refresh":"2026-08-22T00:00:00Z"
    }"#;

    #[test]
    fn missing_store_mode_defaults_to_file() {
        assert_eq!(
            parse_store_mode_from_toml("model = \"gpt-5\"").unwrap(),
            AuthCredentialsStoreMode::File
        );
    }

    #[test]
    fn auto_prefers_keyring_and_falls_back_on_missing_or_error() {
        let selected = load_selected_payload(
            AuthCredentialsStoreMode::Auto,
            || Ok(Some("keyring".to_string())),
            || Ok(Some("file".to_string())),
        )
        .unwrap();
        assert_eq!(selected.0, "keyring");
        assert_eq!(selected.1, CodexCredentialSource::Keyring);

        for keyring in [Ok(None), Err("unavailable".to_string())] {
            let selected = load_selected_payload(
                AuthCredentialsStoreMode::Auto,
                || keyring.clone(),
                || Ok(Some("file".to_string())),
            )
            .unwrap();
            assert_eq!(selected.0, "file");
            assert_eq!(selected.1, CodexCredentialSource::File);
        }
    }

    #[test]
    fn explicit_keyring_never_falls_back_to_file() {
        let result = load_selected_payload(
            AuthCredentialsStoreMode::Keyring,
            || Ok(None),
            || Ok(Some("file".to_string())),
        );
        assert_eq!(result, Err(CodexCredentialError::NotFound(None)));
    }

    #[test]
    fn snapshot_scope_is_stable_and_debug_redacts_secrets() {
        let snapshot = parse_snapshot(VALID_AUTH, CodexCredentialSource::File).unwrap();
        assert_eq!(snapshot.account_scope.len(), "codex:".len() + 32);
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("acct-a"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn rejects_missing_or_non_chatgpt_auth_mode() {
        for payload in [
            r#"{"tokens":{"access_token":"must-not-be-used"}}"#,
            r#"{"auth_mode":"apikey","tokens":{"access_token":"must-not-be-used"}}"#,
        ] {
            assert!(matches!(
                parse_snapshot(payload, CodexCredentialSource::File),
                Err(CodexCredentialError::NotFound(_))
            ));
        }
    }
}
