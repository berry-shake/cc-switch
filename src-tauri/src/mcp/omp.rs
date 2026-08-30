//! OMP user-level `mcp.json` synchronization and import.

use crate::app_config::{McpApps, McpServer, MultiAppConfig};
use crate::config::atomic_write_private;
use crate::error::AppError;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

use super::validation::validate_server_spec;

const MAX_MCP_FILE_BYTES: u64 = 4 * 1024 * 1024;
static MCP_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub fn sync_single_server_to_omp(
    _config: &MultiAppConfig,
    id: &str,
    server_spec: &Value,
) -> Result<(), AppError> {
    validate_server_spec(server_spec)?;
    let _guard = lock_file()?;
    let path = crate::omp_config::get_omp_mcp_path()?;
    let mut document = read_document(&path)?;
    let root = root_mut(&mut document, &path)?;
    let servers = object_field_mut(root, "mcpServers", &path)?;
    let mut spec = server_spec.clone();
    if let Some(spec) = spec.as_object_mut() {
        spec.insert("enabled".to_string(), Value::Bool(true));
    }
    servers.insert(id.to_string(), spec);
    remove_name_from_list(root, "disabledServers", id);
    remove_name_from_list(root, "enabledServers", id);
    write_document(&path, &document)
}

pub fn remove_server_from_omp(id: &str) -> Result<(), AppError> {
    let _guard = lock_file()?;
    let path = crate::omp_config::get_omp_mcp_path()?;
    if !path.exists() {
        return Ok(());
    }
    let mut document = read_document(&path)?;
    let root = root_mut(&mut document, &path)?;
    if let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) {
        servers.remove(id);
    }
    remove_name_from_list(root, "disabledServers", id);
    remove_name_from_list(root, "enabledServers", id);
    write_document(&path, &document)
}

pub fn import_from_omp(config: &mut MultiAppConfig) -> Result<usize, AppError> {
    let _guard = lock_file()?;
    let path = crate::omp_config::get_omp_mcp_path()?;
    if !path.exists() {
        return Ok(0);
    }
    let document = read_document(&path)?;
    let root = document.as_object().ok_or_else(|| invalid_root(&path))?;
    let Some(native) = root.get("mcpServers").and_then(Value::as_object) else {
        return Ok(0);
    };
    let disabled = root
        .get("disabledServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let servers = config.mcp.servers.get_or_insert_with(HashMap::new);
    let mut changed = 0;

    for (id, spec) in native {
        let enabled = spec.get("enabled").and_then(Value::as_bool).unwrap_or(true)
            && !disabled.contains(id.as_str());
        if !enabled {
            continue;
        }
        if let Err(error) = validate_server_spec(spec) {
            log::warn!("Skipping invalid OMP MCP server '{id}': {error}");
            continue;
        }
        if let Some(existing) = servers.get_mut(id) {
            if !existing.apps.omp {
                existing.apps.omp = true;
                changed += 1;
            }
        } else {
            let mut normalized = spec.clone();
            if let Some(normalized) = normalized.as_object_mut() {
                normalized.remove("enabled");
            }
            servers.insert(
                id.clone(),
                McpServer {
                    id: id.clone(),
                    name: id.clone(),
                    server: normalized,
                    apps: McpApps {
                        omp: true,
                        ..McpApps::default()
                    },
                    description: None,
                    homepage: None,
                    docs: None,
                    tags: Vec::new(),
                },
            );
            changed += 1;
        }
    }
    Ok(changed)
}

fn lock_file() -> Result<MutexGuard<'static, ()>, AppError> {
    MCP_FILE_LOCK
        .lock()
        .map_err(|error| AppError::Config(format!("OMP MCP file lock is poisoned: {error}")))
}

fn read_document(path: &Path) -> Result<Value, AppError> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let metadata = fs::metadata(path).map_err(|error| AppError::io(path, error))?;
    if metadata.len() > MAX_MCP_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "OMP MCP file exceeds the 4 MiB limit: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| AppError::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        AppError::McpValidation(format!(
            "OMP MCP file is not valid JSON ({}): {error}",
            path.display()
        ))
    })
}

fn root_mut<'a>(
    document: &'a mut Value,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, AppError> {
    document.as_object_mut().ok_or_else(|| invalid_root(path))
}

fn invalid_root(path: &Path) -> AppError {
    AppError::McpValidation(format!(
        "OMP MCP root must be a JSON object: {}",
        path.display()
    ))
}

fn object_field_mut<'a>(
    root: &'a mut Map<String, Value>,
    field: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, AppError> {
    root.entry(field.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            AppError::McpValidation(format!(
                "OMP MCP '{field}' must be an object: {}",
                path.display()
            ))
        })
}

fn remove_name_from_list(root: &mut Map<String, Value>, field: &str, id: &str) {
    let Some(values) = root.get_mut(field).and_then(Value::as_array_mut) else {
        return;
    };
    values.retain(|value| value.as_str() != Some(id));
    if values.is_empty() {
        root.remove(field);
    }
}

fn write_document(path: &Path, document: &Value) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    }
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| AppError::Config(format!("Failed to serialize OMP MCP JSON: {error}")))?;
    bytes.push(b'\n');
    atomic_write_private(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omp_config::test_support::TestAgentDir;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    #[serial]
    fn projection_preserves_unknown_root_and_clears_denylist() {
        let _agent = TestAgentDir::new();
        let path = crate::omp_config::get_omp_mcp_path().expect("MCP path");
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(
            &path,
            r#"{"future":{"keep":true},"mcpServers":{},"disabledServers":["test","other"]}"#,
        )
        .expect("seed MCP");
        sync_single_server_to_omp(
            &MultiAppConfig::default(),
            "test",
            &json!({"command":"npx","args":["server"]}),
        )
        .expect("project server");
        let value: Value =
            serde_json::from_slice(&fs::read(path).expect("read MCP")).expect("parse MCP");
        assert_eq!(value["future"]["keep"], true);
        assert_eq!(value["mcpServers"]["test"]["enabled"], true);
        assert_eq!(value["disabledServers"], json!(["other"]));
    }

    #[test]
    #[serial]
    fn import_skips_native_disabled_server() {
        let _agent = TestAgentDir::new();
        let path = crate::omp_config::get_omp_mcp_path().expect("MCP path");
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(
            path,
            r#"{"mcpServers":{"active":{"command":"a"},"off":{"command":"b","enabled":false}}}"#,
        )
        .expect("seed MCP");
        let mut config = MultiAppConfig::default();
        assert_eq!(import_from_omp(&mut config).expect("import"), 1);
        let servers = config.mcp.servers.expect("servers");
        assert!(servers.contains_key("active"));
        assert!(!servers.contains_key("off"));
    }
}
