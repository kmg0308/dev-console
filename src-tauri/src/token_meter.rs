use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Duration,
};

use chrono::{Local, Utc, Weekday};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime, State};
use token_meter_core::{
    account_service::fetch_codex_account_usage,
    cache::{OriginKind, TokenEventCache},
    cleanup::{CodexSessionCleanupPlan, CodexSessionEvidence, plan_codex_session_cleanup},
    cleanup_archive::{cleanup_source_snapshot, create_cleanup_archive, remove_cleanup_sources},
    dashboard::{
        DashboardAccountState, DashboardRequest, DashboardSettings, DashboardSnapshotDto,
        compose_dashboard,
    },
    models::{ScanResult, TokenDeviceMetadata},
    scanner::{ScannerRoots, TokenLogScanner},
    settings::TokenMeterSettings,
    sync_store::{TokenSyncStore, merge_local_and_sync},
};
use uuid::Uuid;

pub struct TokenMeterState {
    operation: Mutex<()>,
    dashboard_data: Arc<Mutex<DashboardData>>,
    cleanup_plan: Mutex<Option<PendingCleanup>>,
    data_dir: PathBuf,
    source_isolation_root: Option<PathBuf>,
    home_dir: PathBuf,
    legacy_preferences: Option<PathBuf>,
    codex_home_env: Option<PathBuf>,
    account_executable: Option<OsString>,
    local_device_name: String,
}

struct DashboardData {
    scan: Option<ScanResult>,
    account: DashboardAccountState,
    account_generation: u64,
}

struct PendingCleanup {
    id: Uuid,
    plan: CodexSessionCleanupPlan,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePaths {
    codex_home: Option<String>,
    claude_projects_path: Option<String>,
    hermes_database_path: Option<String>,
    codex_executable_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildCacheResult {
    event_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupPreviewResult {
    plan_id: Uuid,
    candidate_count: usize,
    total_bytes: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupApplyResult {
    archived_count: usize,
}

pub fn initialize<R: Runtime>(app: &AppHandle<R>) -> Result<TokenMeterState, String> {
    let identifier = &app.config().identifier;
    let home_dir = app.path().home_dir().map_err(|error| error.to_string())?;
    let local_data_dir = app
        .path()
        .local_data_dir()
        .map_err(|error| error.to_string())?;
    let isolated_data_dir = updater_qa_data_directory(identifier)?;
    let is_updater_qa = isolated_data_dir.is_some();
    if isolated_data_dir
        .as_ref()
        .is_some_and(|path| crate::platform_paths_equal(path, &local_data_dir.join("TokenMeter")))
    {
        return Err("Updater QA local data isolation is unavailable".to_owned());
    }
    let data_dir = isolated_data_dir
        .clone()
        .unwrap_or_else(|| local_data_dir.join("TokenMeter"));
    let state = TokenMeterState {
        operation: Mutex::new(()),
        dashboard_data: Arc::new(Mutex::new(DashboardData {
            scan: None,
            account: DashboardAccountState::Updating(None),
            account_generation: 0,
        })),
        cleanup_plan: Mutex::new(None),
        data_dir,
        source_isolation_root: isolated_data_dir.map(|path| path.join("sources")),
        #[cfg(target_os = "macos")]
        legacy_preferences: (!crate::is_token_meter_updater_qa(identifier)).then(|| {
            home_dir.join(format!(
                "Library/Preferences/{}.plist",
                crate::TOKEN_METER_IDENTIFIER
            ))
        }),
        #[cfg(not(target_os = "macos"))]
        legacy_preferences: None,
        codex_home_env: std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && !is_updater_qa),
        account_executable: None,
        home_dir,
        local_device_name: local_device_name(),
    };
    state.with_lock(|state| state.load_settings().map(|_| ()))?;
    Ok(state)
}

pub(crate) fn updater_qa_data_directory(identifier: &str) -> Result<Option<PathBuf>, String> {
    if let Some(root) = crate::windows_updater_qa_root(identifier)? {
        return Ok(Some(root.join("TokenMeter")));
    }
    isolated_data_directory(
        identifier,
        option_env!("TOKEN_METER_UPDATER_QA_ROOT").map(PathBuf::from),
    )
}

fn isolated_data_directory(
    identifier: &str,
    configured_root: Option<PathBuf>,
) -> Result<Option<PathBuf>, String> {
    if !crate::is_token_meter_updater_qa(identifier) {
        return Ok(None);
    }
    let root = configured_root
        .filter(|path| path.is_absolute() && path.file_name() == Some(OsStr::new(identifier)))
        .ok_or_else(|| "Updater QA requires an isolated local data root".to_owned())?;
    if root.parent().is_none() {
        return Err("Updater QA local data isolation is unavailable".to_owned());
    }
    Ok(Some(root))
}

#[tauri::command(async)]
pub fn token_meter_dashboard(
    state: State<'_, TokenMeterState>,
    request: DashboardRequest,
    refresh: bool,
) -> Result<DashboardSnapshotDto, String> {
    state.with_lock(|state| state.dashboard(&request, refresh))
}

#[tauri::command(async)]
pub fn token_meter_rebuild_cache(
    state: State<'_, TokenMeterState>,
) -> Result<RebuildCacheResult, String> {
    state.with_lock(|state| {
        let settings = state.load_settings()?;
        let event_count = state.refresh_dashboard_data(&settings, true)?;
        Ok(RebuildCacheResult { event_count })
    })
}

#[tauri::command(async)]
pub fn token_meter_set_sync_folder(
    state: State<'_, TokenMeterState>,
    path: Option<String>,
) -> Result<DashboardSettings, String> {
    state.with_lock(|state| {
        if state.source_isolation_root.is_some() {
            return Err("Source configuration is disabled in updater QA.".to_owned());
        }
        state.with_settings(|settings| {
            let path = checked_path(path, PathKind::Directory)?;
            if let Some(path) = path.as_deref() {
                state.prepare_icloud_sync_folder_if_default(Path::new(path))?;
            }
            settings.sync_folder_path = path;
            settings
                .save(&state.settings_path())
                .map_err(string_error)?;
            state.invalidate_dashboard_data()?;
            Ok(state.dashboard_settings(settings))
        })
    })
}

#[tauri::command(async)]
pub fn token_meter_set_source_paths(
    state: State<'_, TokenMeterState>,
    paths: SourcePaths,
) -> Result<DashboardSettings, String> {
    state.with_lock(|state| {
        if state.source_isolation_root.is_some() {
            return Err("Source configuration is disabled in updater QA.".to_owned());
        }
        state.with_settings(|settings| {
            settings.codex_home = checked_path(paths.codex_home, PathKind::Directory)?;
            settings.claude_projects_path =
                checked_path(paths.claude_projects_path, PathKind::Directory)?;
            settings.hermes_database_path =
                checked_path(paths.hermes_database_path, PathKind::File)?;
            settings.codex_executable_path = checked_executable_path(paths.codex_executable_path)?;
            settings
                .save(&state.settings_path())
                .map_err(string_error)?;
            state.invalidate_dashboard_data()?;
            Ok(state.dashboard_settings(settings))
        })
    })
}

#[tauri::command(async)]
pub fn token_meter_set_show_full_numbers(
    state: State<'_, TokenMeterState>,
    value: bool,
) -> Result<DashboardSettings, String> {
    state.with_lock(|state| {
        state.with_settings(|settings| {
            settings.show_full_token_numbers = value;
            settings
                .save(&state.settings_path())
                .map_err(string_error)?;
            Ok(state.dashboard_settings(settings))
        })
    })
}

#[tauri::command(async)]
pub fn token_meter_cleanup_preview(
    state: State<'_, TokenMeterState>,
    older_than_days: u32,
) -> Result<CleanupPreviewResult, String> {
    state.with_lock(|state| state.cleanup_preview(older_than_days))
}

#[tauri::command(async)]
pub fn token_meter_cleanup_apply(
    state: State<'_, TokenMeterState>,
    plan_id: Uuid,
) -> Result<CleanupApplyResult, String> {
    state.with_lock(|state| state.cleanup_apply(plan_id))
}

impl TokenMeterState {
    fn with_lock<T>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = self.lock()?;
        operation(self)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, String> {
        self.operation
            .lock()
            .map_err(|_| "TokenMeter operation lock is poisoned".to_owned())
    }

    fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

    fn cache_path(&self) -> PathBuf {
        self.data_dir.join("TokenMeter.sqlite")
    }

    fn load_settings(&self) -> Result<TokenMeterSettings, String> {
        self.with_settings(|settings| Ok(settings.clone()))
    }

    fn with_settings<T>(
        &self,
        operation: impl FnOnce(&mut TokenMeterSettings) -> Result<T, String>,
    ) -> Result<T, String> {
        fs::create_dir_all(&self.data_dir).map_err(string_error)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.data_dir.join("settings.lock"))
            .map_err(string_error)?;
        lock.lock().map_err(string_error)?;
        let mut settings = TokenMeterSettings::load_or_import(
            &self.settings_path(),
            self.legacy_preferences.as_deref(),
            &Uuid::new_v4().to_string(),
        )
        .map_err(string_error)?;
        operation(&mut settings)
    }

    fn dashboard(
        &self,
        request: &DashboardRequest,
        refresh: bool,
    ) -> Result<DashboardSnapshotDto, String> {
        let settings = self.load_settings()?;
        let missing = self
            .dashboard_data
            .lock()
            .map_err(|_| "TokenMeter dashboard cache lock is poisoned".to_owned())?
            .scan
            .is_none();
        if refresh || missing {
            self.refresh_dashboard_data(&settings, false)?;
        }
        let data = self
            .dashboard_data
            .lock()
            .map_err(|_| "TokenMeter dashboard cache lock is poisoned".to_owned())?;
        let mut dashboard = compose_dashboard(
            request,
            data.scan.as_ref().expect("dashboard data was refreshed"),
            &settings,
            &self.local_device_name,
            Some(&data.account),
            Utc::now(),
            &Local,
            current_first_weekday()?,
        )
        .map_err(string_error)?;
        dashboard.settings.icloud_sync_folder_path = self.icloud_sync_folder_path();
        Ok(DashboardSnapshotDto::from(dashboard))
    }

    fn refresh_dashboard_data(
        &self,
        settings: &TokenMeterSettings,
        rebuild_cache: bool,
    ) -> Result<usize, String> {
        let scan = self.scan(settings, rebuild_cache)?;
        let event_count = scan.events.len();
        let mut data = self
            .dashboard_data
            .lock()
            .map_err(|_| "TokenMeter dashboard cache lock is poisoned".to_owned())?;
        data.account_generation = data.account_generation.wrapping_add(1);
        let generation = data.account_generation;
        let previous = match &data.account {
            DashboardAccountState::Available(usage)
            | DashboardAccountState::Updating(Some(usage)) => Some(usage.clone()),
            DashboardAccountState::Updating(None) | DashboardAccountState::Unavailable(_) => None,
        };
        data.scan = Some(scan);
        if self.source_isolation_root.is_some() {
            data.account = DashboardAccountState::Unavailable(
                "Codex account access is disabled in updater QA.".to_owned(),
            );
            return Ok(event_count);
        }
        data.account = DashboardAccountState::Updating(previous);
        drop(data);

        let executable = self.account_executable(settings).map(OsStr::to_os_string);
        let dashboard_data = Arc::clone(&self.dashboard_data);
        thread::spawn(move || {
            let account = fetch_codex_account_usage(executable.as_deref(), Duration::from_secs(15))
                .map(DashboardAccountState::Available)
                .unwrap_or_else(|_| {
                    DashboardAccountState::Unavailable(
                "Codex account status is unavailable. Check that Codex is installed and signed in."
                    .to_owned(),
            )
                });
            update_account_if_current(&dashboard_data, generation, account);
        });
        Ok(event_count)
    }

    fn invalidate_dashboard_data(&self) -> Result<(), String> {
        let mut data = self
            .dashboard_data
            .lock()
            .map_err(|_| "TokenMeter dashboard cache lock is poisoned".to_owned())?;
        data.scan = None;
        data.account_generation = data.account_generation.wrapping_add(1);
        Ok(())
    }

    fn scan(
        &self,
        settings: &TokenMeterSettings,
        rebuild_cache: bool,
    ) -> Result<token_meter_core::models::ScanResult, String> {
        let cache = TokenEventCache::open_or_create(&self.cache_path()).map_err(string_error)?;
        let local_device = TokenDeviceMetadata::new(
            settings.local_device_id.clone(),
            self.local_device_name.clone(),
        );
        let scanner = TokenLogScanner::new(
            self.scanner_roots(settings),
            local_device.clone(),
            Some(&cache),
        );
        if rebuild_cache {
            scanner.clear_cache().map_err(string_error)?;
        }
        let mut scan = scanner.scan(None, None, || false);
        if self.source_isolation_root.is_none()
            && let Some(folder) = settings.sync_folder_path.as_deref()
        {
            let scan_complete = scan.parse_error_count == 0;
            let outcome = TokenSyncStore::new(folder, local_device.clone()).synchronize(
                if scan_complete { &scan.events } else { &[] },
                rebuild_cache && scan_complete,
                None,
                || false,
            );
            let mut outcome = outcome;
            if !scan_complete {
                outcome.status.export_error = Some(
                    "Local sync export was skipped because token source scanning was incomplete."
                        .to_owned(),
                );
            }
            scan.events = merge_local_and_sync(scan.events, outcome.events);
            scan.sync_status = outcome.status;
            scan.sync_devices = devices(&scan.events);
        }
        Ok(scan)
    }

    fn scanner_roots(&self, settings: &TokenMeterSettings) -> ScannerRoots {
        if let Some(root) = &self.source_isolation_root {
            let codex_home = root.join("codex");
            return ScannerRoots {
                codex_sessions: Some(codex_home.join("sessions")),
                codex_archive: Some(codex_home.join("archived_sessions")),
                claude_projects: Some(root.join("claude/projects")),
                hermes_database: Some(root.join("hermes/state.db")),
            };
        }
        let codex_home = self.codex_home(settings);
        ScannerRoots {
            codex_sessions: codex_home.as_ref().map(|path| path.join("sessions")),
            codex_archive: codex_home
                .as_ref()
                .map(|path| path.join("archived_sessions")),
            claude_projects: settings
                .claude_projects_path
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| self.default_source_path(".claude/projects")),
            hermes_database: settings
                .hermes_database_path
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| self.default_source_path(".hermes/state.db")),
        }
    }

    fn codex_home(&self, settings: &TokenMeterSettings) -> Option<PathBuf> {
        if let Some(root) = &self.source_isolation_root {
            return Some(root.join("codex"));
        }
        settings
            .codex_home
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| self.codex_home_env.clone())
            .or_else(|| self.default_source_path(".codex"))
    }

    fn default_source_path(&self, relative: &str) -> Option<PathBuf> {
        Some(self.home_dir.join(relative))
    }

    fn cleanup_preview(&self, retention_days: u32) -> Result<CleanupPreviewResult, String> {
        let settings = self.load_settings()?;
        let (_codex_home, evidence, sync_keys) = self.cleanup_proofs(&settings)?;
        let plan = plan_codex_session_cleanup(&evidence, &sync_keys, retention_days, Utc::now());
        let result = CleanupPreviewResult {
            plan_id: Uuid::new_v4(),
            candidate_count: plan.eligible_file_count(),
            total_bytes: plan.eligible_byte_count().to_string(),
        };
        let mut pending = self
            .cleanup_plan
            .lock()
            .map_err(|_| "TokenMeter cleanup plan lock is poisoned".to_owned())?;
        *pending = Some(PendingCleanup {
            id: result.plan_id,
            plan,
        });
        Ok(result)
    }

    fn cleanup_apply(&self, plan_id: Uuid) -> Result<CleanupApplyResult, String> {
        let mut plans = self
            .cleanup_plan
            .lock()
            .map_err(|_| "TokenMeter cleanup plan lock is poisoned".to_owned())?;
        if plans.as_ref().is_none_or(|pending| pending.id != plan_id) {
            return Err("Cleanup preview expired; preview again before archiving.".to_owned());
        }
        let pending = plans.take().expect("matching cleanup plan exists");
        drop(plans);
        let settings = self.load_settings()?;
        let (codex_home, evidence, sync_keys) = self.cleanup_proofs(&settings)?;
        let request = pending
            .plan
            .archive_request(&evidence, &sync_keys)
            .map_err(string_error)?;
        let destination = cleanup_archive_path(&codex_home, pending.plan.retention_days);
        let inspection = create_cleanup_archive(&codex_home, &destination, &request, &evidence)
            .map_err(string_error)?;
        let authorization = pending
            .plan
            .authorize_removal(&evidence, &sync_keys, &inspection)
            .map_err(string_error)?;
        let result = remove_cleanup_sources(&codex_home, &destination, &evidence, &authorization)
            .map_err(string_error)?;
        let scan = self.scan(&settings, false)?;
        self.dashboard_data
            .lock()
            .map_err(|_| "TokenMeter dashboard cache lock is poisoned".to_owned())?
            .scan = Some(scan);
        Ok(CleanupApplyResult {
            archived_count: result.archived_file_count,
        })
    }

    fn cleanup_proofs(
        &self,
        settings: &TokenMeterSettings,
    ) -> Result<(PathBuf, Vec<CodexSessionEvidence>, BTreeSet<String>), String> {
        if self.source_isolation_root.is_some() {
            return Err("Session cleanup is disabled in updater QA.".to_owned());
        }
        let sync_folder = settings
            .sync_folder_path
            .as_deref()
            .ok_or_else(|| "Choose a sync folder before cleaning Codex sessions.".to_owned())?;
        if !Path::new(sync_folder).is_dir() {
            return Err("The configured sync folder is unavailable.".to_owned());
        }
        let codex_home = self.codex_home(settings).ok_or_else(|| {
            "Configure an absolute Codex home before cleaning sessions.".to_owned()
        })?;
        let codex_home = fs::canonicalize(codex_home)
            .map_err(|_| "The configured Codex home is unavailable.".to_owned())?;
        let scan = self.scan(settings, false)?;
        if !scan.sync_status.exists
            || scan.sync_status.export_error.is_some()
            || scan.sync_status.parse_error_count != 0
        {
            return Err("Sync verification failed; no Codex sessions were changed.".to_owned());
        }
        let sync_keys = scan
            .events
            .iter()
            .map(|event| format!("{}|{}", event.device_id, event.id))
            .collect();
        let cache = TokenEventCache::open_or_create(&self.cache_path()).map_err(string_error)?;
        let files = TokenLogScanner::new(
            ScannerRoots {
                codex_sessions: Some(codex_home.join("sessions")),
                codex_archive: Some(codex_home.join("archived_sessions")),
                ..ScannerRoots::default()
            },
            TokenDeviceMetadata::new(
                settings.local_device_id.clone(),
                self.local_device_name.clone(),
            ),
            None,
        )
        .find_codex_files();
        let evidence = files
            .into_iter()
            .map(|path| cleanup_evidence(&codex_home, &cache, &path))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((codex_home, evidence, sync_keys))
    }

    fn dashboard_settings(&self, settings: &TokenMeterSettings) -> DashboardSettings {
        DashboardSettings {
            show_full_token_numbers: settings.show_full_token_numbers,
            sync_folder_path: settings.sync_folder_path.clone(),
            icloud_sync_folder_path: self.icloud_sync_folder_path(),
            local_device_id: settings.local_device_id.clone(),
            local_device_name: self.local_device_name.clone(),
            codex_home: settings.codex_home.clone(),
            claude_projects_path: settings.claude_projects_path.clone(),
            hermes_database_path: settings.hermes_database_path.clone(),
            codex_executable_path: settings.codex_executable_path.clone(),
        }
    }

    fn icloud_sync_folder_path(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            if self.source_isolation_root.is_some() {
                return None;
            }
            let home = fs::canonicalize(&self.home_dir).ok()?;
            let mut root = self.home_dir.clone();
            for component in ["Library", "Mobile Documents", "com~apple~CloudDocs"] {
                root.push(component);
                let metadata = fs::symlink_metadata(&root).ok()?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return None;
                }
            }
            let canonical_root = fs::canonicalize(&root).ok()?;
            if !canonical_root.starts_with(home) {
                return None;
            }
            let folder = root.join("TokenMeter");
            match fs::symlink_metadata(&folder) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return None,
            }
            folder.into_os_string().into_string().ok()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn prepare_icloud_sync_folder_if_default(&self, path: &Path) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let expected = self
                .home_dir
                .join("Library/Mobile Documents/com~apple~CloudDocs/TokenMeter");
            if path != expected {
                return Ok(());
            }
            if self.icloud_sync_folder_path().as_deref() != expected.to_str() {
                return Err("The iCloud Drive sync folder is unavailable or unsafe.".to_owned());
            }
            match fs::create_dir(&expected) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.to_string()),
            }
            if self.icloud_sync_folder_path().as_deref() != expected.to_str() {
                return Err("The iCloud Drive sync folder is unavailable or unsafe.".to_owned());
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = path;
        Ok(())
    }

    fn account_executable<'a>(&'a self, settings: &'a TokenMeterSettings) -> Option<&'a OsStr> {
        settings
            .codex_executable_path
            .as_deref()
            .map(OsStr::new)
            .or(self.account_executable.as_deref())
    }
}

fn update_account_if_current(
    dashboard_data: &Mutex<DashboardData>,
    generation: u64,
    account: DashboardAccountState,
) -> bool {
    let Ok(mut data) = dashboard_data.lock() else {
        return false;
    };
    if data.account_generation != generation {
        return false;
    }
    data.account = account;
    true
}

#[derive(Clone, Copy)]
enum PathKind {
    Directory,
    File,
}

fn checked_path(value: Option<String>, kind: PathKind) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let path = Path::new(&value);
    if !path.is_absolute() {
        return Err("path must be absolute".to_owned());
    }
    if path.exists()
        && match kind {
            PathKind::Directory => !path.is_dir(),
            PathKind::File => !path.is_file(),
        }
    {
        return Err(match kind {
            PathKind::Directory => "path must identify a directory",
            PathKind::File => "path must identify a file",
        }
        .to_owned());
    }
    Ok(Some(value))
}

fn checked_executable_path(value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let path = Path::new(&value);
    if !path.is_absolute() {
        return Err("Codex executable path must be absolute".to_owned());
    }
    let metadata = fs::metadata(path).map_err(|_| {
        "Codex executable path must identify an existing executable file".to_owned()
    })?;
    if !metadata.is_file() {
        return Err("Codex executable path must identify an existing executable file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(
                "Codex executable path must identify an existing executable file".to_owned(),
            );
        }
    }
    Ok(Some(value))
}

fn cleanup_evidence(
    codex_home: &Path,
    cache: &TokenEventCache,
    path: &Path,
) -> Result<CodexSessionEvidence, String> {
    let canonical = fs::canonicalize(path).map_err(string_error)?;
    let relative = canonical.strip_prefix(codex_home).map_err(|_| {
        "Refusing to inspect a Codex session outside the configured home.".to_owned()
    })?;
    let snapshot = cleanup_source_snapshot(codex_home, relative).map_err(string_error)?;
    let origin_path = canonical.to_string_lossy();
    let cached_event_keys = cache
        .origin_file(OriginKind::LocalLog, &origin_path)
        .map_err(string_error)?
        .filter(|origin| origin.source.as_deref() == Some("codex") && !origin.parse_error)
        .map(|_| {
            cache
                .events_for_origin(OriginKind::LocalLog, &origin_path)
                .map(|records| {
                    records
                        .into_iter()
                        .map(|record| format!("{}|{}", record.device_id, record.event_id))
                        .collect()
                })
        })
        .transpose()
        .map_err(string_error)?;
    Ok(CodexSessionEvidence {
        snapshot,
        cached_event_keys,
    })
}

fn cleanup_archive_path(codex_home: &Path, retention_days: u32) -> PathBuf {
    codex_home.join("session_archives").join(format!(
        "codex-sessions-older-than-{}d-{}-{}.zip",
        retention_days.max(1),
        Utc::now().format("%Y%m%d-%H%M%S"),
        Uuid::new_v4()
    ))
}

fn devices(events: &[token_meter_core::models::TokenEvent]) -> Vec<TokenDeviceMetadata> {
    events
        .iter()
        .map(|event| (event.device_id.clone(), event.device_name.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .map(|(id, name)| TokenDeviceMetadata::new(id, name))
        .collect()
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(target_os = "macos")]
fn local_device_name() -> String {
    use std::{ffi::CStr, ffi::c_char, ffi::c_void};

    const UTF8: u32 = 0x0800_0100;

    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCopyComputerName(
            store: *const c_void,
            name_encoding: *mut u32,
        ) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetLength(value: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            value: *const c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: *const c_void);
    }

    // SAFETY: NULL requests the documented temporary dynamic-store session.
    let name = unsafe { SCDynamicStoreCopyComputerName(std::ptr::null(), std::ptr::null_mut()) };
    if name.is_null() {
        return "This Mac".to_owned();
    }
    let decoded = (|| {
        // SAFETY: `name` is a live CFString returned above.
        let length = unsafe { CFStringGetLength(name) };
        // SAFETY: uses the valid CFString length and a public CoreFoundation encoding constant.
        let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8) };
        let buffer_size = maximum.checked_add(1)?;
        let mut buffer: Vec<c_char> = vec![0; usize::try_from(buffer_size).ok()?];
        // SAFETY: the buffer has the exact size passed to CoreFoundation and remains live.
        if unsafe { CFStringGetCString(name, buffer.as_mut_ptr(), buffer_size, UTF8) } == 0 {
            return None;
        }
        // SAFETY: CFStringGetCString guarantees a terminating NUL on success.
        let value = unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_str()
            .ok()?
            .to_owned();
        (!value.is_empty()).then_some(value)
    })();
    // SAFETY: balances the retained result from SCDynamicStoreCopyComputerName.
    unsafe { CFRelease(name) };
    decoded.unwrap_or_else(|| "This Mac".to_owned())
}

#[cfg(target_os = "windows")]
fn local_device_name() -> String {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };

    let mut size = 0_u32;
    // SAFETY: the documented sizing call accepts a NULL output buffer.
    unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if size == 0 {
        return "This PC".to_owned();
    }
    let mut buffer = vec![0_u16; size as usize];
    // SAFETY: the buffer capacity is the size returned by the sizing call.
    let succeeded = unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            buffer.as_mut_ptr(),
            &raw mut size,
        )
    } != 0;
    if !succeeded {
        return "This PC".to_owned();
    }
    buffer.truncate(size as usize);
    String::from_utf16(&buffer)
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "This PC".to_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn local_device_name() -> String {
    "This device".to_owned()
}

#[cfg(target_os = "macos")]
fn current_first_weekday() -> Result<Weekday, String> {
    use std::ffi::c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFCalendarCopyCurrent() -> *const c_void;
        fn CFCalendarGetFirstWeekday(calendar: *const c_void) -> isize;
        fn CFRelease(value: *const c_void);
    }

    // SAFETY: CoreFoundation returns a retained CFCalendar; it is checked and released once.
    let calendar = unsafe { CFCalendarCopyCurrent() };
    if calendar.is_null() {
        return Err("current calendar is unavailable".to_owned());
    }
    // SAFETY: `calendar` is a live CFCalendar returned above.
    let first = unsafe { CFCalendarGetFirstWeekday(calendar) };
    // SAFETY: balances the retained result from CFCalendarCopyCurrent.
    unsafe { CFRelease(calendar) };
    match first {
        1 => Ok(Weekday::Sun),
        2 => Ok(Weekday::Mon),
        3 => Ok(Weekday::Tue),
        4 => Ok(Weekday::Wed),
        5 => Ok(Weekday::Thu),
        6 => Ok(Weekday::Fri),
        7 => Ok(Weekday::Sat),
        _ => Err(format!(
            "current calendar returned invalid first weekday {first}"
        )),
    }
}

#[cfg(target_os = "windows")]
fn current_first_weekday() -> Result<Weekday, String> {
    use windows_sys::Win32::Globalization::{
        GetLocaleInfoEx, LOCALE_IFIRSTDAYOFWEEK, LOCALE_RETURN_NUMBER,
    };

    let mut first = 0_u32;
    // SAFETY: a DWORD is the documented output buffer for LOCALE_RETURN_NUMBER.
    let result = unsafe {
        GetLocaleInfoEx(
            std::ptr::null(),
            LOCALE_IFIRSTDAYOFWEEK | LOCALE_RETURN_NUMBER,
            (&raw mut first).cast(),
            2,
        )
    };
    if result == 0 {
        return Err("current locale first weekday is unavailable".to_owned());
    }
    match first {
        0 => Ok(Weekday::Mon),
        1 => Ok(Weekday::Tue),
        2 => Ok(Weekday::Wed),
        3 => Ok(Weekday::Thu),
        4 => Ok(Weekday::Fri),
        5 => Ok(Weekday::Sat),
        6 => Ok(Weekday::Sun),
        _ => Err(format!(
            "current locale returned invalid first weekday {first}"
        )),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn current_first_weekday() -> Result<Weekday, String> {
    Ok(Weekday::Mon)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::{self, RecvTimeoutError},
        thread,
        time::Duration,
    };

    use super::*;

    fn state(root: &Path) -> TokenMeterState {
        TokenMeterState {
            operation: Mutex::new(()),
            dashboard_data: Arc::new(Mutex::new(DashboardData {
                scan: None,
                account: DashboardAccountState::Updating(None),
                account_generation: 0,
            })),
            cleanup_plan: Mutex::new(None),
            data_dir: root.join("TokenMeter"),
            source_isolation_root: None,
            home_dir: root.join("home"),
            legacy_preferences: None,
            codex_home_env: None,
            account_executable: Some(root.join("missing-codex").into_os_string()),
            local_device_name: "Test device".into(),
        }
    }

    #[test]
    fn filter_dashboard_uses_cached_scan_until_refresh_is_requested() {
        use token_meter_core::{dashboard::DashboardFilters, models::TokenSource};

        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());
        state.dashboard_data.lock().unwrap().scan = Some(ScanResult::default());
        let session = state.home_dir.join(".codex/sessions/2026/session.jsonl");
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        fs::write(
            session,
            concat!(
                "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",",
                "\"payload\":{\"cwd\":\"/tmp/project\",\"model\":\"gpt\",",
                "\"info\":{\"last_token_usage\":{\"input_tokens\":10,\"total_tokens\":10},",
                "\"total_token_usage\":{\"input_tokens\":10,\"total_tokens\":10}}}}\n"
            ),
        )
        .unwrap();
        let request = DashboardRequest {
            source: TokenSource::All,
            range: "All".into(),
            bucket: "auto".into(),
            filters: DashboardFilters::default(),
        };

        assert_eq!(state.dashboard(&request, false).unwrap().event_count, 0);
        let refreshed = state.dashboard(&request, true).unwrap();
        assert_eq!(refreshed.event_count, 1);
    }

    #[test]
    fn stale_account_worker_cannot_replace_the_current_generation() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());
        state.dashboard_data.lock().unwrap().account_generation = 2;

        assert!(!update_account_if_current(
            &state.dashboard_data,
            1,
            DashboardAccountState::Unavailable("stale".into()),
        ));
        assert!(matches!(
            &state.dashboard_data.lock().unwrap().account,
            DashboardAccountState::Updating(None)
        ));
        assert!(update_account_if_current(
            &state.dashboard_data,
            2,
            DashboardAccountState::Unavailable("current".into()),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refresh_returns_while_slow_account_worker_is_running() {
        use std::{os::unix::fs::PermissionsExt, time::Instant};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("slow-codex");
        fs::write(&executable, "#!/bin/sh\nsleep 2\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        let mut state = state(directory.path());
        state.account_executable = Some(executable.into_os_string());
        let settings = state.load_settings().unwrap();

        let started = Instant::now();
        state.refresh_dashboard_data(&settings, false).unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            &state.dashboard_data.lock().unwrap().account,
            DashboardAccountState::Updating(None)
        ));
        let deadline = Instant::now() + Duration::from_secs(4);
        while matches!(
            &state.dashboard_data.lock().unwrap().account,
            DashboardAccountState::Updating(_)
        ) {
            assert!(Instant::now() < deadline, "account worker did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn blank_source_paths_use_the_verified_home_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());
        let settings = state.load_settings().unwrap();
        let home = directory.path().join("home");
        let roots = state.scanner_roots(&settings);

        assert_eq!(state.codex_home(&settings), Some(home.join(".codex")));
        assert_eq!(roots.codex_sessions, Some(home.join(".codex/sessions")));
        assert_eq!(
            roots.codex_archive,
            Some(home.join(".codex/archived_sessions"))
        );
        assert_eq!(roots.claude_projects, Some(home.join(".claude/projects")));
        assert_eq!(roots.hermes_database, Some(home.join(".hermes/state.db")));
    }

    #[test]
    fn icloud_sync_folder_is_platform_verified_before_creation() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());

        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(state.icloud_sync_folder_path(), None);
            let path = directory.path().join("iCloud/TokenMeter");
            state.prepare_icloud_sync_folder_if_default(&path).unwrap();
            assert!(!path.exists());
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::symlink;

            let mut state = state;
            let root = state
                .home_dir
                .join("Library/Mobile Documents/com~apple~CloudDocs");
            let expected = root.join("TokenMeter");
            assert_eq!(state.icloud_sync_folder_path(), None);
            fs::create_dir_all(&root).unwrap();
            assert_eq!(
                state.icloud_sync_folder_path().as_deref(),
                expected.to_str()
            );

            let other = directory.path().join("other");
            state.prepare_icloud_sync_folder_if_default(&other).unwrap();
            assert!(!other.exists());

            fs::write(&expected, b"not a directory").unwrap();
            assert_eq!(state.icloud_sync_folder_path(), None);
            assert!(
                state
                    .prepare_icloud_sync_folder_if_default(&expected)
                    .is_err()
            );
            fs::remove_file(&expected).unwrap();

            state
                .prepare_icloud_sync_folder_if_default(&expected)
                .unwrap();
            assert!(expected.is_dir());
            fs::remove_dir(&expected).unwrap();

            let outside = directory.path().join("outside");
            fs::create_dir(&outside).unwrap();
            symlink(&outside, &expected).unwrap();
            assert_eq!(state.icloud_sync_folder_path(), None);
            assert!(
                state
                    .prepare_icloud_sync_folder_if_default(&expected)
                    .is_err()
            );
            fs::remove_file(&expected).unwrap();

            state.source_isolation_root = Some(directory.path().join("updater-qa"));
            assert_eq!(state.icloud_sync_folder_path(), None);
            assert!(
                state
                    .prepare_icloud_sync_folder_if_default(&expected)
                    .is_err()
            );
            state.source_isolation_root = None;

            fs::remove_dir(&root).unwrap();
            let mobile_documents = root.parent().unwrap();
            fs::remove_dir(mobile_documents).unwrap();
            let escaped_mobile_documents = directory.path().join("outside-mobile-documents");
            fs::create_dir_all(escaped_mobile_documents.join("com~apple~CloudDocs")).unwrap();
            symlink(&escaped_mobile_documents, mobile_documents).unwrap();
            assert_eq!(state.icloud_sync_folder_path(), None);
            assert!(
                state
                    .prepare_icloud_sync_folder_if_default(&expected)
                    .is_err()
            );
            assert!(
                !escaped_mobile_documents
                    .join("com~apple~CloudDocs/TokenMeter")
                    .exists()
            );
        }
    }

    #[test]
    fn updater_qa_uses_only_identifier_scoped_data_and_source_roots() {
        let directory = tempfile::tempdir().unwrap();
        let local_data = directory.path().join("Library/Application Support");
        let identifier = format!(
            "{}0123456789abcdef01234567",
            crate::TOKEN_METER_UPDATER_QA_IDENTIFIER_PREFIX
        );
        let isolated = local_data.join(&identifier);
        assert_eq!(
            isolated_data_directory(&identifier, Some(isolated.clone())).unwrap(),
            Some(isolated.clone())
        );
        assert!(isolated_data_directory(&identifier, None).is_err());
        assert!(isolated_data_directory(&identifier, Some(PathBuf::from("relative"))).is_err());
        assert!(isolated_data_directory(&identifier, Some(local_data.join("wrong"))).is_err());
        assert_eq!(
            isolated_data_directory(crate::TOKEN_METER_IDENTIFIER, Some(isolated.clone())).unwrap(),
            None
        );

        let mut state = state(directory.path());
        state.data_dir = isolated.clone();
        state.source_isolation_root = Some(isolated.join("sources"));
        let mut settings = state.load_settings().unwrap();
        settings.codex_home = Some("/production/codex".into());
        settings.claude_projects_path = Some("/production/claude".into());
        settings.hermes_database_path = Some("/production/hermes.db".into());
        settings.sync_folder_path = Some("/production/sync".into());

        let source_root = state.source_isolation_root.as_ref().unwrap();
        let roots = state.scanner_roots(&settings);
        assert_eq!(state.codex_home(&settings), Some(source_root.join("codex")));
        assert_eq!(
            roots.codex_sessions,
            Some(source_root.join("codex/sessions"))
        );
        assert_eq!(
            roots.claude_projects,
            Some(source_root.join("claude/projects"))
        );
        assert_eq!(
            roots.hermes_database,
            Some(source_root.join("hermes/state.db"))
        );
        assert!(state.cleanup_proofs(&settings).is_err());
        assert!(
            state
                .scan(&settings, false)
                .unwrap()
                .sync_status
                .path
                .is_none()
        );
        assert!(state.cache_path().starts_with(isolated));
    }

    #[test]
    fn empty_host_uses_only_injected_paths_and_returns_wire_counts() {
        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());
        let request: DashboardRequest = serde_json::from_value(serde_json::json!({
            "source": "all",
            "range": "8h",
            "bucket": "auto",
            "filters": {}
        }))
        .unwrap();

        let dashboard = state
            .with_lock(|state| state.dashboard(&request, false))
            .unwrap();

        assert_eq!(dashboard.total.total, "0");
        assert_eq!(
            state.settings_path(),
            directory.path().join("TokenMeter/settings.json")
        );
        assert_eq!(
            state.cache_path(),
            directory.path().join("TokenMeter/TokenMeter.sqlite")
        );
    }

    #[test]
    fn setters_reject_relative_or_wrong_kind_paths() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        let spaced_directory = directory.path().join(" spaced ");
        std::fs::write(&file, b"test").unwrap();
        std::fs::create_dir(&spaced_directory).unwrap();

        assert!(checked_path(Some("relative".into()), PathKind::Directory).is_err());
        assert!(
            checked_path(
                Some(file.to_string_lossy().into_owned()),
                PathKind::Directory
            )
            .is_err()
        );
        let spaced = spaced_directory.to_string_lossy().into_owned();
        assert_eq!(
            checked_path(Some(spaced.clone()), PathKind::Directory).unwrap(),
            Some(spaced)
        );
        assert_eq!(
            checked_path(Some(String::new()), PathKind::File).unwrap(),
            None
        );
    }

    #[test]
    fn executable_path_preserves_stable_symlink_and_controls_account_selection() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join(" codex target ");
        let link = directory.path().join("codex-link");
        fs::write(&target, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(not(unix))]
        fs::copy(&target, &link).unwrap();

        let stable = link.to_string_lossy().into_owned();
        let stored = checked_executable_path(Some(stable.clone())).unwrap();
        assert_eq!(stored.as_deref(), Some(stable.as_str()));

        let old_target = directory.path().join("old-target");
        fs::rename(&target, old_target).unwrap();
        fs::write(&target, b"upgraded fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            checked_executable_path(Some(stable.clone())).unwrap(),
            Some(stable.clone())
        );
        assert!(checked_executable_path(Some("relative".into())).is_err());
        assert!(
            checked_executable_path(Some(directory.path().to_string_lossy().into_owned())).is_err()
        );
        assert!(
            checked_executable_path(Some(
                directory
                    .path()
                    .join("missing")
                    .to_string_lossy()
                    .into_owned()
            ))
            .is_err()
        );
        assert_eq!(checked_executable_path(Some(String::new())).unwrap(), None);
        #[cfg(unix)]
        {
            let non_executable = directory.path().join("not-executable");
            fs::write(&non_executable, b"fixture").unwrap();
            assert!(
                checked_executable_path(Some(non_executable.to_string_lossy().into_owned()))
                    .is_err()
            );
        }

        let state = state(directory.path());
        let mut settings = state.load_settings().unwrap();
        assert_eq!(
            state.account_executable(&settings),
            state.account_executable.as_deref()
        );
        settings.codex_executable_path = stored;
        assert_eq!(
            state.account_executable(&settings),
            Some(OsStr::new(&stable))
        );
    }

    #[test]
    fn settings_updates_are_serialized_across_host_instances() {
        let directory = tempfile::tempdir().unwrap();
        state(directory.path()).load_settings().unwrap();
        let first = state(directory.path());
        let second = state(directory.path());
        let first_path = first.settings_path();
        let second_path = second.settings_path();
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let first_update = thread::spawn(move || {
            first
                .with_settings(|settings| {
                    settings.show_full_token_numbers = true;
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    settings.save(&first_path).map_err(string_error)
                })
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let second_update = thread::spawn(move || {
            second
                .with_settings(|settings| {
                    settings.sync_folder_path = Some("/sync".into());
                    settings.save(&second_path).map_err(string_error)
                })
                .unwrap();
            done_tx.send(()).unwrap();
        });
        let was_blocked = matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
        release_tx.send(()).unwrap();
        first_update.join().unwrap();
        second_update.join().unwrap();

        let settings = state(directory.path()).load_settings().unwrap();
        assert!(was_blocked);
        assert!(settings.show_full_token_numbers);
        assert_eq!(settings.sync_folder_path.as_deref(), Some("/sync"));
    }

    #[test]
    fn native_device_name_or_fallback_is_not_empty() {
        assert!(!local_device_name().is_empty());
    }

    #[test]
    fn incomplete_source_scan_preserves_the_local_sync_ledger_exactly() {
        use chrono::TimeZone;
        use token_meter_core::{
            models::{TokenSource, TokenUsage},
            sync::{SyncLedgerRecord, rewrite_local_ledger_v2},
        };

        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());
        let codex_home = directory.path().join("codex");
        let archive = codex_home.join("archived_sessions/valid.jsonl");
        let sync = directory.path().join("sync");
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        fs::create_dir(&sync).unwrap();
        fs::write(codex_home.join("sessions"), b"not a directory").unwrap();
        fs::write(
            &archive,
            concat!(
                "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",",
                "\"payload\":{\"cwd\":\"/tmp/project\",\"model\":\"gpt\",",
                "\"info\":{\"last_token_usage\":{\"input_tokens\":10,\"total_tokens\":10},",
                "\"total_token_usage\":{\"input_tokens\":10,\"total_tokens\":10}}}}\n"
            ),
        )
        .unwrap();

        let mut settings = state.load_settings().unwrap();
        settings.codex_home = Some(codex_home.to_string_lossy().into_owned());
        settings.sync_folder_path = Some(sync.to_string_lossy().into_owned());
        settings.save(&state.settings_path()).unwrap();
        let store = TokenSyncStore::new(
            &sync,
            TokenDeviceMetadata::new(
                settings.local_device_id.clone(),
                state.local_device_name.clone(),
            ),
        );
        rewrite_local_ledger_v2(
            &store.local_ledger_path(),
            [SyncLedgerRecord::v2(
                settings.local_device_id.clone(),
                state.local_device_name.clone(),
                "preserved-event".into(),
                Utc.with_ymd_and_hms(2025, 12, 31, 0, 0, 0).unwrap(),
                TokenSource::Codex,
                "gpt".into(),
                "/private/preserved",
                "preserved-session",
                TokenUsage {
                    input: 20,
                    total: 20,
                    ..TokenUsage::default()
                },
            )],
        )
        .unwrap();
        let before = fs::read(store.local_ledger_path()).unwrap();

        let scan = state.scan(&settings, true).unwrap();

        assert!(scan.parse_error_count > 0);
        assert_eq!(fs::read(store.local_ledger_path()).unwrap(), before);
        assert!(
            scan.sync_status
                .export_error
                .as_deref()
                .is_some_and(|error| error.contains("scanning was incomplete"))
        );
        assert!(scan.events.iter().any(|event| event.usage.total == 10));
        assert!(
            scan.events
                .iter()
                .any(|event| event.id == "preserved-event")
        );
    }

    #[test]
    fn cleanup_archives_only_a_server_synced_fixture_under_an_arbitrary_codex_home() {
        use std::time::{Duration as StdDuration, SystemTime};

        let directory = tempfile::tempdir().unwrap();
        let state = state(directory.path());
        let codex_home = directory.path().join("explicit-codex-data");
        let source = codex_home.join("sessions/2026/old.jsonl");
        let recent = codex_home.join("sessions/2026/recent.jsonl");
        let sync = directory.path().join("sync");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir(&sync).unwrap();
        fs::write(
            &source,
            concat!(
                "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",",
                "\"payload\":{\"cwd\":\"/tmp/project\",\"model\":\"gpt\",",
                "\"info\":{\"last_token_usage\":{\"input_tokens\":10,\"total_tokens\":10},",
                "\"total_token_usage\":{\"input_tokens\":10,\"total_tokens\":10}}}}\n"
            ),
        )
        .unwrap();
        fs::write(
            &recent,
            concat!(
                "{\"timestamp\":\"2026-01-02T00:00:00.000Z\",",
                "\"payload\":{\"cwd\":\"/tmp/project\",\"model\":\"gpt\",",
                "\"info\":{\"last_token_usage\":{\"input_tokens\":20,\"total_tokens\":20},",
                "\"total_token_usage\":{\"input_tokens\":20,\"total_tokens\":20}}}}\n"
            ),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::now() - StdDuration::from_secs(3 * 86_400)),
            )
            .unwrap();
        state
            .with_settings(|settings| {
                settings.codex_home = Some(codex_home.to_string_lossy().into_owned());
                settings.sync_folder_path = Some(sync.to_string_lossy().into_owned());
                settings.save(&state.settings_path()).map_err(string_error)
            })
            .unwrap();

        let preview = state.cleanup_preview(1).unwrap();
        assert_eq!(preview.candidate_count, 1);
        assert_ne!(preview.total_bytes, "0");
        assert!(state.cleanup_apply(Uuid::new_v4()).is_err());
        let result = state.cleanup_apply(preview.plan_id).unwrap();

        assert_eq!(result.archived_count, 1);
        assert!(!source.exists());
        assert!(recent.exists());
        assert_eq!(
            fs::read_dir(codex_home.join("session_archives"))
                .unwrap()
                .count(),
            1
        );
        let settings = state.load_settings().unwrap();
        assert_eq!(
            state
                .scan(&settings, false)
                .unwrap()
                .events
                .iter()
                .map(|event| event.usage.total)
                .sum::<i64>(),
            30
        );
        assert!(state.cleanup_apply(preview.plan_id).is_err());
    }
}
