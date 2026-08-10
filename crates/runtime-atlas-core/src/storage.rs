use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::CONFIGURATION_SCHEMA_VERSION;
use crate::actions::validate_custom_action;
use crate::models::{
    AppLanguage, CustomActionDefinition, RepositoryRegistration, RuntimeAtlasConfiguration,
    repository_uuid_key,
};
use crate::relations::{ProcessIdentity, UserProcessLink};

const DAMAGED_CONFIGURATION_NOTICE: &str = "The repository configuration file is damaged; an empty configuration is being used until the next save.";
const DAMAGED_SESSIONS_NOTICE: &str =
    "The command session file is damaged; running command buttons may need to be started again.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAtlasPaths {
    pub directory: PathBuf,
    pub configuration_file: PathBuf,
    pub action_sessions_file: PathBuf,
    pub action_session_markers_directory: PathBuf,
    pub process_lease_file: PathBuf,
}

impl RuntimeAtlasPaths {
    pub fn new(base_directory: impl Into<PathBuf>) -> Self {
        let directory = base_directory.into();
        Self {
            configuration_file: directory.join("configuration.json"),
            action_sessions_file: directory.join("action-sessions.json"),
            action_session_markers_directory: directory.join("session-markers"),
            process_lease_file: directory.join("runtime-atlas-process.lock"),
            directory,
        }
    }
}

#[derive(Debug, Error)]
pub enum RuntimeAtlasStorageError {
    #[error("Runtime Atlas could not create its local data directory.")]
    CannotCreateDirectory,
    #[error("Runtime Atlas local data is busy or cannot be locked.")]
    CannotLock,
    #[error("Runtime Atlas could not save local data.")]
    CannotWrite,
    #[error("{0}")]
    InvalidInput(String),
}

pub type StorageResult<T> = Result<T, RuntimeAtlasStorageError>;

pub struct RuntimeAtlasProcessLease {
    _file: File,
}

impl Drop for RuntimeAtlasProcessLease {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl RuntimeAtlasProcessLease {
    pub fn try_acquire(paths: &RuntimeAtlasPaths) -> StorageResult<Self> {
        prepare_directory(&paths.directory)?;
        let file = private_open(&paths.process_lease_file)
            .map_err(|_| RuntimeAtlasStorageError::CannotLock)?;
        file.try_lock()
            .map_err(|_| RuntimeAtlasStorageError::CannotLock)?;
        Ok(Self { _file: file })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreLoad<T> {
    pub value: T,
    pub recovery_notice: Option<String>,
}

#[derive(Clone)]
struct AtomicJsonFile<T> {
    path: PathBuf,
    empty: fn() -> T,
    damaged_notice: &'static str,
}

impl<T> AtomicJsonFile<T>
where
    T: DeserializeOwned + Serialize,
{
    fn load_checked(
        &self,
        validate: impl FnOnce(&[u8]) -> StorageResult<()>,
    ) -> StorageResult<StoreLoad<T>> {
        self.with_lock(|| {
            if !self.path.exists() {
                return Ok(StoreLoad {
                    value: (self.empty)(),
                    recovery_notice: None,
                });
            }
            let data = fs::read(&self.path).ok();
            if let Some(data) = &data {
                validate(data)?;
            }
            match data.and_then(|data| serde_json::from_slice(&data).ok()) {
                Some(value) => Ok(StoreLoad {
                    value,
                    recovery_notice: None,
                }),
                None => Ok(StoreLoad {
                    value: (self.empty)(),
                    recovery_notice: Some(self.damaged_notice.to_owned()),
                }),
            }
        })
    }

    fn update_checked(
        &self,
        validate: impl FnOnce(&[u8]) -> StorageResult<()>,
        mutation: impl FnOnce(&mut T) -> StorageResult<()>,
    ) -> StorageResult<Option<String>> {
        self.with_lock(|| {
            let mut recovery_notice = None;
            let mut document = if self.path.exists() {
                let data = fs::read(&self.path).ok();
                if let Some(data) = &data {
                    validate(data)?;
                }
                match data.and_then(|data| serde_json::from_slice(&data).ok()) {
                    Some(document) => document,
                    None => {
                        preserve_damaged_file(&self.path)?;
                        recovery_notice = Some(self.damaged_notice.to_owned());
                        (self.empty)()
                    }
                }
            } else {
                (self.empty)()
            };
            mutation(&mut document)?;
            atomic_write_json(&self.path, &document)?;
            Ok(recovery_notice)
        })
    }

    fn with_lock<R>(&self, operation: impl FnOnce() -> StorageResult<R>) -> StorageResult<R> {
        let directory = self
            .path
            .parent()
            .ok_or(RuntimeAtlasStorageError::CannotCreateDirectory)?;
        prepare_directory(directory)?;
        let mut lock_name = self.path.as_os_str().to_owned();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let lock = private_open(&lock_path).map_err(|_| RuntimeAtlasStorageError::CannotLock)?;
        lock.lock()
            .map_err(|_| RuntimeAtlasStorageError::CannotLock)?;
        operation()
    }
}

fn prepare_directory(path: &Path) -> StorageResult<()> {
    fs::create_dir_all(path).map_err(|_| RuntimeAtlasStorageError::CannotCreateDirectory)?;
    set_user_only_directory(path).map_err(|_| RuntimeAtlasStorageError::CannotCreateDirectory)?;
    Ok(())
}

fn private_open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_user_only_file(path)?;
    Ok(file)
}

fn preserve_damaged_file(path: &Path) -> StorageResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(RuntimeAtlasStorageError::CannotWrite)?;
    let backup = path.with_file_name(format!(
        "{file_name}.corrupt-{}-{}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        Uuid::new_v4()
    ));
    fs::rename(path, &backup).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
    set_user_only_file(&backup).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, document: &T) -> StorageResult<()> {
    let directory = path.parent().ok_or(RuntimeAtlasStorageError::CannotWrite)?;
    let temporary = create_temporary_file(directory, path.file_name())?;
    let (mut file, temporary_path) = temporary;
    let result = (|| {
        let value =
            serde_json::to_value(document).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        let mut data =
            serde_json::to_vec_pretty(&value).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        data.push(b'\n');
        file.write_all(&data)
            .and_then(|_| file.sync_all())
            .map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        set_user_only_file(&temporary_path).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        drop(file);
        atomic_replace(&temporary_path, path).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        set_user_only_file(path).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        sync_directory(directory).map_err(|_| RuntimeAtlasStorageError::CannotWrite)?;
        Ok(())
    })();
    if result.is_err() {
        cleanup_failed_temporary(&temporary_path, path);
    }
    result
}

fn cleanup_failed_temporary(temporary: &Path, destination: &Path) {
    // Preserve the replacement whenever the canonical name disappeared during a failed replace.
    if destination.exists() {
        let _ = fs::remove_file(temporary);
    }
}

fn create_temporary_file(
    directory: &Path,
    destination_name: Option<&std::ffi::OsStr>,
) -> StorageResult<(File, PathBuf)> {
    let name = destination_name
        .and_then(|name| name.to_str())
        .unwrap_or("runtime-atlas.json");
    for _ in 0..4 {
        let path = directory.join(format!(".{name}.tmp-{}", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(RuntimeAtlasStorageError::CannotWrite),
        }
    }
    Err(RuntimeAtlasStorageError::CannotWrite)
}

#[cfg(not(target_os = "windows"))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    if !destination.exists() {
        fs::rename(source, destination)?;
        return sync_windows_file(destination);
    }
    replace_windows_with(source, destination, |source, destination, backup| {
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let backup: Vec<u16> = backup.as_os_str().encode_wide().chain(Some(0)).collect();
        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn ReplaceFileW(
                replaced_file_name: *const u16,
                replacement_file_name: *const u16,
                backup_file_name: *const u16,
                replace_flags: u32,
                exclude: *mut std::ffi::c_void,
                reserved: *mut std::ffi::c_void,
            ) -> i32;
        }
        let replaced = unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                backup.as_ptr(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if replaced == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

#[cfg(target_os = "windows")]
fn replace_windows_with(
    source: &Path,
    destination: &Path,
    attempt: impl FnOnce(&Path, &Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let backup = windows_backup_sibling(source);
    match attempt(source, destination, &backup) {
        Ok(()) => finish_windows_replacement(destination, &backup),
        Err(error) => {
            if destination.exists()
                || (!is_windows_move_failure(error.raw_os_error()) && !backup.exists())
            {
                return Err(error);
            }
            if source.exists() && fs::rename(source, destination).is_ok() {
                return finish_windows_replacement(destination, &backup);
            }
            if backup.exists() && fs::rename(&backup, destination).is_ok() {
                let _ = sync_windows_file(destination);
            }
            Err(error)
        }
    }
}

#[cfg(target_os = "windows")]
fn finish_windows_replacement(destination: &Path, backup: &Path) -> io::Result<()> {
    sync_windows_file(destination)?;
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn sync_windows_file(path: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()
}

#[cfg(target_os = "windows")]
fn windows_backup_sibling(source: &Path) -> PathBuf {
    let mut name = source.as_os_str().to_owned();
    name.push(".replaced-backup");
    name.into()
}

#[cfg(any(test, target_os = "windows"))]
fn is_windows_move_failure(code: Option<i32>) -> bool {
    matches!(code, Some(1176 | 1177))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_user_only_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_user_only_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_user_only_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_user_only_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Clone)]
pub struct ConfigurationStore {
    file: AtomicJsonFile<RuntimeAtlasConfiguration>,
}

impl ConfigurationStore {
    pub fn new(paths: &RuntimeAtlasPaths) -> Self {
        Self {
            file: AtomicJsonFile {
                path: paths.configuration_file.clone(),
                empty: RuntimeAtlasConfiguration::default,
                damaged_notice: DAMAGED_CONFIGURATION_NOTICE,
            },
        }
    }

    pub fn load(&self) -> StorageResult<StoreLoad<RuntimeAtlasConfiguration>> {
        self.file
            .load_checked(ensure_supported_configuration_schema)
    }

    fn update(
        &self,
        mutation: impl FnOnce(&mut RuntimeAtlasConfiguration) -> StorageResult<()>,
    ) -> StorageResult<Option<String>> {
        self.file
            .update_checked(ensure_supported_configuration_schema, mutation)
    }

    pub fn add_repository(&self, path: impl AsRef<Path>) -> StorageResult<Uuid> {
        let path = canonical_path(path.as_ref());
        let mut id = Uuid::new_v4();
        self.update(|configuration| {
            if let Some(existing) = configuration
                .repositories
                .iter()
                .find(|repository| repository.path == path)
            {
                id = existing.id;
                return Ok(());
            }
            configuration.repositories.push(RepositoryRegistration {
                id,
                path,
                added_at: Utc::now(),
            });
            configuration
                .repositories
                .sort_by_key(|repository| repository.added_at);
            Ok(())
        })?;
        Ok(id)
    }

    pub fn remove_repository(&self, id: Uuid) -> StorageResult<()> {
        self.update(|configuration| {
            configuration
                .repositories
                .retain(|repository| repository.id != id);
            configuration
                .custom_actions
                .retain(|action| action.repository_id != id);
            configuration
                .worktree_order_by_repository
                .remove(&repository_uuid_key(id));
            configuration
                .worktree_order_by_repository
                .remove(&id.to_string());
            Ok(())
        })?;
        Ok(())
    }

    pub fn set_app_language(&self, language: AppLanguage) -> StorageResult<()> {
        self.update(|configuration| {
            configuration.app_language = Some(language);
            Ok(())
        })?;
        Ok(())
    }

    pub fn set_worktree_order(&self, repository_id: Uuid, keys: &[String]) -> StorageResult<()> {
        let mut seen = HashSet::new();
        let keys: Vec<_> = keys
            .iter()
            .filter(|key| seen.insert((*key).clone()))
            .cloned()
            .collect();
        self.update(|configuration| {
            ensure_repository(configuration, repository_id)?;
            configuration.schema_version = configuration.schema_version.max(3);
            configuration
                .worktree_order_by_repository
                .insert(repository_uuid_key(repository_id), keys);
            Ok(())
        })?;
        Ok(())
    }

    pub fn save_custom_action(&self, action: CustomActionDefinition) -> StorageResult<()> {
        validate_custom_action(&action)
            .map_err(|error| RuntimeAtlasStorageError::InvalidInput(error.to_string()))?;
        self.update(|configuration| {
            ensure_repository(configuration, action.repository_id)?;
            configuration.schema_version = configuration.schema_version.max(4);
            if let Some(existing) = configuration
                .custom_actions
                .iter_mut()
                .find(|existing| existing.id == action.id)
            {
                *existing = action;
            } else {
                configuration.custom_actions.push(action);
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn link_process(&self, mut link: UserProcessLink) -> StorageResult<()> {
        if !link.process_identity.is_valid()
            || link.worktree_path.trim().is_empty()
            || !Path::new(&link.worktree_path).is_absolute()
        {
            return Err(RuntimeAtlasStorageError::InvalidInput(
                "process identity and worktree path are required".into(),
            ));
        }
        link.worktree_path = canonical_path(Path::new(&link.worktree_path));
        self.update(|configuration| {
            configuration.schema_version = configuration.schema_version.max(5);
            configuration
                .process_links
                .retain(|stored| stored.process_identity != link.process_identity);
            configuration.process_links.push(link);
            Ok(())
        })?;
        Ok(())
    }

    pub fn unlink_process(&self, identity: &ProcessIdentity) -> StorageResult<()> {
        self.update(|configuration| {
            configuration.schema_version = configuration.schema_version.max(5);
            configuration
                .process_links
                .retain(|link| &link.process_identity != identity);
            Ok(())
        })?;
        Ok(())
    }

    pub fn remove_custom_action(&self, id: Uuid) -> StorageResult<()> {
        self.update(|configuration| {
            configuration
                .custom_actions
                .retain(|action| action.id != id);
            Ok(())
        })?;
        Ok(())
    }
}

fn ensure_supported_configuration_schema(data: &[u8]) -> StorageResult<()> {
    ensure_supported_schema(
        data,
        CONFIGURATION_SCHEMA_VERSION,
        "repository configuration was written by a newer Runtime Atlas version",
    )
}

fn ensure_supported_schema(data: &[u8], maximum: u32, message: &str) -> StorageResult<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Schema {
        #[serde(default = "legacy_stored_schema")]
        schema_version: u32,
    }

    if serde_json::from_slice::<Schema>(data).is_ok_and(|schema| schema.schema_version > maximum) {
        return invalid_input(message);
    }
    Ok(())
}

const fn legacy_stored_schema() -> u32 {
    1
}

fn invalid_input<T>(message: &str) -> StorageResult<T> {
    Err(RuntimeAtlasStorageError::InvalidInput(message.into()))
}

fn ensure_repository(configuration: &RuntimeAtlasConfiguration, id: Uuid) -> StorageResult<()> {
    configuration
        .repositories
        .iter()
        .any(|repository| repository.id == id)
        .then_some(())
        .ok_or_else(|| {
            RuntimeAtlasStorageError::InvalidInput("repository is no longer registered".into())
        })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionSessionRecord {
    pub id: Uuid,
    #[serde(rename = "actionID")]
    pub action_id: Uuid,
    pub worktree_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor_identity: Option<ProcessIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    marker_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    control_identity: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pending: bool,
    pub started_at: DateTime<Utc>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl<'de> Deserialize<'de> for ActionSessionRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StoredSession {
            id: Uuid,
            #[serde(rename = "actionID")]
            action_id: Uuid,
            worktree_path: String,
            #[serde(rename = "supervisorPID")]
            _supervisor_pid: Option<u32>,
            supervisor_identity: Option<ProcessIdentity>,
            marker_identity: Option<String>,
            control_identity: Option<String>,
            #[serde(default)]
            pending: bool,
            started_at: DateTime<Utc>,
        }

        let stored = StoredSession::deserialize(deserializer)?;
        Ok(Self {
            id: stored.id,
            action_id: stored.action_id,
            worktree_path: canonical_path(Path::new(&stored.worktree_path)),
            // Schema-one PID/token records are intentionally not adopted.
            supervisor_identity: stored.supervisor_identity.filter(ProcessIdentity::is_valid),
            marker_identity: stored.marker_identity.filter(|value| !value.is_empty()),
            control_identity: stored.control_identity.filter(|value| !value.is_empty()),
            pending: stored.pending,
            started_at: stored.started_at,
        })
    }
}

impl ActionSessionRecord {
    pub fn new(
        id: Uuid,
        action_id: Uuid,
        worktree_path: impl AsRef<Path>,
        supervisor_identity: ProcessIdentity,
        marker_identity: String,
    ) -> StorageResult<Self> {
        Self::pending(id, action_id, worktree_path)?.finalize(supervisor_identity, marker_identity)
    }

    pub fn pending(
        id: Uuid,
        action_id: Uuid,
        worktree_path: impl AsRef<Path>,
    ) -> StorageResult<Self> {
        if id.is_nil() || action_id.is_nil() || !worktree_path.as_ref().is_absolute() {
            return invalid_input("session, action, and absolute worktree are required");
        }
        Ok(Self {
            id,
            action_id,
            worktree_path: canonical_path(worktree_path.as_ref()),
            supervisor_identity: None,
            marker_identity: None,
            control_identity: None,
            pending: true,
            started_at: Utc::now(),
        })
    }

    pub fn pending_with_control_identity(
        id: Uuid,
        action_id: Uuid,
        worktree_path: impl AsRef<Path>,
        control_identity: String,
    ) -> StorageResult<Self> {
        if control_identity.is_empty() {
            return invalid_input("exact control file identity is required");
        }
        let mut record = Self::pending(id, action_id, worktree_path)?;
        record.control_identity = Some(control_identity);
        Ok(record)
    }

    pub fn finalize(
        mut self,
        supervisor_identity: ProcessIdentity,
        marker_identity: String,
    ) -> StorageResult<Self> {
        if !self.pending || !supervisor_identity.is_valid() || marker_identity.is_empty() {
            return invalid_input("pending session and exact identities are required");
        }
        self.supervisor_identity = Some(supervisor_identity);
        self.marker_identity = Some(marker_identity);
        self.pending = false;
        Ok(self)
    }

    pub fn is_pending(&self) -> bool {
        self.pending && self.supervisor_identity.is_none() && self.marker_identity.is_none()
    }

    pub fn supervisor_identity(&self) -> Option<&ProcessIdentity> {
        self.supervisor_identity.as_ref()
    }

    pub fn marker_identity(&self) -> Option<&str> {
        self.marker_identity.as_deref()
    }

    pub fn control_identity(&self) -> Option<&str> {
        self.control_identity.as_deref()
    }

    pub fn is_unlinked_legacy(&self) -> bool {
        !self.pending
            && self.supervisor_identity.is_none()
            && self.marker_identity.is_none()
            && self.control_identity.is_none()
    }

    pub fn link_state(
        &self,
        observed_supervisor: &ProcessIdentity,
        observed_marker_identity: &str,
        observed_worktree: impl AsRef<Path>,
    ) -> ActionSessionLinkState {
        if self.is_pending() {
            return ActionSessionLinkState::Pending;
        }
        let (Some(supervisor), Some(marker)) = (&self.supervisor_identity, &self.marker_identity)
        else {
            return ActionSessionLinkState::UnlinkedLegacy;
        };
        if supervisor == observed_supervisor
            && marker == observed_marker_identity
            && self.worktree_path == canonical_path(observed_worktree.as_ref())
        {
            ActionSessionLinkState::Verified
        } else {
            ActionSessionLinkState::Mismatch
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionSessionLinkState {
    Verified,
    Pending,
    UnlinkedLegacy,
    Mismatch,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionSessionDocument {
    #[serde(default = "legacy_action_session_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub sessions: Vec<ActionSessionRecord>,
}

impl Default for ActionSessionDocument {
    fn default() -> Self {
        Self {
            schema_version: action_session_schema(),
            sessions: Vec::new(),
        }
    }
}

const fn action_session_schema() -> u32 {
    2
}

const fn legacy_action_session_schema() -> u32 {
    1
}

#[derive(Clone)]
pub struct ActionSessionStore {
    file: AtomicJsonFile<ActionSessionDocument>,
}

impl ActionSessionStore {
    pub fn new(paths: &RuntimeAtlasPaths) -> Self {
        Self {
            file: AtomicJsonFile {
                path: paths.action_sessions_file.clone(),
                empty: ActionSessionDocument::default,
                damaged_notice: DAMAGED_SESSIONS_NOTICE,
            },
        }
    }

    pub fn load(&self) -> StorageResult<StoreLoad<ActionSessionDocument>> {
        self.file
            .load_checked(ensure_supported_action_session_schema)
    }

    fn update(
        &self,
        mutation: impl FnOnce(&mut ActionSessionDocument) -> StorageResult<()>,
    ) -> StorageResult<Option<String>> {
        self.file
            .update_checked(ensure_supported_action_session_schema, mutation)
    }

    pub fn upsert(&self, record: ActionSessionRecord) -> StorageResult<()> {
        self.update(|document| {
            document.schema_version = action_session_schema();
            document.sessions.retain(|session| {
                session.action_id != record.action_id
                    || canonical_path(Path::new(&session.worktree_path)) != record.worktree_path
            });
            document.sessions.push(record);
            Ok(())
        })?;
        Ok(())
    }

    pub fn remove(&self, id: Uuid) -> StorageResult<()> {
        self.update(|document| {
            document.schema_version = action_session_schema();
            document.sessions.retain(|session| session.id != id);
            Ok(())
        })?;
        Ok(())
    }
}

fn ensure_supported_action_session_schema(data: &[u8]) -> StorageResult<()> {
    ensure_supported_schema(
        data,
        action_session_schema(),
        "action session data was written by a newer Runtime Atlas version",
    )
}

pub fn canonical_path(path: &Path) -> String {
    fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub fn is_same_or_descendant(candidate: &Path, root: &Path) -> bool {
    Path::new(&canonical_path(candidate)).starts_with(canonical_path(root))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use chrono::TimeZone;
    use tempfile::tempdir;

    use super::*;
    use crate::models::{
        CustomActionInputDefinition, CustomActionInputKind, CustomActionKind, CustomActionRisk,
        CustomActionWorkingDirectory,
    };

    #[test]
    fn configuration_migrates_and_concurrent_updates_keep_every_repository() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        fs::write(
            &paths.configuration_file,
            r#"{"schemaVersion":1,"repositories":[],"appLanguage":"ko"}"#,
        )
        .unwrap();
        let store = Arc::new(ConfigurationStore::new(&paths));
        let barrier = Arc::new(Barrier::new(9));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let path = directory.path().join(format!("repository-{index}"));
                fs::create_dir(&path).unwrap();
                thread::spawn(move || {
                    barrier.wait();
                    store.add_repository(path).unwrap();
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        let loaded = store.load().unwrap().value;
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.app_language, Some(AppLanguage::Korean));
        assert_eq!(loaded.repositories.len(), 8);
    }

    #[test]
    fn damaged_file_is_preserved_on_the_next_save() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        fs::write(&paths.configuration_file, b"{").unwrap();
        let store = ConfigurationStore::new(&paths);
        assert!(store.load().unwrap().recovery_notice.is_some());
        store.add_repository(directory.path()).unwrap();
        assert!(store.load().unwrap().recovery_notice.is_none());
        let backups: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("configuration.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].path()).unwrap(), b"{");
    }

    #[test]
    fn future_configuration_rejects_load_and_every_mutation_without_changing_bytes() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        let future = br#"{
  "schemaVersion": 6,
  "repositories": {"future": "shape"},
  "unknownField": [1, 2, 3]
}"#;
        fs::write(&paths.configuration_file, future).unwrap();
        let store = ConfigurationStore::new(&paths);
        let repository_id = Uuid::new_v4();
        let identity = ProcessIdentity {
            pid: 4242,
            start_identity: "created-123".to_owned(),
        };
        let action = CustomActionDefinition::new(repository_id, "Build", "cargo build");
        let unchanged = |result: StorageResult<()>| {
            assert!(matches!(
                result,
                Err(RuntimeAtlasStorageError::InvalidInput(_))
            ));
            assert_eq!(fs::read(&paths.configuration_file).unwrap(), future);
        };

        unchanged(store.load().map(drop));
        unchanged(store.add_repository(directory.path()).map(drop));
        unchanged(store.remove_repository(repository_id));
        unchanged(store.set_app_language(AppLanguage::Korean));
        unchanged(store.set_worktree_order(repository_id, &["branch:main".to_owned()]));
        unchanged(store.save_custom_action(action.clone()));
        unchanged(store.remove_custom_action(action.id));
        unchanged(store.link_process(UserProcessLink {
            process_identity: identity.clone(),
            worktree_path: directory.path().to_string_lossy().into_owned(),
        }));
        unchanged(store.unlink_process(&identity));
    }

    #[test]
    fn process_lease_excludes_a_second_holder() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        let first = RuntimeAtlasProcessLease::try_acquire(&paths).unwrap();
        assert!(matches!(
            RuntimeAtlasProcessLease::try_acquire(&paths),
            Err(RuntimeAtlasStorageError::CannotLock)
        ));
        drop(first);
        RuntimeAtlasProcessLease::try_acquire(&paths).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn storage_keeps_directory_and_files_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path().join("data"));
        ConfigurationStore::new(&paths)
            .add_repository(directory.path())
            .unwrap();
        assert_eq!(
            fs::metadata(&paths.directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.configuration_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn swift_uuid_order_keys_and_schema_five_process_links_round_trip() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        let repository_id = Uuid::parse_str("00000000-0000-0000-0000-00000000abcd").unwrap();
        let stored_order =
            std::collections::BTreeMap::from([(repository_id.to_string(), vec!["branch:main"])]);
        fs::write(
            &paths.configuration_file,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 4,
                "repositories": [{
                    "id": repository_id,
                    "path": directory.path(),
                    "addedAt": "2026-08-09T00:00:00Z"
                }],
                "worktreeOrderByRepository": stored_order
            }))
            .unwrap(),
        )
        .unwrap();
        let store = ConfigurationStore::new(&paths);
        let loaded = store.load().unwrap().value;
        assert_eq!(
            loaded
                .worktree_order_by_repository
                .get("00000000-0000-0000-0000-00000000ABCD"),
            Some(&vec!["branch:main".to_owned()])
        );

        let identity = ProcessIdentity {
            pid: 4242,
            start_identity: "created-123".to_owned(),
        };
        assert!(
            store
                .link_process(UserProcessLink {
                    process_identity: identity.clone(),
                    worktree_path: "relative/worktree".to_owned(),
                })
                .is_err()
        );
        store
            .link_process(UserProcessLink {
                process_identity: identity.clone(),
                worktree_path: directory.path().to_string_lossy().into_owned(),
            })
            .unwrap();
        let loaded = store.load().unwrap().value;
        assert_eq!(loaded.schema_version, 5);
        assert_eq!(loaded.process_links.len(), 1);
        assert!(
            loaded
                .worktree_order_by_repository
                .contains_key("00000000-0000-0000-0000-00000000ABCD")
        );
        let encoded = serde_json::to_string(&loaded).unwrap();
        assert_eq!(
            serde_json::from_str::<RuntimeAtlasConfiguration>(&encoded).unwrap(),
            loaded
        );

        store.unlink_process(&identity).unwrap();
        assert!(store.load().unwrap().value.process_links.is_empty());
    }

    #[test]
    fn custom_actions_are_validated_before_storage() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        let store = ConfigurationStore::new(&paths);
        let repository_id = store.add_repository(directory.path()).unwrap();

        for command in [
            "npm run dev && touch nope",
            "echo $(whoami)",
            "curl https://example.invalid/private",
            "/bin/sh -c 'echo nope'",
            "echo token=secret-value",
            "npm {{missing}}",
        ] {
            let action = CustomActionDefinition::new(repository_id, "Unsafe", command);
            assert!(matches!(
                store.save_custom_action(action),
                Err(RuntimeAtlasStorageError::InvalidInput(_))
            ));
        }
        assert!(store.load().unwrap().value.custom_actions.is_empty());

        let mut valid =
            CustomActionDefinition::new(repository_id, "Server", "npm run dev -- {{target}}");
        valid.kind = CustomActionKind::Session;
        valid.risk = CustomActionRisk::Normal;
        valid.working_directory = CustomActionWorkingDirectory::SelectedWorktree;
        valid.detects_running_worktree_listener = true;
        valid.inputs.push(CustomActionInputDefinition {
            id: Uuid::new_v4(),
            key: "target".to_owned(),
            label: "Target".to_owned(),
            kind: CustomActionInputKind::Text,
            flag_argument: None,
            is_enabled_by_default: false,
        });
        store.save_custom_action(valid.clone()).unwrap();
        assert_eq!(store.load().unwrap().value.custom_actions, vec![valid]);
    }

    #[test]
    fn action_session_schema_two_requires_exact_process_and_marker_identities() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        let session_id = Uuid::new_v4();
        let supervisor = ProcessIdentity {
            pid: 4242,
            start_identity: "macos:1:000042".into(),
        };
        assert!(
            ActionSessionRecord::new(
                Uuid::nil(),
                Uuid::new_v4(),
                directory.path(),
                supervisor.clone(),
                "file:1:2".into(),
            )
            .is_err()
        );
        let mut record = ActionSessionRecord::new(
            session_id,
            Uuid::new_v4(),
            directory.path(),
            supervisor.clone(),
            "file:1:2".into(),
        )
        .unwrap();
        record.started_at = Utc.timestamp_opt(123, 0).unwrap();
        let store = ActionSessionStore::new(&paths);
        store.upsert(record.clone()).unwrap();
        assert_eq!(store.load().unwrap().value.sessions, vec![record.clone()]);
        assert_eq!(
            record.link_state(&supervisor, "file:1:2", directory.path().join(".")),
            ActionSessionLinkState::Verified
        );
        assert!(!record.is_unlinked_legacy());
        assert_eq!(
            record.link_state(
                &ProcessIdentity {
                    pid: 4242,
                    start_identity: "macos:2:000042".into(),
                },
                "file:1:2",
                directory.path(),
            ),
            ActionSessionLinkState::Mismatch
        );
        assert_eq!(
            record.link_state(&supervisor, "file:1:3", directory.path()),
            ActionSessionLinkState::Mismatch
        );
        let legacy: ActionSessionRecord = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "actionID": Uuid::new_v4(),
            "worktreePath": directory.path(),
            "supervisorPID": 4242,
            "startedAt": "1970-01-01T00:02:03Z"
        }))
        .unwrap();
        assert_eq!(
            legacy.link_state(&supervisor, "file:1:2", directory.path()),
            ActionSessionLinkState::UnlinkedLegacy
        );
        assert!(legacy.is_unlinked_legacy());
        let pending =
            ActionSessionRecord::pending(Uuid::new_v4(), Uuid::new_v4(), directory.path()).unwrap();
        assert!(pending.is_pending());
        assert_eq!(
            pending.link_state(&supervisor, "file:1:2", directory.path()),
            ActionSessionLinkState::Pending
        );
        let finalized = pending
            .finalize(supervisor.clone(), "file:1:2".into())
            .unwrap();
        assert!(!finalized.is_pending());
        assert_eq!(
            finalized.link_state(&supervisor, "file:1:2", directory.path()),
            ActionSessionLinkState::Verified
        );
        let json = fs::read_to_string(&paths.action_sessions_file).unwrap();
        assert!(json.contains("1970-01-01T00:02:03Z"));
        assert_eq!(
            serde_json::from_str::<ActionSessionDocument>(r#"{"sessions":[]}"#)
                .unwrap()
                .schema_version,
            1
        );
        assert_eq!(store.load().unwrap().value.schema_version, 2);
    }

    #[test]
    fn future_action_sessions_reject_load_and_mutations_without_changing_bytes() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        let future = br#"{
  "schemaVersion": 3,
  "sessions": {"future": "shape"},
  "unknownField": true
}"#;
        fs::write(&paths.action_sessions_file, future).unwrap();
        let store = ActionSessionStore::new(&paths);
        let unchanged = |result: StorageResult<()>| {
            assert!(matches!(
                result,
                Err(RuntimeAtlasStorageError::InvalidInput(_))
            ));
            assert_eq!(fs::read(&paths.action_sessions_file).unwrap(), future);
        };

        unchanged(store.load().map(drop));
        unchanged(store.upsert(
            ActionSessionRecord::pending(Uuid::new_v4(), Uuid::new_v4(), directory.path()).unwrap(),
        ));
        unchanged(store.remove(Uuid::new_v4()));
    }

    #[test]
    fn recognizes_replace_file_move_failures() {
        assert!(is_windows_move_failure(Some(1176)));
        assert!(is_windows_move_failure(Some(1177)));
        assert!(!is_windows_move_failure(Some(1175)));
    }

    #[test]
    fn failed_replace_never_deletes_the_only_new_copy() {
        let directory = tempdir().unwrap();
        let temporary = directory.path().join("new.tmp");
        let destination = directory.path().join("configuration.json");
        fs::write(&temporary, b"new").unwrap();
        cleanup_failed_temporary(&temporary, &destination);
        assert_eq!(fs::read(&temporary).unwrap(), b"new");

        fs::write(&destination, b"old").unwrap();
        cleanup_failed_temporary(&temporary, &destination);
        assert!(!temporary.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"old");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_replace_failures_keep_or_recover_a_canonical_file() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("new.tmp");
        let destination = directory.path().join("configuration.json");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();
        let error = replace_windows_with(&source, &destination, |_, _, _| {
            Err(io::Error::from_raw_os_error(1176))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(1176));
        assert_eq!(fs::read(&source).unwrap(), b"new");
        assert_eq!(fs::read(&destination).unwrap(), b"old");

        replace_windows_with(&source, &destination, |_, destination, backup| {
            fs::rename(destination, backup)?;
            Err(io::Error::from_raw_os_error(1177))
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!source.exists());
        assert!(!windows_backup_sibling(&source).exists());
    }
}
