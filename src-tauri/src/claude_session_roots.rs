//! Read-only Claude discovery roots. Never use these for provider/MCP/Skills writes.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug)]
pub(crate) struct ClaudeSessionRoot {
    pub config_dir: PathBuf,
    pub projects_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeScanDirectoryStatus {
    pub path: String,
    pub status: &'static str,
}

pub(crate) fn normalize_directories(dirs: &mut Vec<String>) {
    let mut seen = HashSet::new();
    dirs.retain_mut(|dir| {
        *dir = dir.trim().to_owned();
        !dir.is_empty() && seen.insert(dir.clone())
    });
}

pub(crate) fn validate_directories(dirs: &[String]) -> Result<(), String> {
    for dir in dirs.iter().filter(|dir| !dir.trim().is_empty()) {
        if dir.chars().any(char::is_control)
            || !crate::settings::resolve_override_path(dir.trim()).is_absolute()
        {
            return Err("Claude 扫描目录必须为绝对路径或 ~/ 路径，且不能包含控制字符".into());
        }
    }
    Ok(())
}

pub(crate) fn roots() -> Vec<ClaudeSessionRoot> {
    let settings = crate::settings::get_settings();
    let primary = settings
        .claude_config_dir
        .as_deref()
        .map(crate::settings::resolve_override_path)
        .unwrap_or_else(|| crate::config::get_home_dir().join(".claude"));
    resolve_roots(primary, &settings.claude_additional_config_dirs)
}

pub(crate) fn resolve_roots(primary: PathBuf, extras: &[String]) -> Vec<ClaudeSessionRoot> {
    let mut seen = HashSet::new();
    std::iter::once(primary)
        .chain(
            extras
                .iter()
                .filter(|dir| {
                    !dir.trim().is_empty() && validate_directories(&[(*dir).clone()]).is_ok()
                })
                .map(|dir| crate::settings::resolve_override_path(dir.trim())),
        )
        .filter_map(|config_dir| {
            let projects_dir = config_dir.join("projects");
            let identity = projects_dir
                .canonicalize()
                .unwrap_or_else(|_| projects_dir.clone());
            seen.insert(identity).then_some(ClaudeSessionRoot {
                config_dir,
                projects_dir,
            })
        })
        .collect()
}

pub(crate) fn inspect_directories(
    primary: PathBuf,
    dirs: &[String],
) -> Vec<ClaudeScanDirectoryStatus> {
    let primary_projects = primary.join("projects");
    let mut seen = HashSet::from([primary_projects.canonicalize().unwrap_or(primary_projects)]);
    dirs.iter()
        .map(|raw| {
            let path = raw.trim().to_owned();
            let projects = crate::settings::resolve_override_path(&path).join("projects");
            let identity = projects.canonicalize().unwrap_or_else(|_| projects.clone());
            let status =
                if path.is_empty() || validate_directories(std::slice::from_ref(raw)).is_err() {
                    "invalid"
                } else if !seen.insert(identity) {
                    "duplicate"
                } else {
                    match std::fs::read_dir(&projects) {
                        Ok(_) => "ready",
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
                        Err(_) => "unavailable",
                    }
                };
            ClaudeScanDirectoryStatus { path, status }
        })
        .collect()
}

pub(crate) fn canonical_file(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    (path.starts_with(root) && path.is_file()).then_some(path)
}

/// Only traverse directories inside this root, once, including symlink aliases.
pub(crate) fn collect_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let mut pending = vec![root.clone()];
    let mut visited = HashSet::new();
    let mut files = HashSet::new();
    while let Some(dir) = pending.pop() {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if !dir.starts_with(&root) || !visited.insert(dir.clone()) {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if dir == root => return Err(error),
            Err(error) => {
                log::warn!("Claude 会话目录不可读 {}: {error}", dir.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                if let Some(path) = canonical_file(&root, &path) {
                    files.insert(path);
                }
            }
        }
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort();
    Ok(files)
}

pub(crate) fn resume_command(config_dir: &Path, session_id: &str) -> String {
    let default = crate::config::get_home_dir().join(".claude");
    let is_default = config_dir == default;
    if cfg!(windows) {
        let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
        let assignment = if is_default {
            "$null".to_owned()
        } else {
            quote(&config_dir.to_string_lossy())
        };
        format!("& {{ $previousClaudeDir = $env:CLAUDE_CONFIG_DIR; try {{ $env:CLAUDE_CONFIG_DIR = {assignment}; claude --resume {} }} finally {{ $env:CLAUDE_CONFIG_DIR = $previousClaudeDir }} }}", quote(session_id))
    } else {
        let quote = crate::session_manager::terminal::shell_escape;
        let env = if is_default {
            "env -u CLAUDE_CONFIG_DIR".to_owned()
        } else {
            format!(
                "env CLAUDE_CONFIG_DIR={}",
                quote(&config_dir.to_string_lossy())
            )
        };
        format!("{env} claude --resume {}", quote(session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_normalizes_without_creating_directories() {
        let mut dirs = vec![
            " /tmp/one ".into(),
            "".into(),
            "/tmp/one".into(),
            "~/two".into(),
        ];
        normalize_directories(&mut dirs);
        assert_eq!(dirs, vec!["/tmp/one", "~/two"]);
        assert!(validate_directories(&dirs).is_ok());
        assert!(validate_directories(&["relative/path".into()]).is_err());
        assert!(validate_directories(&["/tmp/a\nb".into()]).is_err());
    }

    #[test]
    fn roots_keep_primary_and_report_unavailable_extras() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary");
        std::fs::create_dir_all(primary.join("projects")).unwrap();
        let extras = vec![
            primary.to_string_lossy().into_owned(),
            tmp.path().join("missing").to_string_lossy().into_owned(),
        ];
        assert_eq!(resolve_roots(primary.clone(), &extras).len(), 2);
        let statuses = inspect_directories(primary, &extras);
        assert_eq!(statuses[0].status, "duplicate");
        assert_eq!(statuses[1].status, "missing");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_loop_escape_or_duplicate_roots() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let projects = config.join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("one.jsonl"), "{}\n").unwrap();
        std::fs::write(tmp.path().join("outside.jsonl"), "{}\n").unwrap();
        symlink(&projects, projects.join("loop")).unwrap();
        symlink(
            tmp.path().join("outside.jsonl"),
            projects.join("escape.jsonl"),
        )
        .unwrap();
        let alias = tmp.path().join("alias");
        symlink(&config, &alias).unwrap();
        assert_eq!(
            resolve_roots(config, &[alias.to_string_lossy().into_owned()]).len(),
            1
        );
        assert_eq!(collect_files(&projects).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn resume_quotes_source_and_id_and_isolates_default_environment() {
        assert_eq!(
            resume_command(Path::new("/tmp/work's profile"), "id'1"),
            "env CLAUDE_CONFIG_DIR='/tmp/work'\\''s profile' claude --resume 'id'\\''1'"
        );
        assert!(
            resume_command(&crate::config::get_home_dir().join(".claude"), "id")
                .starts_with("env -u CLAUDE_CONFIG_DIR ")
        );
    }
}
