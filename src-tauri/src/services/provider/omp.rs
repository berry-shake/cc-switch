use super::{ProviderService, SwitchResult};
use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::{Provider, ProviderMeta, UsageScript};
use crate::store::AppState;
use indexmap::IndexMap;
use serde_json::Value;

const OMP_APP: &str = "omp";

pub(super) fn list(state: &AppState) -> Result<IndexMap<String, Provider>, AppError> {
    let _guard = futures::executor::block_on(state.proxy_service.lock_switch_for_app(OMP_APP));
    match crate::omp_config::read_omp_native_providers() {
        Ok(native) => {
            if let Err(error) = sync_native_locked(state, &native) {
                log::warn!("Failed to sync OMP providers from native config: {error}");
            }
        }
        Err(error) => {
            log::warn!("Failed to read OMP providers; showing saved catalog: {error}");
        }
    }
    state.db.get_all_providers(OMP_APP)
}

pub(super) fn import_from_live(state: &AppState) -> Result<usize, AppError> {
    let _guard = futures::executor::block_on(state.proxy_service.lock_switch_for_app(OMP_APP));
    let native = crate::omp_config::read_omp_native_providers()?;
    sync_native_locked(state, &native)
}

pub(super) fn add(
    state: &AppState,
    mut provider: Provider,
    add_to_live: bool,
) -> Result<bool, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    sanitize_omp_provider(&mut provider);
    ProviderService::validate_provider_settings(&app_type, &provider)?;
    ProviderService::normalize_usage_script_credential_overrides(&app_type, &mut provider);

    if state
        .db
        .get_provider_by_id(&provider.id, app_type.as_str())?
        .is_some()
    {
        return Err(AppError::InvalidInput(format!(
            "OMP provider '{}' already exists",
            provider.id
        )));
    }

    if !add_to_live && crate::omp_config::omp_provider_exists(&provider.id)? {
        return Err(AppError::InvalidInput(format!(
            "OMP provider key '{}' already exists in models.yml",
            provider.id
        )));
    }

    let native_inserted = if add_to_live {
        crate::omp_config::insert_omp_provider(&provider.id, &provider.settings_config)?
    } else {
        false
    };

    if let Err(error) = state.db.save_provider(app_type.as_str(), &provider) {
        if native_inserted {
            if let Err(rollback) = crate::omp_config::remove_omp_provider_if_matches(
                &provider.id,
                &provider.settings_config,
            ) {
                return Err(AppError::Config(format!(
                    "failed to save OMP provider: {error}; native rollback failed: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(true)
}

pub(super) fn update_usage_script(
    state: &AppState,
    id: &str,
    script: UsageScript,
) -> Result<bool, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    super::validate_usage_script(&script)?;

    let mut provider = state
        .db
        .get_provider_by_id(id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput(format!("OMP provider '{id}' not found")))?;
    provider
        .meta
        .get_or_insert_with(ProviderMeta::default)
        .usage_script = Some(script);
    sanitize_omp_provider(&mut provider);
    ProviderService::normalize_usage_script_credential_overrides(&app_type, &mut provider);
    state.db.save_provider(app_type.as_str(), &provider)?;
    Ok(true)
}

pub(super) fn update(
    state: &AppState,
    original_id: Option<&str>,
    mut provider: Provider,
) -> Result<bool, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let original_id = original_id.unwrap_or(&provider.id).to_string();
    if original_id != provider.id {
        return Err(AppError::InvalidInput(
            "OMP provider keys cannot be renamed".to_string(),
        ));
    }

    state
        .db
        .get_provider_by_id(&original_id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput(format!("OMP provider '{original_id}' not found")))?;
    sanitize_omp_provider(&mut provider);
    ProviderService::validate_provider_settings(&app_type, &provider)?;
    ProviderService::normalize_usage_script_credential_overrides(&app_type, &mut provider);

    let previous_native = crate::omp_config::replace_omp_provider_if_present(
        &original_id,
        &provider.settings_config,
    )?;
    if let Err(error) = state.db.save_provider(app_type.as_str(), &provider) {
        if let Some(previous_native) = previous_native.as_ref() {
            if let Err(rollback) = crate::omp_config::replace_omp_provider(
                &original_id,
                &provider.settings_config,
                previous_native,
            ) {
                return Err(AppError::Config(format!(
                    "failed to save OMP provider: {error}; native rollback failed: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(true)
}

pub(super) fn delete(state: &AppState, id: &str) -> Result<(), AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let Some(_) = state.db.get_provider_by_id(id, app_type.as_str())? else {
        return Ok(());
    };
    let removed = crate::omp_config::remove_omp_provider(id)?;

    if let Err(error) = state.db.delete_provider(app_type.as_str(), id) {
        if let Some(removed) = removed.as_ref() {
            if let Err(rollback) = crate::omp_config::restore_omp_provider_if_missing(id, removed) {
                return Err(AppError::Config(format!(
                    "failed to delete OMP provider: {error}; native rollback failed: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn remove(state: &AppState, id: &str) -> Result<(), AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let provider = state
        .db
        .get_provider_by_id(id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput(format!("OMP provider '{id}' not found")))?;
    let Some(removed) = crate::omp_config::remove_omp_provider(id)? else {
        return Ok(());
    };
    let mut synced = provider;
    synced.settings_config = removed.clone();
    if let Err(error) = state.db.save_provider(app_type.as_str(), &synced) {
        if let Err(rollback) = crate::omp_config::restore_omp_provider_if_missing(id, &removed) {
            return Err(AppError::Config(format!(
                "failed to preserve OMP provider before removal: {error}; native rollback failed: {rollback}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn enable(state: &AppState, id: &str) -> Result<SwitchResult, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let provider = state
        .db
        .get_provider_by_id(id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput(format!("OMP provider '{id}' not found")))?;

    if let Some(native) = crate::omp_config::read_omp_native_provider(id)? {
        let mut synced = provider;
        synced.settings_config = native;
        state.db.save_provider(app_type.as_str(), &synced)?;
        return Ok(SwitchResult::default());
    }

    ProviderService::validate_provider_settings(&app_type, &provider)?;
    crate::omp_config::insert_omp_provider(id, &provider.settings_config)?;
    Ok(SwitchResult::default())
}

fn sync_native_locked(
    state: &AppState,
    native: &IndexMap<String, Value>,
) -> Result<usize, AppError> {
    let saved = state.db.get_all_providers(OMP_APP)?;
    let mut changed = 0;

    for (id, config) in native {
        let mut provider = saved.get(id).cloned().unwrap_or_else(|| {
            let mut imported = Provider::with_id(id.clone(), id.clone(), config.clone(), None);
            imported.category = Some("custom".to_string());
            imported.icon = Some("omp".to_string());
            imported
        });
        let is_new = !saved.contains_key(id);
        let previous_config = provider.settings_config.clone();
        provider.settings_config = config.clone();
        if !is_new && provider.settings_config == previous_config {
            continue;
        }

        state.db.save_provider(OMP_APP, &provider)?;
        changed += 1;
    }

    Ok(changed)
}

fn sanitize_omp_provider(provider: &mut Provider) {
    provider.in_failover_queue = false;
    if let Some(config) = provider.settings_config.as_object_mut() {
        // Display names are CC Switch metadata. OMP's provider schema is strict
        // and intentionally has no provider-level `name` field.
        config.remove("name");
    }
    let Some(meta) = provider.meta.take() else {
        return;
    };
    provider.meta = Some(ProviderMeta {
        usage_script: meta.usage_script,
        is_partner: meta.is_partner,
        partner_promotion_key: meta.partner_promotion_key,
        ..ProviderMeta::default()
    });
}
