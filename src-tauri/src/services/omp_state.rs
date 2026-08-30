//! Read-only OMP provider membership and global default reference.

use crate::error::AppError;
use crate::omp_config::{read_omp_default_provider, read_omp_native_providers};
use crate::store::AppState;
use serde::Serialize;

const OMP_APP: &str = "omp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmpCurrentState {
    pub enabled_provider_ids: Vec<String>,
    pub default_provider_id: Option<String>,
}

pub(crate) struct OmpStateService;

impl OmpStateService {
    pub(crate) fn current(state: &AppState) -> Result<OmpCurrentState, AppError> {
        let _guard = futures::executor::block_on(state.proxy_service.lock_switch_for_app(OMP_APP));
        let native = read_omp_native_providers()?;
        let enabled_provider_ids = native.keys().cloned().collect::<Vec<_>>();
        let default_provider_id = match read_omp_default_provider() {
            Ok(default_provider) => default_provider,
            Err(error) => {
                log::warn!("Failed to read OMP global default provider for advisory UI: {error}");
                None
            }
        };
        Ok(OmpCurrentState {
            enabled_provider_ids,
            default_provider_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::omp_config::test_support::TestAgentDir;
    use serial_test::serial;
    use std::fs;
    use std::sync::Arc;

    #[test]
    #[serial]
    fn state_reads_omp_yaml_without_using_pi_json() {
        let _agent = TestAgentDir::new();
        let state = AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ));
        let models_path = crate::omp_config::get_omp_models_path().expect("models path");
        fs::create_dir_all(models_path.parent().expect("models directory"))
            .expect("create models directory");
        fs::write(
            models_path,
            "providers:\n  cc-switch-managed:\n    baseUrl: https://api.example.com/v1\n    api: openai-responses\n    models:\n      - id: model-a\n",
        )
        .expect("write models");
        let settings_path = crate::omp_config::get_omp_settings_path().expect("settings path");
        fs::write(
            settings_path,
            "modelRoles:\n  default: cc-switch-managed/model-a:high\n",
        )
        .expect("write settings");

        let current = OmpStateService::current(&state).expect("read state");
        assert_eq!(
            current.enabled_provider_ids,
            vec!["cc-switch-managed".to_string()]
        );
        assert_eq!(
            current.default_provider_id.as_deref(),
            Some("cc-switch-managed")
        );
    }
}
