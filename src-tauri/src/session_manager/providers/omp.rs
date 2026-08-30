use std::path::{Path, PathBuf};

use crate::session_manager::{SessionMessage, SessionMeta};

use super::pi::PiSessionDiscovery;

fn resolve_session_root() -> Result<PathBuf, String> {
    crate::omp_config::get_omp_agent_dir()
        .map(|agent_dir| agent_dir.join("sessions"))
        .map_err(|error| error.to_string())
}

pub fn session_roots() -> Vec<PathBuf> {
    resolve_session_root().into_iter().collect()
}

pub(crate) fn session_files() -> Result<Vec<PathBuf>, String> {
    resolve_session_root().map(|root| super::pi::project_session_files(&root))
}

pub fn session_discovery() -> PiSessionDiscovery {
    match resolve_session_root() {
        Ok(_) => PiSessionDiscovery::Available,
        Err(reason) => PiSessionDiscovery::Unavailable { reason },
    }
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    match resolve_session_root() {
        Ok(root) => super::pi::scan_project_sessions(&root, "omp", "omp", "OMP"),
        Err(error) => {
            log::warn!("OMP session discovery unavailable: {error}");
            Vec::new()
        }
    }
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let root = resolve_session_root()?;
    super::pi::load_project_messages(&root, path, "OMP")
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let configured_root = resolve_session_root()?;
    let configured_root = configured_root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve OMP session root {}: {error}",
            configured_root.display()
        )
    })?;
    let requested_root = root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve OMP session root {}: {error}",
            root.display()
        )
    })?;
    if configured_root != requested_root {
        return Err("OMP session root changed before deletion".to_string());
    }
    super::pi::delete_project_session(root, path, session_id, "OMP")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omp_config::test_support::TestAgentDir;
    use serial_test::serial;
    use std::fs;

    #[test]
    #[serial]
    fn scans_only_the_omp_agent_session_root() {
        let _agent = TestAgentDir::new();
        let root = resolve_session_root().expect("session root");
        let project = root.join("project-a");
        fs::create_dir_all(&project).expect("create project dir");
        let path = project.join("2026-03-01T00-00-00_session-omp.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"session-omp\",\"timestamp\":\"2026-03-01T00:00:00Z\",\"cwd\":\"/tmp/project\"}\n",
                "{\"type\":\"message\",\"id\":\"entry-1\",\"parentId\":null,\"timestamp\":\"2026-03-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello OMP\"}}\n"
            ),
        )
        .expect("write session");

        let sessions = scan_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].provider_id, "omp");
        assert!(sessions[0]
            .resume_command
            .as_deref()
            .is_some_and(|command| command.starts_with("omp --session ")));
    }
}
