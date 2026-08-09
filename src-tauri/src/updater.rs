use serde::Serialize;
use tauri::AppHandle;
#[cfg(feature = "runtime-atlas")]
use tauri::Manager;
use tauri_plugin_updater::{Update, UpdaterExt};

const NOT_CONFIGURED: &str = "Update service is not configured for this build";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    available: bool,
    version: Option<String>,
}

#[tauri::command]
pub async fn updater_check(app: AppHandle) -> Result<UpdateCheck, String> {
    let update = check(&app).await?;
    Ok(UpdateCheck {
        available: update.is_some(),
        version: update.map(|update| update.version),
    })
}

#[tauri::command]
pub async fn updater_install(app: AppHandle, expected_version: String) -> Result<(), String> {
    let update = check(&app).await?;
    let update = require_expected_version(update, &expected_version)?;
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(string_error)?;
    validate_artifact(&app, &bytes, &update.version)?;
    shutdown_runtime_atlas_for_update(&app)?;
    #[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
    if app.config().identifier == "com.kmg0308.runtimeatlas" {
        install_runtime_atlas_macos_update(&app, &bytes, &update.version).inspect_err(|_| {
            cancel_runtime_atlas_update(&app);
        })?;
        app.restart();
    }
    update.install(bytes).map_err(|error| {
        cancel_runtime_atlas_update(&app);
        string_error(error)
    })?;
    #[cfg(target_os = "macos")]
    app.restart();
    #[allow(unreachable_code)]
    Ok(())
}

async fn check(app: &AppHandle) -> Result<Option<Update>, String> {
    if !updater_configured(app.config().plugins.0.get("updater")) {
        return Err(NOT_CONFIGURED.into());
    }
    let cleanup = app.clone();
    app.updater_builder()
        .on_before_exit(move || {
            cleanup.cleanup_before_exit();
        })
        .build()
        .map_err(string_error)?
        .check()
        .await
        .map_err(string_error)
}

pub(crate) fn updater_configured(config: Option<&serde_json::Value>) -> bool {
    let Some(config) = config.and_then(serde_json::Value::as_object) else {
        return false;
    };
    config
        .get("pubkey")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|key| !key.trim().is_empty())
        && config
            .get("endpoints")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|endpoints| {
                !endpoints.is_empty()
                    && endpoints.iter().all(|endpoint| {
                        endpoint
                            .as_str()
                            .is_some_and(|endpoint| !endpoint.trim().is_empty())
                    })
            })
}

fn require_expected_version(update: Option<Update>, expected: &str) -> Result<Update, String> {
    let update = update.ok_or_else(|| "The update is no longer available".to_owned())?;
    validate_expected_version(expected, &update.version)?;
    Ok(update)
}

fn validate_expected_version(expected: &str, available: &str) -> Result<(), String> {
    if expected.trim().is_empty() {
        return Err("Expected update version is required".into());
    }
    if expected != available {
        return Err(format!(
            "Update changed from {expected} to {available}; check again before installing"
        ));
    }
    Ok(())
}

struct ArtifactMetadata {
    version: String,
    identity: String,
}

fn validate_artifact(app: &AppHandle, bytes: &[u8], expected_version: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let (actual, expected_identity) = (macos_artifact_metadata(bytes)?, &app.config().identifier);
    #[cfg(windows)]
    let (actual, expected_identity) = (
        windows_artifact_metadata(bytes)?,
        app.config()
            .product_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| "Windows update requires an application product name".to_owned())?,
    );
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (app, bytes, expected_version);
        return Err("Updates are only supported on macOS and Windows".into());
    }

    validate_embedded_artifact(expected_version, expected_identity, &actual)
}

fn validate_embedded_artifact(
    expected_version: &str,
    expected_identity: &str,
    actual: &ArtifactMetadata,
) -> Result<(), String> {
    if actual.version != expected_version {
        return Err(format!(
            "Downloaded update contains version {}, expected {expected_version}",
            actual.version
        ));
    }
    if actual.identity != expected_identity {
        return Err("Downloaded update is for a different application".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_artifact_metadata(bytes: &[u8]) -> Result<ArtifactMetadata, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    const MAX_INFO_PLIST_BYTES: u64 = 1024 * 1024;

    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    let mut metadata = None;
    for entry in archive.entries().map_err(string_error)? {
        let entry = entry.map_err(string_error)?;
        let path = entry.path().map_err(string_error)?;
        if !is_macos_info_plist(&path) {
            continue;
        }
        if metadata.is_some() {
            return Err("Downloaded update contains multiple app Info.plist files".into());
        }
        if !entry.header().entry_type().is_file() || entry.size() > MAX_INFO_PLIST_BYTES {
            return Err("Downloaded update contains an invalid app Info.plist".into());
        }
        let mut plist_bytes = Vec::new();
        entry
            .take(MAX_INFO_PLIST_BYTES + 1)
            .read_to_end(&mut plist_bytes)
            .map_err(string_error)?;
        if plist_bytes.len() as u64 > MAX_INFO_PLIST_BYTES {
            return Err("Downloaded update app Info.plist is too large".into());
        }
        let plist =
            plist::Value::from_reader(std::io::Cursor::new(plist_bytes)).map_err(string_error)?;
        let dictionary = plist
            .as_dictionary()
            .ok_or_else(|| "Downloaded update app Info.plist is not a dictionary".to_owned())?;
        let version = dictionary
            .get("CFBundleShortVersionString")
            .and_then(plist::Value::as_string)
            .filter(|version| !version.is_empty())
            .ok_or_else(|| "Downloaded update app Info.plist has no version".to_owned())?;
        let identity = dictionary
            .get("CFBundleIdentifier")
            .and_then(plist::Value::as_string)
            .filter(|identity| !identity.is_empty())
            .ok_or_else(|| "Downloaded update app Info.plist has no identifier".to_owned())?;
        metadata = Some(ArtifactMetadata {
            version: version.to_owned(),
            identity: identity.to_owned(),
        });
    }
    metadata.ok_or_else(|| "Downloaded update contains no app Info.plist".into())
}

#[cfg(target_os = "macos")]
fn is_macos_info_plist(path: &std::path::Path) -> bool {
    use std::path::Component;

    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(app)) if std::path::Path::new(app).extension().is_some_and(|extension| extension == "app"))
        && matches!(components.next(), Some(Component::Normal(part)) if part == "Contents")
        && matches!(components.next(), Some(Component::Normal(part)) if part == "Info.plist")
        && components.next().is_none()
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn install_runtime_atlas_macos_update(
    app: &AppHandle,
    bytes: &[u8],
    expected_version: &str,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    const BUNDLE_ID: &str = "com.kmg0308.runtimeatlas";
    const APP_NAME: &str = "RuntimeAtlas.app";
    const CLI_NAME: &str = "runtime-atlas";
    const MAX_CLI_BYTES: u64 = 256 * 1024 * 1024;
    const SYSTEM_CLI: &str = "/usr/local/bin/runtime-atlas";

    let current_executable = std::env::current_exe().map_err(string_error)?;
    let current_macos = current_executable
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "MacOS"))
        .ok_or_else(|| "Runtime Atlas update must run from its installed app bundle".to_owned())?;
    let current_contents = current_macos
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        .ok_or_else(|| "Runtime Atlas update must run from its installed app bundle".to_owned())?;
    let current_app = current_contents
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == APP_NAME))
        .ok_or_else(|| "Runtime Atlas update must run from RuntimeAtlas.app".to_owned())?;

    let temporary = RuntimeAtlasUpdateDirectory::new()?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let expected_cli_path = std::path::Path::new(APP_NAME)
        .join("Contents/MacOS")
        .join(CLI_NAME);
    let mut cli_entries = 0;
    for entry in archive.entries().map_err(string_error)? {
        let mut entry = entry.map_err(string_error)?;
        if entry.path().map_err(string_error)?.as_ref() == expected_cli_path {
            if !entry.header().entry_type().is_file() || entry.size() > MAX_CLI_BYTES {
                return Err(
                    "Downloaded Runtime Atlas CLI helper must be a regular file no larger than 256 MiB"
                        .into(),
                );
            }
            cli_entries += 1;
        }
        if !entry.unpack_in(temporary.path()).map_err(string_error)? {
            return Err("Downloaded Runtime Atlas archive contains an unsafe path".into());
        }
    }
    if cli_entries != 1 {
        return Err("Downloaded Runtime Atlas archive must contain exactly one CLI helper".into());
    }
    let new_app = temporary.path().join(APP_NAME);
    let current_cli = current_macos.join(CLI_NAME);
    let new_cli = new_app.join("Contents/MacOS").join(CLI_NAME);

    require_regular_executable(&current_cli, "installed Runtime Atlas CLI helper")?;
    require_regular_executable(&new_cli, "downloaded Runtime Atlas CLI helper")?;
    require_regular_directory(current_app, "installed Runtime Atlas app")?;
    require_regular_directory(&new_app, "downloaded Runtime Atlas app")?;

    verify_code_signature(current_app, true)?;
    verify_code_signature(&new_app, true)?;
    verify_code_signature(&current_cli, false)?;
    verify_code_signature(&new_cli, false)?;

    let current_app_identity = code_identity(current_app)?;
    let new_app_identity = code_identity(&new_app)?;
    let current_cli_identity = code_identity(&current_cli)?;
    let new_cli_identity = code_identity(&new_cli)?;
    if current_app_identity.identifier != BUNDLE_ID || new_app_identity.identifier != BUNDLE_ID {
        return Err("Runtime Atlas app code-signing identity is invalid".into());
    }
    for identity in [&current_cli_identity, &new_cli_identity] {
        if !is_runtime_atlas_cli_identifier(&identity.identifier) {
            return Err("Runtime Atlas CLI helper code-signing role is invalid".into());
        }
    }
    let team = current_app_identity
        .team
        .as_deref()
        .ok_or_else(|| "Installed Runtime Atlas app is not Developer ID signed".to_owned())?;
    if new_app_identity.team.as_deref() != Some(team)
        || current_cli_identity.team.as_deref() != Some(team)
        || new_cli_identity.team.as_deref() != Some(team)
    {
        return Err("Runtime Atlas app and CLI helper signing teams do not match".into());
    }

    let system_cli = std::path::Path::new(SYSTEM_CLI);
    let system_cli_identity = match std::fs::symlink_metadata(system_cli) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "Existing {SYSTEM_CLI} must be a regular non-symlink file"
                ));
            }
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(format!("Existing {SYSTEM_CLI} is not executable"));
            }
            verify_code_signature(system_cli, false)?;
            let identity = code_identity(system_cli)?;
            if !is_runtime_atlas_cli_identifier(&identity.identifier) {
                return Err(format!("Existing {SYSTEM_CLI} is not a Runtime Atlas CLI"));
            }
            if identity
                .team
                .as_deref()
                .is_some_and(|existing| existing != team)
            {
                return Err(format!(
                    "Existing {SYSTEM_CLI} has a different signing team"
                ));
            }
            Some(identity)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Could not inspect {SYSTEM_CLI}: {error}")),
    };

    run_runtime_atlas_install_transaction(RuntimeAtlasInstallTransaction {
        target_app: current_app,
        new_app: &new_app,
        target_cli: system_cli,
        new_cli: &new_cli,
        target_cli_identity: system_cli_identity.as_ref(),
        expected_team: team,
        expected_new_cli_identifier: &new_cli_identity.identifier,
        expected_version,
        fail_after_cli_for_test: false,
    })
    .map_err(|error| format!("Runtime Atlas app and CLI update transaction failed: {error}"))?;
    app.cleanup_before_exit();
    Ok(())
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
struct RuntimeAtlasUpdateDirectory(std::path::PathBuf);

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
impl RuntimeAtlasUpdateDirectory {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "runtime-atlas-update-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path).map_err(string_error)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(string_error)?;
        Ok(Self(path))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
impl Drop for RuntimeAtlasUpdateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
struct CodeIdentity {
    identifier: String,
    team: Option<String>,
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn code_identity(path: &std::path::Path) -> Result<CodeIdentity, String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=4"])
        .arg(path)
        .output()
        .map_err(string_error)?;
    if !output.status.success() {
        return Err(format!(
            "Could not read code-signing identity for {}",
            path.display()
        ));
    }
    let details = String::from_utf8_lossy(&output.stderr);
    let value = |key: &str| {
        details
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::to_owned)
    };
    Ok(CodeIdentity {
        identifier: value("Identifier=")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{} has no code-signing identifier", path.display()))?,
        team: value("TeamIdentifier=").filter(|value| value != "not set" && !value.is_empty()),
    })
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn is_runtime_atlas_cli_identifier(identifier: &str) -> bool {
    ["runtime-atlas-", "runtime_atlas-"]
        .into_iter()
        .find_map(|prefix| identifier.strip_prefix(prefix))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn verify_code_signature(path: &std::path::Path, deep: bool) -> Result<(), String> {
    let mut command = std::process::Command::new("/usr/bin/codesign");
    command.arg("--verify");
    if deep {
        command.arg("--deep");
    }
    let output = command
        .arg("--strict")
        .arg(path)
        .output()
        .map_err(string_error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Code signature verification failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn require_regular_directory(path: &std::path::Path, role: &str) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {role}: {error}"))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(format!("{role} must be a regular non-symlink directory"))
    }
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn require_regular_executable(path: &std::path::Path, role: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {role}: {error}"))?;
    if metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && metadata.permissions().mode() & 0o111 != 0
    {
        Ok(())
    } else {
        Err(format!(
            "{role} must be a regular executable non-symlink file"
        ))
    }
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
struct RuntimeAtlasInstallTransaction<'a> {
    target_app: &'a std::path::Path,
    new_app: &'a std::path::Path,
    target_cli: &'a std::path::Path,
    new_cli: &'a std::path::Path,
    target_cli_identity: Option<&'a CodeIdentity>,
    expected_team: &'a str,
    expected_new_cli_identifier: &'a str,
    expected_version: &'a str,
    fail_after_cli_for_test: bool,
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn run_runtime_atlas_install_transaction(
    transaction: RuntimeAtlasInstallTransaction<'_>,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let RuntimeAtlasInstallTransaction {
        target_app,
        new_app,
        target_cli,
        new_cli,
        target_cli_identity,
        expected_team,
        expected_new_cli_identifier,
        expected_version,
        fail_after_cli_for_test,
    } = transaction;

    const SCRIPT: &str = r#"#!/bin/zsh
set -euo pipefail
NEW_APP="$1"
TARGET_APP="$2"
STAGED_APP="$3"
OLD_APP="$4"
UPDATE_CLI="$5"
NEW_CLI="$6"
TARGET_CLI="$7"
STAGED_CLI="$8"
OLD_CLI="$9"
FAIL_AFTER_CLI_FOR_TEST="${10}"
EXPECTED_TEAM="${11}"
EXPECTED_NEW_CLI_IDENTIFIER="${12}"
EXPECTED_TARGET_CLI_IDENTIFIER="${13}"
EXPECTED_TARGET_CLI_TEAM="${14}"
EXPECTED_VERSION="${15}"
SUCCESS=0
APP_REPLACED=0
CLI_REPLACED=0

rollback() {
  local exit_code=$?
  local rollback_failed=0
  if [[ "$SUCCESS" == "0" ]]; then
    if [[ "$CLI_REPLACED" == "1" ]]; then
      if [[ -f "$OLD_CLI" && ! -L "$OLD_CLI" ]]; then
        /bin/rm -f "$TARGET_CLI" || rollback_failed=1
        /bin/mv "$OLD_CLI" "$TARGET_CLI" || rollback_failed=1
      else
        rollback_failed=1
      fi
    fi
    if [[ "$APP_REPLACED" == "1" ]]; then
      if [[ -d "$OLD_APP" && ! -L "$OLD_APP" ]]; then
        /bin/rm -rf "$TARGET_APP" || rollback_failed=1
        /bin/mv "$OLD_APP" "$TARGET_APP" || rollback_failed=1
      else
        rollback_failed=1
      fi
    fi
  fi
  /bin/rm -rf "$STAGED_APP" || rollback_failed=1
  /bin/rm -f "$STAGED_CLI" || rollback_failed=1
  if [[ "$CLI_REPLACED" == "0" ]]; then /bin/rm -f "$OLD_CLI" || rollback_failed=1; fi
  if [[ "$SUCCESS" == "0" && "$rollback_failed" == "0" ]]; then
    /bin/echo "rollback completed" >&2
  elif [[ "$rollback_failed" == "1" ]]; then
    /bin/echo "rollback incomplete; manual repair is required" >&2
    exit 91
  fi
  exit $exit_code
}
trap rollback EXIT

code_value() {
  /usr/bin/codesign -dv --verbose=4 "$2" 2>&1 | /usr/bin/sed -n "s/^$1=//p"
}

[[ -d "$NEW_APP" && ! -L "$NEW_APP" && -d "$TARGET_APP" && ! -L "$TARGET_APP" ]]
[[ ! -e "$STAGED_APP" && ! -L "$STAGED_APP" && ! -e "$OLD_APP" && ! -L "$OLD_APP" ]]
/usr/bin/codesign --verify --deep --strict "$NEW_APP"
/usr/bin/codesign --verify --deep --strict "$TARGET_APP"
[[ "$(code_value Identifier "$NEW_APP")" == "com.kmg0308.runtimeatlas" ]]
[[ "$(code_value Identifier "$TARGET_APP")" == "com.kmg0308.runtimeatlas" ]]
[[ "$(code_value TeamIdentifier "$NEW_APP")" == "$EXPECTED_TEAM" ]]
[[ "$(code_value TeamIdentifier "$TARGET_APP")" == "$EXPECTED_TEAM" ]]
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$NEW_APP/Contents/Info.plist")" == "$EXPECTED_VERSION" ]]

if [[ "$UPDATE_CLI" == "1" ]]; then
  [[ -f "$NEW_CLI" && ! -L "$NEW_CLI" && -x "$NEW_CLI" ]]
  [[ -f "$TARGET_CLI" && ! -L "$TARGET_CLI" && -x "$TARGET_CLI" ]]
  [[ ! -e "$STAGED_CLI" && ! -L "$STAGED_CLI" && ! -e "$OLD_CLI" && ! -L "$OLD_CLI" ]]
  /usr/bin/codesign --verify --strict "$NEW_CLI"
  /usr/bin/codesign --verify --strict "$TARGET_CLI"
  [[ "$(code_value Identifier "$NEW_CLI")" == "$EXPECTED_NEW_CLI_IDENTIFIER" ]]
  [[ "$(code_value TeamIdentifier "$NEW_CLI")" == "$EXPECTED_TEAM" ]]
  [[ "$(code_value Identifier "$TARGET_CLI")" == "$EXPECTED_TARGET_CLI_IDENTIFIER" ]]
  [[ "$(code_value TeamIdentifier "$TARGET_CLI")" == "$EXPECTED_TARGET_CLI_TEAM" ]]
else
  [[ ! -e "$TARGET_CLI" && ! -L "$TARGET_CLI" ]]
fi

/usr/bin/ditto "$NEW_APP" "$STAGED_APP"
/usr/bin/codesign --verify --deep --strict "$STAGED_APP"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$STAGED_APP/Contents/Info.plist")" == "$EXPECTED_VERSION" ]]
if [[ "$UPDATE_CLI" == "1" ]]; then
  /usr/bin/ditto "$TARGET_CLI" "$OLD_CLI"
  /usr/bin/install -m 0755 "$NEW_CLI" "$STAGED_CLI"
  /usr/bin/codesign --verify --strict "$STAGED_CLI"
fi

/bin/mv "$TARGET_APP" "$OLD_APP"
APP_REPLACED=1
/bin/mv "$STAGED_APP" "$TARGET_APP"
if [[ "$UPDATE_CLI" == "1" ]]; then
  /bin/mv -f "$STAGED_CLI" "$TARGET_CLI"
  CLI_REPLACED=1
fi
if [[ "$FAIL_AFTER_CLI_FOR_TEST" == "1" ]]; then exit 90; fi

/usr/bin/codesign --verify --deep --strict "$TARGET_APP"
[[ "$(code_value Identifier "$TARGET_APP")" == "com.kmg0308.runtimeatlas" ]]
[[ "$(code_value TeamIdentifier "$TARGET_APP")" == "$EXPECTED_TEAM" ]]
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$TARGET_APP/Contents/Info.plist")" == "$EXPECTED_VERSION" ]]
if [[ "$UPDATE_CLI" == "1" ]]; then
  /usr/bin/codesign --verify --strict "$TARGET_CLI"
  [[ "$(code_value Identifier "$TARGET_CLI")" == "$EXPECTED_NEW_CLI_IDENTIFIER" ]]
  [[ "$(code_value TeamIdentifier "$TARGET_CLI")" == "$EXPECTED_TEAM" ]]
  /usr/bin/cmp -s "$TARGET_APP/Contents/MacOS/runtime-atlas" "$TARGET_CLI"
else
  [[ ! -e "$TARGET_CLI" && ! -L "$TARGET_CLI" ]]
fi

SUCCESS=1
/bin/rm -rf "$OLD_APP" || true
/bin/rm -f "$OLD_CLI" || true
"#;

    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let target_parent = target_app
        .parent()
        .ok_or_else(|| "Runtime Atlas app has no parent directory".to_owned())?;
    let cli_parent = target_cli
        .parent()
        .ok_or_else(|| "Runtime Atlas CLI has no parent directory".to_owned())?;
    let staged_app = target_parent.join(format!(".RuntimeAtlas.app.new.{suffix}"));
    let old_app = target_parent.join(format!(".RuntimeAtlas.app.old.{suffix}"));
    let staged_cli = cli_parent.join(format!(".runtime-atlas.new.{suffix}"));
    let old_cli = cli_parent.join(format!(".runtime-atlas.old.{suffix}"));
    for path in [&staged_app, &old_app, &staged_cli, &old_cli] {
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(format!(
                "Update staging path already exists: {}",
                path.display()
            ));
        }
    }

    let helper_directory = RuntimeAtlasUpdateDirectory::new()?;
    let helper = helper_directory.path().join("install.zsh");
    std::fs::write(&helper, SCRIPT).map_err(string_error)?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
        .map_err(string_error)?;
    let arguments = [
        helper.as_os_str(),
        new_app.as_os_str(),
        target_app.as_os_str(),
        staged_app.as_os_str(),
        old_app.as_os_str(),
        std::ffi::OsStr::new(if target_cli_identity.is_some() {
            "1"
        } else {
            "0"
        }),
        new_cli.as_os_str(),
        target_cli.as_os_str(),
        staged_cli.as_os_str(),
        old_cli.as_os_str(),
        std::ffi::OsStr::new(if fail_after_cli_for_test { "1" } else { "0" }),
        std::ffi::OsStr::new(expected_team),
        std::ffi::OsStr::new(expected_new_cli_identifier),
        std::ffi::OsStr::new(
            target_cli_identity.map_or("", |identity| identity.identifier.as_str()),
        ),
        std::ffi::OsStr::new(
            target_cli_identity
                .map_or("", |identity| identity.team.as_deref().unwrap_or("not set")),
        ),
        std::ffi::OsStr::new(expected_version),
    ];

    let needs_admin = !path_is_writable(target_parent)
        || !path_is_writable(target_app)
        || (target_cli_identity.is_some()
            && (!path_is_writable(cli_parent) || !path_is_writable(target_cli)));
    let output = if needs_admin {
        const APPLE_SCRIPT: &str = r#"on run argv
set commandText to "/bin/zsh"
repeat with argumentValue in argv
  set commandText to commandText & " " & quoted form of argumentValue
end repeat
do shell script commandText with administrator privileges
end run
"#;
        let mut child = std::process::Command::new("/usr/bin/osascript")
            .arg("-")
            .args(arguments)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(string_error)?;
        child
            .stdin
            .take()
            .ok_or_else(|| "Could not open administrator authorization input".to_owned())?
            .write_all(APPLE_SCRIPT.as_bytes())
            .map_err(string_error)?;
        child.wait_with_output().map_err(string_error)?
    } else {
        std::process::Command::new("/bin/zsh")
            .args(arguments)
            .output()
            .map_err(string_error)?
    };
    if output.status.success() {
        Ok(())
    } else if needs_admin {
        Err(format!(
            "Administrator permission is required to keep RuntimeAtlas.app and /usr/local/bin/runtime-atlas synchronized: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
fn path_is_writable(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 })
}

#[cfg(windows)]
fn windows_artifact_metadata(bytes: &[u8]) -> Result<ArtifactMetadata, String> {
    if infer::archive::is_zip(bytes) {
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(string_error)?;
        let mut installer = None;
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(string_error)?;
            let Some(path) = entry.enclosed_name() else {
                return Err("Downloaded Windows update ZIP contains an unsafe path".into());
            };
            if !entry.is_dir()
                && path.components().count() == 1
                && path.extension().is_some_and(|extension| extension == "exe")
                && installer.replace(index).is_some()
            {
                return Err(
                    "Downloaded Windows update ZIP contains multiple NSIS installers".into(),
                );
            }
        }
        let index = installer
            .ok_or_else(|| "Downloaded Windows update ZIP contains no NSIS installer".to_owned())?;
        let mut installer = archive.by_index(index).map_err(string_error)?;
        windows_reader_product_metadata(&mut installer)
    } else if infer::app::is_exe(bytes) {
        windows_reader_product_metadata(&mut std::io::Cursor::new(bytes))
    } else {
        Err("Downloaded Windows update is not an NSIS installer".into())
    }
}

#[cfg(windows)]
fn windows_reader_product_metadata(
    reader: &mut impl std::io::Read,
) -> Result<ArtifactMetadata, String> {
    use std::io::Write;

    let mut artifact = tempfile::Builder::new()
        .suffix(".exe")
        .tempfile()
        .map_err(string_error)?;
    std::io::copy(reader, &mut artifact).map_err(string_error)?;
    artifact.flush().map_err(string_error)?;
    windows_file_product_metadata(artifact.path())
}

#[cfg(windows)]
fn windows_file_product_metadata(path: &std::path::Path) -> Result<ArtifactMetadata, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW};

    let path = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return Err("Downloaded Windows update has no readable version resource".into());
    }
    let mut info = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, info.as_mut_ptr().cast()) } == 0 {
        return Err(format!(
            "Could not read downloaded Windows update version: {}",
            std::io::Error::last_os_error()
        ));
    }

    let (translations, translations_len) =
        unsafe { query_windows_version_value(&info, r"\VarFileInfo\Translation")? };
    let translations =
        checked_windows_version_slice(&info, translations, translations_len as usize)?;
    if translations.is_empty() || translations.len() % 4 != 0 {
        return Err("Downloaded Windows update has an invalid version translation table".into());
    }

    Ok(ArtifactMetadata {
        version: windows_version_string(&info, translations, "ProductVersion")?,
        identity: windows_version_string(&info, translations, "ProductName")?,
    })
}

#[cfg(windows)]
fn windows_version_string(info: &[u8], translations: &[u8], field: &str) -> Result<String, String> {
    let mut result = None;
    for translation in translations.chunks_exact(4) {
        let language = u16::from_le_bytes([translation[0], translation[1]]);
        let code_page = u16::from_le_bytes([translation[2], translation[3]]);
        let query = format!(r"\StringFileInfo\{language:04x}{code_page:04x}\{field}");
        let Ok((value, characters)) = (unsafe { query_windows_version_value(info, &query) }) else {
            continue;
        };
        let byte_len = (characters as usize)
            .checked_mul(2)
            .ok_or_else(|| "Downloaded Windows update version is too large".to_owned())?;
        let value = checked_windows_version_slice(info, value, byte_len)?;
        let mut utf16 = value
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect::<Vec<_>>();
        while utf16.last() == Some(&0) {
            utf16.pop();
        }
        if utf16.is_empty() || utf16.contains(&0) {
            continue;
        }
        let value = String::from_utf16(&utf16).map_err(string_error)?;
        if result.as_ref().is_some_and(|current| current != &value) {
            return Err(format!(
                "Downloaded Windows update has conflicting {field} values"
            ));
        }
        result = Some(value);
    }

    result.ok_or_else(|| format!("Downloaded Windows update has no {field} value"))
}

#[cfg(windows)]
unsafe fn query_windows_version_value(
    info: &[u8],
    query: &str,
) -> Result<(*const u8, u32), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::VerQueryValueW;

    let query = std::ffi::OsStr::new(query)
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let mut value = std::ptr::null_mut();
    let mut length = 0;
    if unsafe {
        VerQueryValueW(
            info.as_ptr().cast(),
            query.as_ptr(),
            &mut value,
            &mut length,
        )
    } == 0
        || value.is_null()
    {
        return Err(format!("Downloaded Windows update has no {query:?} value"));
    }
    Ok((value.cast(), length))
}

#[cfg(windows)]
fn checked_windows_version_slice(
    info: &[u8],
    value: *const u8,
    byte_len: usize,
) -> Result<&[u8], String> {
    let start = info.as_ptr() as usize;
    let end = start
        .checked_add(info.len())
        .ok_or_else(|| "Downloaded Windows update version resource is invalid".to_owned())?;
    let value_start = value as usize;
    let value_end = value_start
        .checked_add(byte_len)
        .ok_or_else(|| "Downloaded Windows update version resource is invalid".to_owned())?;
    if value_start < start || value_end > end {
        return Err("Downloaded Windows update version resource is invalid".into());
    }
    Ok(unsafe { std::slice::from_raw_parts(value, byte_len) })
}

pub(crate) fn shutdown_runtime_atlas(_app: &AppHandle) {
    let _ = shutdown_runtime_atlas_for_update(_app);
}

fn shutdown_runtime_atlas_for_update(_app: &AppHandle) -> Result<(), String> {
    #[cfg(feature = "runtime-atlas")]
    if let Some(state) = _app.try_state::<crate::runtime_atlas::RuntimeAtlasState>() {
        return state.shutdown_for_update();
    }
    Ok(())
}

fn cancel_runtime_atlas_update(_app: &AppHandle) {
    #[cfg(feature = "runtime-atlas")]
    if let Some(state) = _app.try_state::<crate::runtime_atlas::RuntimeAtlasState>() {
        state.cancel_update_shutdown();
    }
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_requires_both_real_configuration_fields() {
        let missing = serde_json::json!({});
        let partial = serde_json::json!({"endpoints": ["https://example.com/latest.json"]});
        let configured = serde_json::json!({
            "endpoints": ["https://example.com/latest.json"],
            "pubkey": "public-key"
        });
        assert!(!updater_configured(None));
        assert!(!updater_configured(Some(&missing)));
        assert!(!updater_configured(Some(&partial)));
        assert!(updater_configured(Some(&configured)));
    }

    #[test]
    fn install_requires_the_exact_checked_version() {
        assert!(validate_expected_version("1.2.3", "1.2.3").is_ok());
        assert!(validate_expected_version("", "1.2.3").is_err());
        assert!(validate_expected_version("1.2.3", "1.2.4").is_err());
    }

    #[test]
    fn downloaded_artifact_must_match_version_and_application() {
        let artifact = ArtifactMetadata {
            version: "1.2.3".into(),
            identity: "TokenMeter".into(),
        };
        assert!(validate_embedded_artifact("1.2.3", "TokenMeter", &artifact).is_ok());
        assert!(validate_embedded_artifact("1.2.4", "TokenMeter", &artifact).is_err());
        assert!(validate_embedded_artifact("1.2.3", "RuntimeAtlas", &artifact).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_artifact_metadata_matches_for_raw_and_single_executable_zip() {
        // System binaries may keep their strings in a sibling MUI file; this resource is embedded.
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let raw = windows_artifact_metadata(&bytes).unwrap();
        assert_eq!(raw.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(raw.identity, "DevConsole");

        let zipped =
            windows_artifact_metadata(&windows_test_zip(&[("setup.exe", &bytes)])).unwrap();
        assert_eq!(zipped.version, raw.version);
        assert_eq!(zipped.identity, raw.identity);
        assert!(validate_embedded_artifact(&raw.version, &raw.identity, &zipped).is_ok());
        assert!(validate_embedded_artifact("different-version", &raw.identity, &zipped).is_err());
        assert!(validate_embedded_artifact(&raw.version, "different-product", &zipped).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_artifact_zip_rejects_unsafe_and_multiple_executables() {
        let fake_exe = b"MZ";
        assert!(
            windows_artifact_metadata(&windows_test_zip(&[("../setup.exe", fake_exe)])).is_err()
        );
        assert!(
            windows_artifact_metadata(&windows_test_zip(&[
                ("first.exe", fake_exe),
                ("second.exe", fake_exe),
            ]))
            .is_err()
        );
    }

    #[cfg(windows)]
    fn windows_test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;

        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            archive
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_artifact_metadata_comes_from_the_bundled_info_plist() {
        let current = macos_updater_artifact(Some("1.2.3"));
        let metadata = macos_artifact_metadata(&current).unwrap();
        assert!(validate_embedded_artifact("1.2.3", "local.tokenmeter.app", &metadata).is_ok());
        assert!(validate_embedded_artifact("1.2.4", "local.tokenmeter.app", &metadata).is_err());
        assert!(
            validate_embedded_artifact("1.2.3", "com.kmg0308.runtimeatlas", &metadata).is_err()
        );
        assert!(macos_artifact_metadata(&macos_updater_artifact(None)).is_err());
    }

    #[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
    #[test]
    fn runtime_atlas_app_and_global_cli_transaction_rolls_back_together() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let target_app = temporary.path().join("RuntimeAtlas.app");
        let new_app = temporary.path().join("download/RuntimeAtlas.app");
        signed_runtime_atlas_test_app(&target_app, "/bin/echo", "old");
        signed_runtime_atlas_test_app(&new_app, "/bin/date", "new");
        let target_cli = temporary.path().join("bin/runtime-atlas");
        std::fs::create_dir_all(target_cli.parent().unwrap()).unwrap();
        std::fs::copy("/bin/echo", &target_cli).unwrap();
        std::fs::set_permissions(&target_cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        sign_test_path(&target_cli, "runtime-atlas-deadbeef");
        let new_cli = new_app.join("Contents/MacOS/runtime-atlas");
        let old_cli = std::fs::read(&target_cli).unwrap();
        let target_cli_identity = code_identity(&target_cli).unwrap();
        let new_cli_identity = code_identity(&new_cli).unwrap();

        let failed = run_runtime_atlas_install_transaction(RuntimeAtlasInstallTransaction {
            target_app: &target_app,
            new_app: &new_app,
            target_cli: &target_cli,
            new_cli: &new_cli,
            target_cli_identity: Some(&target_cli_identity),
            expected_team: "not set",
            expected_new_cli_identifier: &new_cli_identity.identifier,
            expected_version: "1.2.3",
            fail_after_cli_for_test: true,
        });
        assert!(failed.unwrap_err().contains("rollback completed"));
        assert_eq!(
            std::fs::read_to_string(target_app.join("Contents/Resources/version")).unwrap(),
            "old"
        );
        assert_eq!(std::fs::read(&target_cli).unwrap(), old_cli);

        run_runtime_atlas_install_transaction(RuntimeAtlasInstallTransaction {
            target_app: &target_app,
            new_app: &new_app,
            target_cli: &target_cli,
            new_cli: &new_cli,
            target_cli_identity: Some(&target_cli_identity),
            expected_team: "not set",
            expected_new_cli_identifier: &new_cli_identity.identifier,
            expected_version: "1.2.3",
            fail_after_cli_for_test: false,
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(target_app.join("Contents/Resources/version")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read(&target_cli).unwrap(),
            std::fs::read(new_cli).unwrap()
        );
        for directory in [temporary.path(), target_cli.parent().unwrap()] {
            assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
                let name = entry.unwrap().file_name();
                let name = name.to_string_lossy();
                !name.contains(".old.") && !name.contains(".new.")
            }));
        }
    }

    #[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
    #[test]
    fn runtime_atlas_cli_code_signing_role_is_explicit() {
        assert!(is_runtime_atlas_cli_identifier(
            "runtime-atlas-55554944547857ce0541321bb5ae2478943555c6"
        ));
        assert!(is_runtime_atlas_cli_identifier(
            "runtime_atlas-ce4eb1782ee4dcf7"
        ));
        for invalid in ["runtime-atlas", "runtime-atlas-not-hex", "other-deadbeef"] {
            assert!(!is_runtime_atlas_cli_identifier(invalid));
        }
    }

    #[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
    fn signed_runtime_atlas_test_app(path: &std::path::Path, cli_source: &str, version: &str) {
        use std::os::unix::fs::PermissionsExt;

        let macos = path.join("Contents/MacOS");
        let resources = path.join("Contents/Resources");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::copy("/bin/echo", macos.join("RuntimeAtlas")).unwrap();
        std::fs::copy(cli_source, macos.join("runtime-atlas")).unwrap();
        for executable in [macos.join("RuntimeAtlas"), macos.join("runtime-atlas")] {
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(resources.join("version"), version).unwrap();
        std::fs::write(
            path.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.kmg0308.runtimeatlas</string>
<key>CFBundleExecutable</key><string>RuntimeAtlas</string>
<key>CFBundleShortVersionString</key><string>1.2.3</string>
</dict></plist>"#,
        )
        .unwrap();
        sign_test_path(&macos.join("runtime-atlas"), "runtime_atlas-deadbeef");
        sign_test_path(path, "com.kmg0308.runtimeatlas");
    }

    #[cfg(all(target_os = "macos", feature = "runtime-atlas"))]
    fn sign_test_path(path: &std::path::Path, identifier: &str) {
        assert!(
            std::process::Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-", "--identifier", identifier])
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(target_os = "macos")]
    fn macos_updater_artifact(version: Option<&str>) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};

        let mut dictionary = plist::Dictionary::new();
        if let Some(version) = version {
            dictionary.insert(
                "CFBundleShortVersionString".into(),
                plist::Value::String(version.into()),
            );
        }
        dictionary.insert(
            "CFBundleIdentifier".into(),
            plist::Value::String("local.tokenmeter.app".into()),
        );
        let mut plist = Vec::new();
        plist::Value::Dictionary(dictionary)
            .to_writer_xml(&mut plist)
            .unwrap();

        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(plist.len() as u64);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                "TokenMeter.app/Contents/Info.plist",
                plist.as_slice(),
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }
}
