#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(any(
    test,
    target_os = "windows",
    feature = "runtime-atlas",
    feature = "token-meter"
))]
use std::path::{Component, PathBuf};

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

#[cfg(any(
    test,
    target_os = "windows",
    feature = "runtime-atlas",
    feature = "token-meter"
))]
fn windows_updater_qa_root_for(
    identifier: &str,
    configured_root: Option<&str>,
    configured_flavor: Option<&str>,
    is_windows: bool,
) -> Result<Option<PathBuf>, String> {
    if !is_windows {
        return Ok(None);
    }
    let (Some(root), Some(flavor)) = (configured_root, configured_flavor) else {
        return if configured_root.is_none() && configured_flavor.is_none() {
            Ok(None)
        } else {
            Err("Windows updater QA root and flavor must be configured together".to_owned())
        };
    };
    let expected_flavor = match identifier {
        TOKEN_METER_IDENTIFIER => "token-meter",
        "com.kmg0308.runtimeatlas" => "runtime-atlas",
        "com.kmg0308.devconsole" => "dev-console",
        _ => return Err("Windows updater QA requires an exact production identity".to_owned()),
    };
    if flavor != expected_flavor {
        return Err("Windows updater QA flavor does not match the production identity".to_owned());
    }
    let root = PathBuf::from(root);
    if !root.is_absolute()
        || root.parent().is_none()
        || root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err("Windows updater QA root must be one normalized absolute path".to_owned());
    }
    Ok(Some(root))
}

#[cfg(any(
    target_os = "windows",
    feature = "runtime-atlas",
    feature = "token-meter"
))]
pub(crate) fn windows_updater_qa_root(identifier: &str) -> Result<Option<PathBuf>, String> {
    windows_updater_qa_root_for(
        identifier,
        option_env!("DEV_CONSOLE_WINDOWS_UPDATER_QA_ROOT"),
        option_env!("DEV_CONSOLE_WINDOWS_UPDATER_QA_FLAVOR"),
        cfg!(target_os = "windows"),
    )
}

#[cfg(any(feature = "runtime-atlas", feature = "token-meter"))]
pub(crate) fn platform_paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    #[cfg(target_os = "windows")]
    return left
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy());
    #[cfg(not(target_os = "windows"))]
    return left == right;
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
    #[cfg(target_os = "windows")]
    let mut context = tauri::generate_context!();
    #[cfg(not(target_os = "windows"))]
    let context = tauri::generate_context!();
    #[cfg(target_os = "windows")]
    let windows_updater_qa_root = windows_updater_qa_root(&context.config().identifier)
        .expect("Windows updater QA isolation is invalid");
    #[cfg(target_os = "windows")]
    if let Some(root) = &windows_updater_qa_root {
        for window in &mut context.config_mut().app.windows {
            window.data_directory = Some(root.join("webview").join(&window.label));
        }
    }
    #[cfg(target_os = "windows")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--windows-updater-qa-preflight"))
    {
        let root = windows_updater_qa_root.expect("Windows updater QA isolation is missing");
        println!("{}\n{}", context.config().identifier, root.display());
        return;
    }
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
    let builder = builder.invoke_handler(tauri::generate_handler![
        app_identity,
        updater::updater_check,
        updater::updater_install,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_dashboard,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_rebuild_cache,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_set_sync_folder,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_set_source_paths,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_set_show_full_numbers,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_cleanup_preview,
        #[cfg(feature = "token-meter")]
        token_meter::token_meter_cleanup_apply,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_status,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_add_repository,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_remove_repository,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_set_language,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_save_action,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_delete_action,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_plan_action,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_confirm_action,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_set_worktree_order,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_stop_action,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_stop_process,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_link_process,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_unlink_process,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_advance_worktree_navigation,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_commit_worktree_navigation,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_cancel_worktree_navigation,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_record_worktree_selection,
        #[cfg(feature = "runtime-atlas")]
        runtime_atlas::runtime_atlas_open_worktree_in_vscode,
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

    #[test]
    fn windows_updater_qa_root_requires_exact_production_flavor_and_absolute_path() {
        let root = std::env::temp_dir().join("dev-console-updater-qa");
        let root = root.to_str().unwrap();
        for (identifier, flavor) in [
            (TOKEN_METER_IDENTIFIER, "token-meter"),
            ("com.kmg0308.runtimeatlas", "runtime-atlas"),
            ("com.kmg0308.devconsole", "dev-console"),
        ] {
            assert_eq!(
                windows_updater_qa_root_for(identifier, Some(root), Some(flavor), true).unwrap(),
                Some(PathBuf::from(root))
            );
        }
        assert!(
            windows_updater_qa_root_for(
                TOKEN_METER_IDENTIFIER,
                Some(root),
                Some("dev-console"),
                true
            )
            .is_err()
        );
        assert!(
            windows_updater_qa_root_for(TOKEN_METER_IDENTIFIER, Some(root), None, true).is_err()
        );
        assert!(
            windows_updater_qa_root_for(
                TOKEN_METER_IDENTIFIER,
                Some("relative"),
                Some("token-meter"),
                true
            )
            .is_err()
        );
        assert_eq!(
            windows_updater_qa_root_for(
                TOKEN_METER_IDENTIFIER,
                Some(root),
                Some("token-meter"),
                false
            )
            .unwrap(),
            None
        );
    }
}
