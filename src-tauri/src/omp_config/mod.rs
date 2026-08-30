//! Adapter for OMP's native configuration files.
//!
//! OMP providers live in `models.yml` (with `models.yaml` as a read fallback).
//! This module deliberately owns a separate path, lock, and persistence contract
//! from the legacy Pi `models.json` adapter.

use crate::config::{atomic_write_private, get_home_dir};
use crate::error::AppError;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

const MAX_OMP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MISSING_MODELS_REVISION: &str = "missing";
const OMP_API_FORMATS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "openai-codex-responses",
    "azure-openai-responses",
    "anthropic-messages",
    "bedrock-converse-stream",
    "google-generative-ai",
    "google-gemini-cli",
    "google-vertex",
];
static MODELS_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static TEST_AGENT_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

pub(crate) fn get_omp_agent_dir() -> Result<PathBuf, AppError> {
    #[cfg(test)]
    if let Some(path) = TEST_AGENT_DIR
        .lock()
        .expect("lock OMP test directory")
        .clone()
    {
        return validate_agent_dir(path, "OMP test override");
    }

    if let Some(path) = crate::settings::get_omp_override_dir() {
        return validate_agent_dir(path, "OMP settings override");
    }

    // Do not inherit PI_* overrides here. OMP still accepts those historical
    // variables itself, but CC Switch must never point its Pi and OMP state at
    // the same directory implicitly.
    if let Some(value) = std::env::var_os("OMP_CODING_AGENT_DIR").filter(|value| !value.is_empty())
    {
        return validate_agent_dir(
            crate::settings::resolve_override_path(value.to_string_lossy().as_ref()),
            "OMP agent environment override",
        );
    }

    let config_root = std::env::var_os("OMP_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(|value| crate::settings::resolve_override_path(value.to_string_lossy().as_ref()))
        .unwrap_or_else(|| get_home_dir().join(".omp"));
    let profile = std::env::var("OMP_PROFILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let agent_dir = match profile {
        Some(profile) => {
            validate_profile_name(&profile)?;
            config_root.join("profiles").join(profile).join("agent")
        }
        None => config_root.join("agent"),
    };
    validate_agent_dir(agent_dir, "OMP default")
}

fn validate_agent_dir(path: PathBuf, source: &str) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(AppError::InvalidInput(format!(
            "{source} must resolve to an absolute directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn validate_profile_name(profile: &str) -> Result<(), AppError> {
    let valid = profile != "."
        && profile != ".."
        && !profile.contains('/')
        && !profile.contains('\\')
        && !profile.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidInput(format!(
            "Invalid OMP profile name: {profile}"
        )))
    }
}

pub(crate) fn get_omp_models_path() -> Result<PathBuf, AppError> {
    let agent_dir = get_omp_agent_dir()?;
    let primary = agent_dir.join("models.yml");
    let fallback = agent_dir.join("models.yaml");
    Ok(if !primary.exists() && fallback.exists() {
        fallback
    } else {
        primary
    })
}

pub(crate) fn get_omp_settings_path() -> Result<PathBuf, AppError> {
    let agent_dir = get_omp_agent_dir()?;
    let primary = agent_dir.join("config.yml");
    let fallback = agent_dir.join("config.yaml");
    Ok(if !primary.exists() && fallback.exists() {
        fallback
    } else {
        primary
    })
}

pub(crate) fn get_omp_mcp_path() -> Result<PathBuf, AppError> {
    Ok(get_omp_agent_dir()?.join("mcp.json"))
}

pub(crate) fn read_omp_default_provider() -> Result<Option<String>, AppError> {
    let path = get_omp_settings_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let document = read_yaml_value(&path, "OMP settings")?;
    let selector = document
        .get("modelRoles")
        .and_then(Value::as_object)
        .and_then(|roles| roles.get("default"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Ok(selector.and_then(provider_from_model_selector))
}

fn provider_from_model_selector(selector: &str) -> Option<String> {
    let selector = selector.strip_prefix('@').unwrap_or(selector);
    selector
        .split_once('/')
        .map(|(provider, _)| provider.trim())
        .filter(|provider| !provider.is_empty())
        .map(str::to_string)
}

pub(crate) fn read_omp_native_providers() -> Result<IndexMap<String, Value>, AppError> {
    let _guard = lock_models_file()?;
    read_omp_native_providers_locked(&get_omp_models_path()?)
}

pub(crate) fn read_omp_native_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let document = read_models_document(&path)?;
    Ok(providers(&document, &path)?.get(provider_key).cloned())
}

pub(crate) fn omp_provider_exists(provider_key: &str) -> Result<bool, AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let document = read_models_document(&path)?;
    Ok(providers(&document, &path)?.contains_key(provider_key))
}

pub(crate) fn insert_omp_provider(provider_key: &str, config: &Value) -> Result<bool, AppError> {
    validate_provider_node(provider_key, config)?;
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;

    match providers.get(provider_key) {
        Some(current) if current == config => return Ok(false),
        Some(_) => {
            return Err(AppError::InvalidInput(format!(
                "OMP provider key '{provider_key}' already exists in {}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("models.yml")
            )))
        }
        None => {}
    }

    providers.insert(provider_key.to_string(), config.clone());
    write_models_document(&path, &document, &expected_revision)?;
    Ok(true)
}

pub(crate) fn replace_omp_provider(
    provider_key: &str,
    expected: &Value,
    replacement: &Value,
) -> Result<(), AppError> {
    validate_provider_node(provider_key, replacement)?;
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let current = providers.get(provider_key).ok_or_else(|| {
        AppError::Conflict(format!(
            "OMP provider '{provider_key}' is no longer present"
        ))
    })?;
    if current != expected {
        return Err(AppError::Conflict(format!(
            "OMP provider '{provider_key}' changed outside CC Switch"
        )));
    }
    if current == replacement {
        return Ok(());
    }
    providers.insert(provider_key.to_string(), replacement.clone());
    write_models_document(&path, &document, &expected_revision)
}

pub(crate) fn replace_omp_provider_if_present(
    provider_key: &str,
    replacement: &Value,
) -> Result<Option<Value>, AppError> {
    validate_provider_node(provider_key, replacement)?;
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let Some(current) = providers.get(provider_key).cloned() else {
        return Ok(None);
    };
    if current == *replacement {
        return Ok(Some(current));
    }
    providers.insert(provider_key.to_string(), replacement.clone());
    write_models_document(&path, &document, &expected_revision)?;
    Ok(Some(current))
}

pub(crate) fn remove_omp_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    remove_omp_provider_inner(provider_key, None)
}

pub(crate) fn remove_omp_provider_if_matches(
    provider_key: &str,
    expected: &Value,
) -> Result<bool, AppError> {
    remove_omp_provider_inner(provider_key, Some(expected)).map(|removed| removed.is_some())
}

fn remove_omp_provider_inner(
    provider_key: &str,
    expected: Option<&Value>,
) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    let Some(current) = providers.get(provider_key).cloned() else {
        return Ok(None);
    };
    if expected.is_some_and(|expected| current != *expected) {
        return Err(AppError::Conflict(format!(
            "OMP provider '{provider_key}' changed outside CC Switch"
        )));
    }
    providers.remove(provider_key);
    write_models_document(&path, &document, &expected_revision)?;
    Ok(Some(current))
}

pub(crate) fn restore_omp_provider_if_missing(
    provider_key: &str,
    config: &Value,
) -> Result<(), AppError> {
    let _guard = lock_models_file()?;
    let path = get_omp_models_path()?;
    let (mut document, expected_revision) = read_models_document_with_revision(&path)?;
    let providers = providers_mut(&mut document, &path)?;
    match providers.get(provider_key) {
        Some(current) if current == config => Ok(()),
        Some(_) => Err(AppError::Conflict(format!(
            "cannot restore OMP provider '{provider_key}' because another value now owns the key"
        ))),
        None => {
            providers.insert(provider_key.to_string(), config.clone());
            write_models_document(&path, &document, &expected_revision)
        }
    }
}

pub(crate) fn validate_provider_node(provider_key: &str, config: &Value) -> Result<(), AppError> {
    if provider_key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "OMP provider key cannot be empty".to_string(),
        ));
    }
    let provider = config.as_object().ok_or_else(|| {
        AppError::InvalidInput("OMP provider configuration must be an object".to_string())
    })?;
    if provider.contains_key("thinkingLevelMap") {
        return Err(AppError::InvalidInput(
            "OMP uses model.thinking instead of thinkingLevelMap".to_string(),
        ));
    }
    if let Some(api) = provider.get("api") {
        validate_api(api, "provider api")?;
    }
    if let Some(models) = provider.get("models") {
        let models = models.as_array().ok_or_else(|| {
            AppError::InvalidInput("OMP provider models must be an array".to_string())
        })?;
        for (index, model) in models.iter().enumerate() {
            let model = model.as_object().ok_or_else(|| {
                AppError::InvalidInput(format!("OMP model #{} must be an object", index + 1))
            })?;
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    AppError::InvalidInput(format!("OMP model #{} requires an id", index + 1))
                })?;
            if model.contains_key("thinkingLevelMap") {
                return Err(AppError::InvalidInput(format!(
                    "OMP model '{id}' uses model.thinking instead of thinkingLevelMap"
                )));
            }
            if let Some(api) = model.get("api") {
                validate_api(api, &format!("model '{id}' api"))?;
            }
        }
    }
    Ok(())
}

fn validate_api(value: &Value, label: &str) -> Result<(), AppError> {
    let api = value
        .as_str()
        .ok_or_else(|| AppError::InvalidInput(format!("OMP {label} must be a string")))?;
    if OMP_API_FORMATS.contains(&api) {
        Ok(())
    } else {
        Err(AppError::InvalidInput(format!(
            "Unsupported OMP API format '{api}'"
        )))
    }
}

pub(crate) fn provider_base_url(config: &Value) -> Result<String, AppError> {
    let provider = config.as_object().ok_or_else(|| {
        AppError::InvalidInput("OMP provider configuration must be an object".to_string())
    })?;
    nonempty_string(provider.get("baseUrl"))
        .or_else(|| {
            provider
                .get("models")
                .and_then(Value::as_array)
                .and_then(|models| {
                    models
                        .iter()
                        .find_map(|model| nonempty_string(model.get("baseUrl")))
                })
        })
        .map(str::to_string)
        .ok_or_else(|| AppError::InvalidInput("OMP provider has no request URL".to_string()))
}

fn lock_models_file() -> Result<MutexGuard<'static, ()>, AppError> {
    MODELS_FILE_LOCK
        .lock()
        .map_err(|error| AppError::Config(format!("OMP models file lock is poisoned: {error}")))
}

fn read_omp_native_providers_locked(path: &Path) -> Result<IndexMap<String, Value>, AppError> {
    let document = read_models_document(path)?;
    Ok(providers(&document, path)?
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn read_models_document(path: &Path) -> Result<Value, AppError> {
    read_models_document_with_revision(path).map(|(document, _)| document)
}

fn read_models_document_with_revision(path: &Path) -> Result<(Value, String), AppError> {
    if !path.exists() {
        return Ok((
            Value::Object(Map::new()),
            MISSING_MODELS_REVISION.to_string(),
        ));
    }
    let bytes = read_file_limited(path, "OMP models")?;
    let file_revision = revision(&bytes);
    let document = parse_yaml_value(path, "OMP models", &bytes)?;
    Ok((document, file_revision))
}

fn read_yaml_value(path: &Path, label: &str) -> Result<Value, AppError> {
    let bytes = read_file_limited(path, label)?;
    parse_yaml_value(path, label, &bytes)
}

fn parse_yaml_value(path: &Path, label: &str, bytes: &[u8]) -> Result<Value, AppError> {
    serde_yaml::from_slice(bytes).map_err(|error| {
        AppError::Config(format!(
            "{label} file is not valid YAML ({}): {error}",
            path.display()
        ))
    })
}

fn read_file_limited(path: &Path, label: &str) -> Result<Vec<u8>, AppError> {
    let file = fs::File::open(path).map_err(|error| AppError::io(path, error))?;
    let metadata = file.metadata().map_err(|error| AppError::io(path, error))?;
    if metadata.len() > MAX_OMP_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{label} file exceeds the 4 MiB limit: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_OMP_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(path, error))?;
    if bytes.len() as u64 > MAX_OMP_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "{label} file exceeds the 4 MiB limit: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn providers<'a>(document: &'a Value, path: &Path) -> Result<&'a Map<String, Value>, AppError> {
    let root = document.as_object().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models root must be an object: {}",
            path.display()
        ))
    })?;
    match root.get("providers") {
        None => Ok(empty_json_object()),
        Some(Value::Object(providers)) => Ok(providers),
        Some(_) => Err(AppError::Config(format!(
            "OMP models 'providers' must be an object: {}",
            path.display()
        ))),
    }
}

fn providers_mut<'a>(
    document: &'a mut Value,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, AppError> {
    let root = document.as_object_mut().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models root must be an object: {}",
            path.display()
        ))
    })?;
    let value = root
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    value.as_object_mut().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models 'providers' must be an object: {}",
            path.display()
        ))
    })
}

fn empty_json_object() -> &'static Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    &EMPTY
}

fn write_models_document(
    path: &Path,
    document: &Value,
    expected_revision: &str,
) -> Result<(), AppError> {
    let yaml = serde_yaml::to_string(document).map_err(|error| {
        AppError::Config(format!("Failed to serialize OMP models YAML: {error}"))
    })?;
    ensure_private_parent(path)?;
    ensure_models_revision(path, expected_revision)?;
    atomic_write_private(path, yaml.as_bytes())
}

fn ensure_models_revision(path: &Path, expected_revision: &str) -> Result<(), AppError> {
    let actual_revision = match fs::File::open(path) {
        Ok(_) => revision(&read_file_limited(path, "OMP models")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            MISSING_MODELS_REVISION.to_string()
        }
        Err(error) => return Err(AppError::io(path, error)),
    };
    if actual_revision == expected_revision {
        Ok(())
    } else {
        Err(AppError::Conflict(format!(
            "OMP models file changed outside CC Switch: {}",
            path.display()
        )))
    }
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn ensure_private_parent(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Config(format!(
            "OMP models path has no parent directory: {}",
            path.display()
        ))
    })?;
    let created = !parent.exists();
    fs::create_dir_all(parent).map_err(|source| AppError::io(parent, source))?;

    #[cfg(not(unix))]
    let _ = created;

    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| AppError::io(parent, source))?;
    }
    Ok(())
}

fn nonempty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    pub(crate) struct TestAgentDir {
        _dir: Option<tempfile::TempDir>,
        previous: Option<PathBuf>,
    }

    impl TestAgentDir {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().expect("create OMP test directory");
            let agent_dir = dir.path().join("agent");
            Self::set(agent_dir, Some(dir))
        }

        pub(crate) fn at(agent_dir: &Path) -> Self {
            Self::set(agent_dir.to_path_buf(), None)
        }

        fn set(agent_dir: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
            let previous = super::TEST_AGENT_DIR
                .lock()
                .expect("lock OMP test directory")
                .replace(agent_dir);
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TestAgentDir {
        fn drop(&mut self) {
            *super::TEST_AGENT_DIR
                .lock()
                .expect("lock OMP test directory") = self.previous.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn provider() -> Value {
        json!({
            "baseUrl": "https://api.example.com/v1",
            "apiKey": "secret",
            "api": "openai-responses",
            "models": [{ "id": "model-a", "reasoning": true }]
        })
    }

    #[test]
    #[serial]
    fn provider_round_trip_uses_models_yml() {
        let _agent = test_support::TestAgentDir::new();
        assert!(insert_omp_provider("example", &provider()).expect("insert"));
        let path = get_omp_models_path().expect("models path");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("models.yml")
        );
        assert!(!path.with_extension("json").exists());
        assert_eq!(
            read_omp_native_provider("example").expect("read"),
            Some(provider())
        );
        let text = fs::read_to_string(path).expect("read YAML");
        assert!(text.contains("providers:"));
        assert!(text.contains("openai-responses"));
    }

    #[test]
    #[serial]
    fn existing_models_yaml_is_preserved_as_authoritative_path() {
        let _agent = test_support::TestAgentDir::new();
        let agent_dir = get_omp_agent_dir().expect("agent dir");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::write(agent_dir.join("models.yaml"), "providers: {}\n").expect("seed YAML");
        insert_omp_provider("example", &provider()).expect("insert");
        assert!(agent_dir.join("models.yaml").exists());
        assert!(!agent_dir.join("models.yml").exists());
    }

    #[test]
    #[serial]
    fn default_provider_comes_from_model_roles() {
        let _agent = test_support::TestAgentDir::new();
        let path = get_omp_settings_path().expect("settings path");
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(&path, "modelRoles:\n  default: example/model-a:high\n").expect("write settings");
        assert_eq!(
            read_omp_default_provider().expect("read default"),
            Some("example".to_string())
        );
    }

    #[test]
    fn stale_pi_thinking_field_is_rejected() {
        let mut config = provider();
        config["models"][0]["thinkingLevelMap"] = json!({ "high": "high" });
        assert!(validate_provider_node("example", &config).is_err());
    }
}
