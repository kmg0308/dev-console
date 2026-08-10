use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SETTINGS_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenMeterSettings {
    pub schema_version: u32,
    pub show_full_token_numbers: bool,
    pub local_device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_folder_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_projects_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hermes_database_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_executable_path: Option<String>,
}

impl TokenMeterSettings {
    pub fn load_or_import(
        settings_path: &Path,
        legacy_plist_path: Option<&Path>,
        new_device_id: &str,
    ) -> Result<Self, SettingsError> {
        if settings_path.exists() {
            return Self::load(settings_path);
        }
        if new_device_id.trim().is_empty() {
            return Err(SettingsError::EmptyDeviceId);
        }

        let legacy = legacy_plist_path
            .filter(|path| path.exists())
            .map(LegacyPreferences::read)
            .transpose()?
            .unwrap_or_default();
        let settings = Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            show_full_token_numbers: legacy.show_full_token_numbers,
            local_device_id: legacy
                .local_device_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| new_device_id.to_owned()),
            sync_folder_path: legacy.sync_folder_path.filter(|path| !path.is_empty()),
            codex_home: None,
            claude_projects_path: None,
            hermes_database_path: None,
            codex_executable_path: None,
        };
        settings.save(settings_path)?;
        Ok(settings)
    }

    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let mut settings: Self = serde_json::from_slice(&fs::read(path)?)?;
        match settings.schema_version {
            1 | 2 | SETTINGS_SCHEMA_VERSION => {}
            version => return Err(SettingsError::UnsupportedSchema(version)),
        }
        if settings.local_device_id.trim().is_empty() {
            return Err(SettingsError::EmptyDeviceId);
        }
        if settings.schema_version != SETTINGS_SCHEMA_VERSION {
            settings.schema_version = SETTINGS_SCHEMA_VERSION;
            settings.save(path)?;
        }
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(SettingsError::UnsupportedSchema(self.schema_version));
        }
        if self.local_device_id.trim().is_empty() {
            return Err(SettingsError::EmptyDeviceId);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let data = serde_json::to_vec_pretty(self)?;
        let temp = temporary_sibling(path);
        let mut replacement_started = false;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&data)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            replacement_started = true;
            crate::atomic_file::replace(&temp, path)
        })();
        if result.is_err() && (!replacement_started || path.exists()) {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(SettingsError::Io)
    }
}

#[derive(Default)]
struct LegacyPreferences {
    show_full_token_numbers: bool,
    local_device_id: Option<String>,
    sync_folder_path: Option<String>,
}

impl LegacyPreferences {
    fn read(path: &Path) -> Result<Self, SettingsError> {
        let value = plist::Value::from_file(path)?;
        let dictionary = value
            .as_dictionary()
            .ok_or(SettingsError::InvalidLegacyPlist)?;
        Ok(Self {
            show_full_token_numbers: dictionary
                .get("showFullTokenNumbers")
                .and_then(plist::Value::as_boolean)
                .unwrap_or(false),
            local_device_id: dictionary
                .get("tokenMeter.localDeviceId")
                .and_then(plist::Value::as_string)
                .map(str::to_owned),
            sync_folder_path: dictionary
                .get("tokenMeter.syncFolderPath")
                .and_then(plist::Value::as_string)
                .map(str::to_owned),
        })
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("tmp-{}-{nonce}", std::process::id()))
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("legacy preferences are invalid: {0}")]
    Plist(#[from] plist::Error),
    #[error("legacy preferences root is not a dictionary")]
    InvalidLegacyPlist,
    #[error("unsupported settings schema version {0}")]
    UnsupportedSchema(u32),
    #[error("local device ID must not be empty")]
    EmptyDeviceId,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imports_legacy_preferences_without_mutating_them_and_keeps_device_id() {
        let directory = tempfile::tempdir().unwrap();
        let plist_path = directory.path().join("local.tokenmeter.app.plist");
        let settings_path = directory.path().join("settings.json");
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("showFullTokenNumbers".into(), plist::Value::Boolean(true));
        dictionary.insert(
            "tokenMeter.localDeviceId".into(),
            plist::Value::String("mac-existing".into()),
        );
        dictionary.insert(
            "tokenMeter.syncFolderPath".into(),
            plist::Value::String("/Volumes/Shared/TokenMeter".into()),
        );
        plist::Value::Dictionary(dictionary)
            .to_file_binary(&plist_path)
            .unwrap();
        let original = fs::read(&plist_path).unwrap();

        let imported = TokenMeterSettings::load_or_import(
            &settings_path,
            Some(&plist_path),
            "must-not-replace-existing",
        )
        .unwrap();
        assert!(imported.show_full_token_numbers);
        assert_eq!(imported.local_device_id, "mac-existing");
        assert_eq!(
            imported.sync_folder_path.as_deref(),
            Some("/Volumes/Shared/TokenMeter")
        );
        assert_eq!(imported.schema_version, SETTINGS_SCHEMA_VERSION);
        assert_eq!(imported.codex_home, None);
        assert_eq!(imported.claude_projects_path, None);
        assert_eq!(imported.hermes_database_path, None);
        assert_eq!(imported.codex_executable_path, None);
        assert_eq!(fs::read(&plist_path).unwrap(), original);

        let reloaded =
            TokenMeterSettings::load_or_import(&settings_path, Some(&plist_path), "another-new-id")
                .unwrap();
        assert_eq!(reloaded.local_device_id, "mac-existing");
    }

    #[test]
    fn missing_legacy_values_use_safe_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let settings = TokenMeterSettings::load_or_import(
            &directory.path().join("settings.json"),
            None,
            "generated-id",
        )
        .unwrap();
        assert!(!settings.show_full_token_numbers);
        assert_eq!(settings.local_device_id, "generated-id");
        assert_eq!(settings.sync_folder_path, None);
        assert_eq!(settings.codex_executable_path, None);
    }

    #[test]
    fn migrates_schema_one_atomically_and_preserves_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "schemaVersion": 1,
  "showFullTokenNumbers": true,
  "localDeviceId": "existing-device",
  "syncFolderPath": "/shared/token-meter"
}"#,
        )
        .unwrap();

        let settings = TokenMeterSettings::load(&path).unwrap();
        assert_eq!(settings.schema_version, SETTINGS_SCHEMA_VERSION);
        assert!(settings.show_full_token_numbers);
        assert_eq!(settings.local_device_id, "existing-device");
        assert_eq!(
            settings.sync_folder_path.as_deref(),
            Some("/shared/token-meter")
        );
        assert_eq!(settings.codex_home, None);
        assert_eq!(settings.claude_projects_path, None);
        assert_eq!(settings.hermes_database_path, None);
        assert_eq!(settings.codex_executable_path, None);

        let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["schemaVersion"], SETTINGS_SCHEMA_VERSION);
    }

    #[test]
    fn saves_and_loads_explicit_source_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let expected = TokenMeterSettings {
            schema_version: SETTINGS_SCHEMA_VERSION,
            show_full_token_numbers: false,
            local_device_id: "device-2".into(),
            sync_folder_path: Some("C:\\TokenMeter Sync".into()),
            codex_home: Some("C:\\Users\\me\\.codex".into()),
            claude_projects_path: Some("C:\\Users\\me\\.claude\\projects".into()),
            hermes_database_path: Some("C:\\Users\\me\\.hermes\\state.db".into()),
            codex_executable_path: Some("C:\\Program Files\\Codex\\codex.cmd".into()),
        };

        expected.save(&path).unwrap();
        assert_eq!(TokenMeterSettings::load(&path).unwrap(), expected);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn replaces_existing_settings_after_closing_the_flushed_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut settings = TokenMeterSettings::load_or_import(&path, None, "device").unwrap();
        settings.show_full_token_numbers = true;

        settings.save(&path).unwrap();

        assert!(
            TokenMeterSettings::load(&path)
                .unwrap()
                .show_full_token_numbers
        );
    }

    #[test]
    fn migrates_schema_two_without_changing_existing_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "schemaVersion": 2,
  "showFullTokenNumbers": true,
  "localDeviceId": "existing-device",
  "syncFolderPath": "/shared/token-meter",
  "codexHome": "/data/codex",
  "claudeProjectsPath": "/data/claude",
  "hermesDatabasePath": "/data/hermes.db"
}"#,
        )
        .unwrap();

        let settings = TokenMeterSettings::load(&path).unwrap();
        assert_eq!(settings.schema_version, SETTINGS_SCHEMA_VERSION);
        assert!(settings.show_full_token_numbers);
        assert_eq!(settings.local_device_id, "existing-device");
        assert_eq!(
            settings.sync_folder_path.as_deref(),
            Some("/shared/token-meter")
        );
        assert_eq!(settings.codex_home.as_deref(), Some("/data/codex"));
        assert_eq!(
            settings.claude_projects_path.as_deref(),
            Some("/data/claude")
        );
        assert_eq!(
            settings.hermes_database_path.as_deref(),
            Some("/data/hermes.db")
        );
        assert_eq!(settings.codex_executable_path, None);

        let stored: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(stored["schemaVersion"], SETTINGS_SCHEMA_VERSION);
    }
}
