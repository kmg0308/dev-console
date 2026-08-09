#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
#[cfg(any(feature = "runtime-atlas", feature = "token-meter"))]
use tauri::Manager;

#[cfg(feature = "runtime-atlas")]
mod runtime_atlas;
#[cfg(feature = "token-meter")]
mod token_meter;
mod updater;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum Feature {
    TokenMeter,
    RuntimeAtlas,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppIdentity {
    kind: &'static str,
    display_name: &'static str,
    features: &'static [Feature],
}

pub(crate) const TOKEN_METER_IDENTIFIER: &str = "local.tokenmeter.app";
pub(crate) const TOKEN_METER_UPDATER_QA_IDENTIFIER_PREFIX: &str = "local.tokenmeter.updaterqa.";

pub(crate) fn is_token_meter_updater_qa(identifier: &str) -> bool {
    identifier
        .strip_prefix(TOKEN_METER_UPDATER_QA_IDENTIFIER_PREFIX)
        .is_some_and(|suffix| {
            suffix.len() == 24
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn resolve_identity(identifier: &str) -> Result<AppIdentity, String> {
    resolve_identity_for_features(
        identifier,
        cfg!(feature = "token-meter"),
        cfg!(feature = "runtime-atlas"),
    )
}

fn resolve_identity_for_features(
    identifier: &str,
    has_token_meter: bool,
    has_runtime_atlas: bool,
) -> Result<AppIdentity, String> {
    match (has_token_meter, has_runtime_atlas) {
        (true, false) if identifier == TOKEN_METER_IDENTIFIER => Ok(AppIdentity {
            kind: "tokenMeter",
            display_name: "TokenMeter",
            features: &[Feature::TokenMeter],
        }),
        (true, false) if is_token_meter_updater_qa(identifier) => Ok(AppIdentity {
            kind: "tokenMeterUpdaterQa",
            display_name: "TokenMeter Updater QA",
            features: &[Feature::TokenMeter],
        }),
        (false, true) if identifier == "com.kmg0308.runtimeatlas" => Ok(AppIdentity {
            kind: "runtimeAtlas",
            display_name: "Runtime Atlas",
            features: &[Feature::RuntimeAtlas],
        }),
        (true, true) if identifier == "com.kmg0308.devconsole" => Ok(AppIdentity {
            kind: "devConsole",
            display_name: "DevConsole",
            features: &[Feature::RuntimeAtlas, Feature::TokenMeter],
        }),
        _ => Err(format!(
            "invalid app identity/features: {identifier} (token-meter={has_token_meter}, runtime-atlas={has_runtime_atlas})"
        )),
    }
}

#[tauri::command]
fn app_identity(app: tauri::AppHandle) -> Result<AppIdentity, String> {
    resolve_identity(&app.config().identifier)
}

fn main() {
    let context = tauri::generate_context!();
    #[cfg(feature = "token-meter")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--token-meter-updater-qa-preflight"))
    {
        let identifier = &context.config().identifier;
        resolve_identity(identifier).expect("updater QA identity is invalid");
        let data_directory = token_meter::updater_qa_data_directory(identifier)
            .expect("updater QA data isolation is invalid")
            .expect("updater QA data isolation is missing");
        println!("{identifier}\n{}", data_directory.display());
        return;
    }
    let builder = tauri::Builder::default();
    let builder = if updater::updater_configured(context.config().plugins.0.get("updater")) {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };
    let builder = builder.setup(|app| {
        resolve_identity(&app.config().identifier).map_err(std::io::Error::other)?;
        #[cfg(feature = "token-meter")]
        app.manage(token_meter::initialize(app.handle()).map_err(std::io::Error::other)?);
        #[cfg(feature = "runtime-atlas")]
        app.manage(runtime_atlas::initialize(app.handle()).map_err(std::io::Error::other)?);
        Ok(())
    });
    #[cfg(all(feature = "runtime-atlas", feature = "token-meter"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_identity,
        updater::updater_check,
        updater::updater_install,
        token_meter::token_meter_dashboard,
        token_meter::token_meter_rebuild_cache,
        token_meter::token_meter_set_sync_folder,
        token_meter::token_meter_set_source_paths,
        token_meter::token_meter_set_show_full_numbers,
        token_meter::token_meter_cleanup_preview,
        token_meter::token_meter_cleanup_apply,
        runtime_atlas::runtime_atlas_status,
        runtime_atlas::runtime_atlas_add_repository,
        runtime_atlas::runtime_atlas_remove_repository,
        runtime_atlas::runtime_atlas_set_language,
        runtime_atlas::runtime_atlas_save_action,
        runtime_atlas::runtime_atlas_delete_action,
        runtime_atlas::runtime_atlas_plan_action,
        runtime_atlas::runtime_atlas_confirm_action,
        runtime_atlas::runtime_atlas_set_worktree_order,
        runtime_atlas::runtime_atlas_stop_action,
        runtime_atlas::runtime_atlas_stop_process,
        runtime_atlas::runtime_atlas_link_process,
        runtime_atlas::runtime_atlas_unlink_process,
        runtime_atlas::runtime_atlas_advance_worktree_navigation,
        runtime_atlas::runtime_atlas_commit_worktree_navigation,
        runtime_atlas::runtime_atlas_record_worktree_selection,
    ]);
    #[cfg(all(feature = "runtime-atlas", not(feature = "token-meter")))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_identity,
        updater::updater_check,
        updater::updater_install,
        runtime_atlas::runtime_atlas_status,
        runtime_atlas::runtime_atlas_add_repository,
        runtime_atlas::runtime_atlas_remove_repository,
        runtime_atlas::runtime_atlas_set_language,
        runtime_atlas::runtime_atlas_save_action,
        runtime_atlas::runtime_atlas_delete_action,
        runtime_atlas::runtime_atlas_plan_action,
        runtime_atlas::runtime_atlas_confirm_action,
        runtime_atlas::runtime_atlas_set_worktree_order,
        runtime_atlas::runtime_atlas_stop_action,
        runtime_atlas::runtime_atlas_stop_process,
        runtime_atlas::runtime_atlas_link_process,
        runtime_atlas::runtime_atlas_unlink_process,
        runtime_atlas::runtime_atlas_advance_worktree_navigation,
        runtime_atlas::runtime_atlas_commit_worktree_navigation,
        runtime_atlas::runtime_atlas_record_worktree_selection,
    ]);
    #[cfg(all(not(feature = "runtime-atlas"), feature = "token-meter"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_identity,
        updater::updater_check,
        updater::updater_install,
        token_meter::token_meter_dashboard,
        token_meter::token_meter_rebuild_cache,
        token_meter::token_meter_set_sync_folder,
        token_meter::token_meter_set_source_paths,
        token_meter::token_meter_set_show_full_numbers,
        token_meter::token_meter_cleanup_preview,
        token_meter::token_meter_cleanup_apply,
    ]);
    #[cfg(not(any(feature = "runtime-atlas", feature = "token-meter")))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_identity,
        updater::updater_check,
        updater::updater_install,
    ]);
    let app = builder
        .build(context)
        .expect("desktop host could not be built");
    app.run(|app, event| {
        if matches!(event, tauri::RunEvent::ExitRequested { .. }) {
            updater::shutdown_runtime_atlas(app);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_flavors_and_ephemeral_updater_qa_identity_are_exact() {
        assert_eq!(
            resolve_identity_for_features(TOKEN_METER_IDENTIFIER, true, false)
                .unwrap()
                .kind,
            "tokenMeter"
        );
        assert_eq!(
            resolve_identity_for_features("com.kmg0308.runtimeatlas", false, true)
                .unwrap()
                .kind,
            "runtimeAtlas"
        );
        assert_eq!(
            resolve_identity_for_features("com.kmg0308.devconsole", true, true)
                .unwrap()
                .kind,
            "devConsole"
        );

        let qa_identifier =
            format!("{TOKEN_METER_UPDATER_QA_IDENTIFIER_PREFIX}0123456789abcdef01234567");
        assert_eq!(
            resolve_identity_for_features(&qa_identifier, true, false)
                .unwrap()
                .kind,
            "tokenMeterUpdaterQa"
        );
        for invalid in [
            TOKEN_METER_UPDATER_QA_IDENTIFIER_PREFIX,
            "local.tokenmeter.updaterqa.0123456789ABCDEF01234567",
            "local.tokenmeter.updaterqa.0123456789abcdef0123456g",
            "com.example.wrong",
        ] {
            assert!(resolve_identity_for_features(invalid, true, false).is_err());
        }
        assert!(resolve_identity_for_features(TOKEN_METER_IDENTIFIER, true, true).is_err());
    }
}
