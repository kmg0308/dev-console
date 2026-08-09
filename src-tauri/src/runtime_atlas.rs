use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use runtime_atlas_core::actions::{
    CustomActionPlan, plan_custom_action, plan_custom_action_restart, sanitize_output,
};
use runtime_atlas_core::command::output as command_output;
#[cfg(target_os = "macos")]
use runtime_atlas_core::command::output_with_timeout;
use runtime_atlas_core::models::{
    AppLanguage, AtlasNotice, AtlasNoticeKind, AvailabilityState, CustomActionDefinition,
    CustomActionKind, RepositoryStatus, WorktreeNavigationDirection, WorktreeNavigationSession,
    WorktreeStatus, advance_worktree_navigation, reconcile_recent_worktrees,
    record_recent_worktree,
};
use runtime_atlas_core::observe::{
    DockerObservation, ProcessObservation, observe_docker, observe_processes,
};
use runtime_atlas_core::relations::{
    ManagedSessionLink, ObservedProcess, PathFlavor, ProcessIdentity, TerminationSnapshot,
    UserProcessLink, paths_equal, plan_termination,
};
use runtime_atlas_core::repository::{expand_worktree_order, inspect_repositories};
use runtime_atlas_core::service::{
    ActionRun, ActionRunPhase, RepositorySnapshotInput, RuntimeAtlasSnapshot,
    RuntimeAtlasSnapshotInput, build_snapshot,
};
use runtime_atlas_core::sessions::{
    SessionMarker, file_identity as marker_file_identity, open_session_control, process_identity,
    read_control_identity, read_session_marker, reconcile_action_sessions,
    registered_action_session, supervisor_executable_matches, validate_action_session,
};
use runtime_atlas_core::storage::{
    ActionSessionRecord, ActionSessionStore, ConfigurationStore, RuntimeAtlasPaths,
    RuntimeAtlasProcessLease, canonical_path,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

pub struct RuntimeAtlasState {
    store: ConfigurationStore,
    sessions: ActionSessionStore,
    paths: RuntimeAtlasPaths,
    _lease: RuntimeAtlasProcessLease,
    default_language: AppLanguage,
    memory: Arc<Mutex<RuntimeMemory>>,
    // ponytail: one operation lock keeps rare action transitions atomic; split per key if latency matters.
    action_operation: Mutex<()>,
    update_shutdown: AtomicBool,
    #[cfg(target_os = "macos")]
    login_environment: OnceLock<Result<Vec<(OsString, OsString)>, String>>,
}

#[derive(Default)]
struct RuntimeMemory {
    displayed_processes: Vec<ObservedProcess>,
    worktree_paths: Vec<String>,
    recent_paths: Vec<String>,
    navigation: Option<WorktreeNavigationSession>,
    action_runs: BTreeMap<ActionRunKey, ActionRun>,
    action_output: BTreeMap<ActionRunKey, String>,
    active_runs: BTreeMap<ActionRunKey, ActiveRun>,
    action_confirmations: BTreeMap<Uuid, PendingActionConfirmation>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ActionRunKey {
    action_id: Uuid,
    worktree_path: String,
}

#[derive(Clone)]
struct ActiveRun {
    generation: Uuid,
    session_id: Option<Uuid>,
    supervisor_identity: ProcessIdentity,
}

type ReconciledSessions = (Vec<ManagedSessionLink>, Vec<ActionRun>, Option<String>);

enum PendingResolution {
    Finalized(ActionSessionRecord),
    Stale,
    Unverified,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum ActionInputValue {
    Text(String),
    Boolean(bool),
}

#[derive(Clone)]
struct PendingActionConfirmation {
    action: CustomActionDefinition,
    worktree_path: String,
    values: BTreeMap<String, ActionInputValue>,
    restart: bool,
    plan: CustomActionPlan,
    expires_at: Instant,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionConfirmationPlan {
    confirmation_token: Uuid,
    display_command: String,
    worktree_path: String,
    effects: Vec<String>,
}

const ACTION_CONFIRMATION_TTL: Duration = Duration::from_secs(120);

pub fn initialize(app: &AppHandle) -> Result<RuntimeAtlasState, String> {
    let local_data = app
        .path()
        .local_data_dir()
        .map_err(string_error)?
        .join("Runtime Atlas");
    let isolated = crate::windows_updater_qa_root(&app.config().identifier)?
        .map(|root| root.join("Runtime Atlas"));
    if isolated
        .as_ref()
        .is_some_and(|path| crate::platform_paths_equal(path, &local_data))
    {
        return Err("Updater QA local data isolation is unavailable".to_owned());
    }
    let base = isolated.unwrap_or(local_data);
    RuntimeAtlasState::new(base, system_language())
}

impl RuntimeAtlasState {
    fn new(directory: PathBuf, default_language: AppLanguage) -> Result<Self, String> {
        let paths = RuntimeAtlasPaths::new(directory);
        prepare_private_directory(&paths.action_session_markers_directory)?;
        Ok(Self {
            store: ConfigurationStore::new(&paths),
            sessions: ActionSessionStore::new(&paths),
            paths: paths.clone(),
            _lease: RuntimeAtlasProcessLease::try_acquire(&paths).map_err(string_error)?,
            default_language,
            memory: Arc::new(Mutex::new(RuntimeMemory::default())),
            action_operation: Mutex::new(()),
            update_shutdown: AtomicBool::new(false),
            #[cfg(target_os = "macos")]
            login_environment: OnceLock::new(),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, RuntimeMemory>, String> {
        self.memory
            .lock()
            .map_err(|_| "Runtime Atlas state lock is poisoned".to_owned())
    }

    fn action_sessions_for_mutation(&self) -> Result<(u32, Vec<ActionSessionRecord>), String> {
        let loaded = self.sessions.load().map_err(string_error)?;
        if loaded.recovery_notice.is_some() {
            return Err(
                "action session data is damaged; managed actions cannot be changed safely"
                    .to_owned(),
            );
        }
        if loaded.value.schema_version < 2
            && loaded
                .value
                .sessions
                .iter()
                .any(|record| !record.is_unlinked_legacy())
        {
            return Err(
                "legacy action session data contains unverifiable managed state".to_owned(),
            );
        }
        Ok((loaded.value.schema_version, loaded.value.sessions))
    }

    #[cfg(target_os = "macos")]
    fn login_environment(&self) -> Result<&[(OsString, OsString)], String> {
        self.login_environment
            .get_or_init(resolve_login_environment)
            .as_ref()
            .map(Vec::as_slice)
            .map_err(Clone::clone)
    }

    fn status(&self) -> Result<RuntimeAtlasSnapshot, String> {
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        self.compose_status(
            observe_processes(),
            observe_docker(runtime_atlas_core::observe::resolve_docker_executable().as_deref()),
        )
    }

    fn compose_status(
        &self,
        mut process_observation: ProcessObservation,
        mut docker_observation: DockerObservation,
    ) -> Result<RuntimeAtlasSnapshot, String> {
        let loaded = self.store.load().map_err(string_error)?;
        let repositories = inspect_repositories(
            &loaded.value.repositories,
            &loaded.value.worktree_order_by_repository,
            git_executable(),
        );
        let (managed_sessions, action_runs, session_notice) = self.reconcile_sessions(
            &loaded.value.custom_actions,
            &repositories,
            &process_observation.processes,
        )?;
        let mut notices = std::mem::take(&mut process_observation.notices);
        notices.append(&mut docker_observation.notices);
        for message in [loaded.recovery_notice, session_notice]
            .into_iter()
            .flatten()
        {
            notices.push(AtlasNotice {
                kind: AtlasNoticeKind::Error,
                message,
            });
        }
        let snapshot = build_snapshot(RuntimeAtlasSnapshotInput {
            generated_at: chrono::Utc::now(),
            language: loaded.value.app_language.unwrap_or(self.default_language),
            process_discovery: process_observation.availability,
            docker_discovery: docker_observation.availability,
            notices,
            repositories: repositories
                .into_iter()
                .map(|repository| RepositorySnapshotInput {
                    repository,
                    path_flavor: path_flavor(),
                })
                .collect(),
            observed_processes: process_observation.processes,
            managed_sessions,
            user_links: loaded.value.process_links,
            containers: docker_observation.containers,
            actions: loaded.value.custom_actions,
            action_runs,
        });
        let worktree_paths = snapshot
            .repositories
            .iter()
            .flat_map(|repository| &repository.worktrees)
            .filter(|worktree| worktree.availability == AvailabilityState::Available)
            .map(|worktree| worktree.path.clone())
            .collect::<Vec<_>>();
        let mut memory = self.lock()?;
        memory.displayed_processes = snapshot.processes.clone();
        memory.recent_paths = reconcile_recent_worktrees(&memory.recent_paths, &worktree_paths);
        if memory
            .navigation
            .as_ref()
            .and_then(|session| session.selected_path())
            .is_some_and(|selected| {
                !worktree_paths
                    .iter()
                    .any(|path| paths_equal(path, selected, path_flavor()))
            })
        {
            memory.navigation = None;
        }
        memory.worktree_paths = worktree_paths;
        Ok(snapshot)
    }

    fn reconcile_sessions(
        &self,
        actions: &[CustomActionDefinition],
        repositories: &[RepositoryStatus],
        observed: &[ObservedProcess],
    ) -> Result<ReconciledSessions, String> {
        let marker_notice = self.recover_orphan_markers(actions, repositories)?;
        let loaded = self.sessions.load().map_err(string_error)?;
        if loaded.value.schema_version > 2 {
            return Err("action session data requires a newer Runtime Atlas version".to_owned());
        }
        let mut stale = Vec::new();
        let session_schema_version = loaded.value.schema_version;
        let mut memory = self.lock()?;
        for stored in &loaded.value.sessions {
            let finalized;
            let record = if stored.is_pending() {
                match self.resolve_pending_session(stored)? {
                    PendingResolution::Finalized(record) => {
                        finalized = record;
                        &finalized
                    }
                    PendingResolution::Stale => {
                        stale.push(stored.id);
                        continue;
                    }
                    PendingResolution::Unverified => {
                        memory
                            .action_runs
                            .remove(&action_key(stored.action_id, &stored.worktree_path));
                        continue;
                    }
                }
            } else {
                stored
            };
            if !self.validate_session(record, &record.worktree_path, session_schema_version) {
                if self.exact_supervisor_is_running(record) {
                    remove_memory_session(&mut memory, record);
                    continue;
                }
                self.remove_marker_if_owned(record);
                remove_memory_session(&mut memory, record);
                stale.push(record.id);
                continue;
            }
            let Some((_action, _worktree)) = self.registered_session(record, actions, repositories)
            else {
                if let Some(supervisor) = record.supervisor_identity() {
                    stop_verified_supervisor(supervisor)?;
                }
                self.remove_marker_if_owned(record);
                remove_memory_session(&mut memory, record);
                stale.push(record.id);
                continue;
            };
            let key = action_key(record.action_id, &record.worktree_path);
            memory.active_runs.entry(key.clone()).or_insert(ActiveRun {
                generation: Uuid::new_v4(),
                session_id: Some(record.id),
                supervisor_identity: record
                    .supervisor_identity()
                    .expect("validated identity")
                    .clone(),
            });
        }
        drop(memory);
        for id in stale {
            self.sessions.remove(id).map_err(string_error)?;
        }
        let reconciled = reconcile_action_sessions(
            &self.paths,
            actions,
            repositories,
            observed,
            &supervisor_executable()?,
            path_flavor(),
        )?;
        let mut memory = self.lock()?;
        for run in reconciled.action_runs {
            memory
                .action_runs
                .entry(action_key(run.action_id, &run.worktree_path))
                .or_insert(run);
        }
        let action_runs = memory
            .action_runs
            .iter()
            .map(|(key, run)| {
                let mut run = run.clone();
                run.output = sanitize_output(
                    memory
                        .action_output
                        .get(key)
                        .map(String::as_str)
                        .unwrap_or(&run.output),
                );
                run
            })
            .collect();
        let session_notice = [loaded.recovery_notice, marker_notice]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Ok((
            reconciled.managed_sessions,
            action_runs,
            (!session_notice.is_empty()).then_some(session_notice),
        ))
    }

    fn resolve_pending_session(
        &self,
        record: &ActionSessionRecord,
    ) -> Result<PendingResolution, String> {
        let expected = supervisor_executable()?;
        self.resolve_pending_session_with(record, &expected)
    }

    fn resolve_pending_session_with(
        &self,
        record: &ActionSessionRecord,
        expected_executable: &Path,
    ) -> Result<PendingResolution, String> {
        let remaining = (record.started_at + chrono::Duration::seconds(3) - chrono::Utc::now())
            .to_std()
            .unwrap_or_default()
            .min(Duration::from_secs(3));
        let deadline = Instant::now() + remaining;
        let path = self.marker_path(record.id);
        loop {
            match fs::symlink_metadata(&path) {
                Ok(_) => match read_session_marker(&path) {
                    Ok((marker, marker_identity))
                        if marker.matches_session(record, path_flavor())
                            && marker.schema_version == 2 =>
                    {
                        if !record.control_identity().is_some_and(|expected| {
                            read_control_identity(&self.control_path(record.id))
                                .is_ok_and(|(actual, state)| actual == expected && state == 0)
                        }) {
                            if Instant::now() >= deadline {
                                return Ok(PendingResolution::Unverified);
                            }
                            thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        let supervisor = ProcessIdentity {
                            pid: marker.supervisor_pid,
                            start_identity: marker.start_identity,
                        };
                        match process_identity(supervisor.pid) {
                            Ok(current)
                                if current == supervisor
                                    && supervisor_executable_matches(
                                        &supervisor,
                                        expected_executable,
                                    ) =>
                            {
                                let finalized = record
                                    .clone()
                                    .finalize(supervisor, marker_identity)
                                    .map_err(string_error)?;
                                self.sessions
                                    .upsert(finalized.clone())
                                    .map_err(string_error)?;
                                return Ok(PendingResolution::Finalized(finalized));
                            }
                            Ok(_) => {
                                remove_marker_with_identity(&path, record.id, &marker_identity);
                                return Ok(PendingResolution::Stale);
                            }
                            Err(_) if Instant::now() >= deadline => {
                                return Ok(PendingResolution::Unverified);
                            }
                            Err(_) => {}
                        }
                    }
                    Ok(_) | Err(_) if Instant::now() >= deadline => {
                        return Ok(PendingResolution::Unverified);
                    }
                    Ok(_) | Err(_) => {}
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if Instant::now() >= deadline {
                        if self.cancel_pending_control(record)? {
                            return Ok(PendingResolution::Stale);
                        }
                        return Ok(PendingResolution::Unverified);
                    }
                }
                Err(_) => return Ok(PendingResolution::Unverified),
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn cancel_pending_control(&self, record: &ActionSessionRecord) -> Result<bool, String> {
        let Some(expected_identity) = record.control_identity() else {
            return Ok(false);
        };
        let path = self.control_path(record.id);
        let mut file = match open_session_control(&path) {
            Ok(file) => file,
            Err(_) if !path.exists() => return Ok(true),
            Err(_) => return Ok(false),
        };
        if marker_file_identity(&file).as_deref() != Ok(expected_identity) {
            return Ok(false);
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => return Ok(false),
        }
        let mut state = [0u8; 1];
        file.seek(SeekFrom::Start(0)).map_err(string_error)?;
        let valid = file.read_exact(&mut state).is_ok() && matches!(state[0], 0 | 1);
        if valid {
            file.seek(SeekFrom::Start(0)).map_err(string_error)?;
            file.write_all(&[1]).map_err(string_error)?;
            file.set_len(1).map_err(string_error)?;
            file.sync_all().map_err(string_error)?;
        }
        let _ = file.unlock();
        drop(file);
        if !valid {
            return Ok(false);
        }
        remove_control_with_identity(&path, expected_identity);
        Ok(true)
    }

    fn registered_session<'a>(
        &self,
        record: &ActionSessionRecord,
        actions: &'a [CustomActionDefinition],
        repositories: &'a [RepositoryStatus],
    ) -> Option<(&'a CustomActionDefinition, &'a WorktreeStatus)> {
        registered_action_session(record, actions, repositories, path_flavor())
    }

    fn validate_session(
        &self,
        record: &ActionSessionRecord,
        worktree: &str,
        session_schema_version: u32,
    ) -> bool {
        let Ok(expected) = supervisor_executable() else {
            return false;
        };
        self.validate_session_with(record, worktree, session_schema_version, &expected)
    }

    fn validate_session_with(
        &self,
        record: &ActionSessionRecord,
        worktree: &str,
        session_schema_version: u32,
        expected_executable: &Path,
    ) -> bool {
        validate_action_session(
            &self.paths,
            record,
            worktree,
            session_schema_version,
            expected_executable,
            path_flavor(),
        )
    }

    fn exact_supervisor_is_running(&self, record: &ActionSessionRecord) -> bool {
        let (Some(supervisor), Ok(expected_executable)) =
            (record.supervisor_identity(), supervisor_executable())
        else {
            return false;
        };
        process_identity(supervisor.pid).ok().as_ref() == Some(supervisor)
            && supervisor_executable_matches(supervisor, &expected_executable)
    }

    fn marker_path(&self, id: Uuid) -> PathBuf {
        self.paths
            .action_session_markers_directory
            .join(format!("{id}.json"))
    }

    fn control_path(&self, id: Uuid) -> PathBuf {
        self.paths
            .action_session_markers_directory
            .join(format!("{id}.control"))
    }

    fn remove_marker_if_owned(&self, record: &ActionSessionRecord) {
        let path = self.marker_path(record.id);
        if let Some(expected) = record.marker_identity() {
            remove_marker_with_identity(&path, record.id, expected);
        }
        remove_owned_control(&self.paths.action_session_markers_directory, record);
    }

    fn recover_orphan_markers(
        &self,
        actions: &[CustomActionDefinition],
        repositories: &[RepositoryStatus],
    ) -> Result<Option<String>, String> {
        let expected_executable = supervisor_executable()?;
        self.recover_orphan_markers_with(actions, repositories, &expected_executable)
    }

    fn recover_orphan_markers_with(
        &self,
        actions: &[CustomActionDefinition],
        repositories: &[RepositoryStatus],
        expected_executable: &Path,
    ) -> Result<Option<String>, String> {
        let loaded = self.sessions.load().map_err(string_error)?;
        if loaded.recovery_notice.is_some() {
            return Ok(None);
        }
        let mut stored = loaded.value.sessions;
        let mut needs_attention = false;
        let entries =
            fs::read_dir(&self.paths.action_session_markers_directory).map_err(string_error)?;
        for entry in entries {
            let Ok(entry) = entry else {
                needs_attention = true;
                continue;
            };
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                needs_attention = true;
                continue;
            };
            let Some(id_text) = file_name.strip_suffix(".json") else {
                continue;
            };
            let Ok(file_id) = Uuid::parse_str(id_text) else {
                needs_attention = true;
                continue;
            };
            if file_name != format!("{file_id}.json") {
                needs_attention = true;
                continue;
            }
            let Ok((marker, marker_identity)) = read_session_marker(&path) else {
                needs_attention = true;
                continue;
            };
            if marker.schema_version != 2
                || marker.session_id != file_id
                || stored.iter().any(|record| record.id == marker.session_id)
            {
                continue;
            }
            let Some((action_id, worktree_path)) = marker.registration() else {
                needs_attention = true;
                continue;
            };
            let supervisor = ProcessIdentity {
                pid: marker.supervisor_pid,
                start_identity: marker.start_identity.clone(),
            };
            if process_identity(supervisor.pid).ok().as_ref() != Some(&supervisor)
                || !supervisor_executable_matches(&supervisor, expected_executable)
            {
                needs_attention = true;
                continue;
            }
            let control_path = self.control_path(marker.session_id);
            let Ok((control_identity, 0)) = read_control_identity(&control_path) else {
                stop_verified_supervisor(&supervisor)?;
                remove_marker_with_identity(&path, marker.session_id, &marker_identity);
                needs_attention = true;
                continue;
            };
            let registered = actions.iter().any(|action| {
                action.id == action_id
                    && action.kind == CustomActionKind::Session
                    && repositories.iter().any(|repository| {
                        repository.id == action.repository_id
                            && repository.availability == AvailabilityState::Available
                            && repository.worktrees.iter().any(|worktree| {
                                worktree.availability == AvailabilityState::Available
                                    && paths_equal(&worktree.path, worktree_path, path_flavor())
                            })
                    })
            });
            let key_is_occupied = stored.iter().any(|record| {
                record.action_id == action_id
                    && paths_equal(&record.worktree_path, worktree_path, path_flavor())
            });
            if registered && !key_is_occupied {
                let record = ActionSessionRecord::pending_with_control_identity(
                    marker.session_id,
                    action_id,
                    Path::new(worktree_path),
                    control_identity,
                )
                .and_then(|record| record.finalize(supervisor, marker_identity))
                .map_err(string_error)?;
                self.sessions.upsert(record.clone()).map_err(string_error)?;
                stored.push(record);
            } else {
                stop_verified_supervisor(&supervisor)?;
                remove_marker_with_identity(&path, marker.session_id, &marker_identity);
                needs_attention = true;
            }
        }
        Ok(needs_attention
            .then(|| "One or more action session markers could not be safely linked.".to_owned()))
    }

    fn action_plan(
        &self,
        action_id: Uuid,
        worktree_path: &str,
        values: BTreeMap<String, ActionInputValue>,
        restart: bool,
    ) -> Result<(CustomActionDefinition, String, CustomActionPlan), String> {
        let configuration = self.store.load().map_err(string_error)?.value;
        let repositories = inspect_repositories(
            &configuration.repositories,
            &configuration.worktree_order_by_repository,
            git_executable(),
        );
        let action = configuration
            .custom_actions
            .into_iter()
            .find(|action| action.id == action_id)
            .ok_or_else(|| "action is no longer registered".to_owned())?;
        let repository = repositories
            .iter()
            .find(|repository| {
                repository.id == action.repository_id
                    && repository.availability == AvailabilityState::Available
            })
            .ok_or_else(|| "action repository is no longer available".to_owned())?;
        let worktree = repository
            .worktrees
            .iter()
            .find(|worktree| {
                worktree.availability == AvailabilityState::Available
                    && paths_equal(&worktree.path, worktree_path, path_flavor())
            })
            .ok_or_else(|| "worktree is no longer available".to_owned())?;
        let values = values
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    match value {
                        ActionInputValue::Text(value) => value,
                        ActionInputValue::Boolean(value) => value.to_string(),
                    },
                )
            })
            .collect();
        let plan = if restart {
            plan_custom_action_restart(&action, &values, repository, worktree)
        } else {
            plan_custom_action(&action, &values, repository, worktree)
        }
        .map_err(string_error)?;
        Ok((action, worktree.path.clone(), plan))
    }

    fn plan_action(
        &self,
        action_id: Uuid,
        worktree_path: &str,
        values: BTreeMap<String, ActionInputValue>,
        restart: bool,
    ) -> Result<ActionConfirmationPlan, String> {
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        self.require_actions_available()?;
        let (action, worktree_path, plan) =
            self.action_plan(action_id, worktree_path, values.clone(), restart)?;
        if restart && action.kind != CustomActionKind::Session {
            return Err("only a keep-running action can be restarted".to_owned());
        }
        let confirmation_token = Uuid::new_v4();
        let preview = ActionConfirmationPlan {
            confirmation_token,
            display_command: plan.display_command.clone(),
            worktree_path: worktree_path.clone(),
            effects: action.effects.clone(),
        };
        let now = Instant::now();
        let mut memory = self.lock()?;
        memory
            .action_confirmations
            .retain(|_, confirmation| confirmation.expires_at > now);
        memory.action_confirmations.insert(
            confirmation_token,
            PendingActionConfirmation {
                action,
                worktree_path,
                values,
                restart,
                plan,
                expires_at: now + ACTION_CONFIRMATION_TTL,
            },
        );
        Ok(preview)
    }

    fn confirm_action(&self, confirmation_token: Uuid) -> Result<(), String> {
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        self.require_actions_available()?;
        let confirmation = self.take_action_confirmation(confirmation_token)?;
        let (mut action, mut worktree, mut plan) =
            self.revalidate_action_confirmation(&confirmation)?;
        if confirmation.restart {
            self.stop_action_inner(&action, &worktree, ActionRunPhase::Restarting)?;
            (action, worktree, plan) = self.revalidate_action_confirmation(&confirmation)?;
        }
        self.launch_action(&confirmation, &action, &worktree, &plan)
    }

    fn take_action_confirmation(
        &self,
        confirmation_token: Uuid,
    ) -> Result<PendingActionConfirmation, String> {
        let confirmation = self
            .lock()?
            .action_confirmations
            .remove(&confirmation_token)
            .ok_or_else(|| "action confirmation is missing or already used".to_owned())?;
        (confirmation.expires_at > Instant::now())
            .then_some(confirmation)
            .ok_or_else(|| "action confirmation expired; review the action again".to_owned())
    }

    fn revalidate_action_confirmation(
        &self,
        confirmation: &PendingActionConfirmation,
    ) -> Result<(CustomActionDefinition, String, CustomActionPlan), String> {
        let current = self.action_plan(
            confirmation.action.id,
            &confirmation.worktree_path,
            confirmation.values.clone(),
            confirmation.restart,
        )?;
        if current.0 != confirmation.action
            || current.1 != confirmation.worktree_path
            || current.2 != confirmation.plan
        {
            return Err("action changed after review; review the action again".to_owned());
        }
        Ok(current)
    }

    fn stop_action(&self, action_id: Uuid, worktree_path: &str) -> Result<(), String> {
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        let configuration = self.store.load().map_err(string_error)?.value;
        let action = configuration
            .custom_actions
            .iter()
            .find(|action| action.id == action_id && action.kind == CustomActionKind::Session)
            .ok_or_else(|| "session action is no longer registered".to_owned())?;
        let worktree = self.verified_worktree(worktree_path)?;
        self.stop_action_inner(action, &worktree, ActionRunPhase::Stopping)
    }

    fn launch_action(
        &self,
        confirmation: &PendingActionConfirmation,
        action: &CustomActionDefinition,
        worktree: &str,
        plan: &CustomActionPlan,
    ) -> Result<(), String> {
        let key = action_key(action.id, worktree);
        let generation = Uuid::new_v4();
        if self.lock()?.active_runs.contains_key(&key) {
            return Err("action is already running for this worktree".to_owned());
        }
        self.remove_stale_record_for_key(&key)?;
        let supervisor_executable = supervisor_executable()?;
        #[cfg(target_os = "macos")]
        let supervisor_environment = self.login_environment()?;

        let session_id = (action.kind == CustomActionKind::Session).then(Uuid::new_v4);
        let marker_path = session_id.map(|id| self.marker_path(id));
        let control_path = session_id.map(|id| self.control_path(id));
        if marker_path.as_ref().is_some_and(|path| path.exists()) {
            return Err("session marker already exists".to_owned());
        }
        let mut control = control_path
            .as_ref()
            .map(|path| create_session_control(path))
            .transpose()?;
        let pending = session_id
            .zip(control.as_ref())
            .map(|(id, (_, identity))| {
                ActionSessionRecord::pending_with_control_identity(
                    id,
                    action.id,
                    worktree,
                    identity.clone(),
                )
            })
            .transpose()
            .map_err(string_error)?;
        if let Some(record) = &pending
            && let Err(error) = self.sessions.upsert(record.clone())
        {
            if let Some((file, identity)) = control.take()
                && let Some(path) = &control_path
            {
                let _ = file.unlock();
                drop(file);
                remove_control_with_identity(path, &identity);
            }
            return Err(string_error(error));
        }
        let mut command = Command::new(&supervisor_executable);
        if let (Some(session_id), Some(marker_path), Some(control_path)) =
            (session_id, marker_path.as_ref(), control_path.as_ref())
        {
            command
                .args(["--session-id", &session_id.to_string(), "--session-file"])
                .arg(marker_path)
                .arg("--control-file")
                .arg(control_path)
                .arg("--control-identity")
                .arg(
                    pending
                        .as_ref()
                        .and_then(ActionSessionRecord::control_identity)
                        .expect("pending session has a control identity"),
                )
                .args(["--action-id", &action.id.to_string(), "--worktree"])
                .arg(worktree);
        }
        command
            .arg("--cwd")
            .arg(&plan.current_directory)
            .arg("--")
            .arg(&plan.executable)
            .args(&plan.arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "macos")]
        apply_supervisor_environment(&mut command, supervisor_environment);
        configure_supervisor_launch(&mut command);

        if let Err(error) = self.revalidate_action_confirmation(confirmation) {
            if let Some((file, _)) = control.take() {
                let _ = file.unlock();
            }
            if let Some(record) = &pending {
                let _ = self.sessions.remove(record.id);
                remove_owned_control(&self.paths.action_session_markers_directory, record);
            }
            return Err(error);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some((file, _)) = control.take() {
                    let _ = file.unlock();
                }
                if let Some(record) = &pending {
                    let _ = self.sessions.remove(record.id);
                    remove_owned_control(&self.paths.action_session_markers_directory, record);
                }
                return Err(string_error(error));
            }
        };
        if let Some((file, _)) = control.take() {
            let _ = file.unlock();
        }
        let record = if let (Some(session_id), Some(marker_path)) =
            (session_id, marker_path.as_ref())
        {
            match wait_for_session_marker(
                marker_path,
                session_id,
                action.id,
                worktree,
                &supervisor_executable,
                &mut child,
            ) {
                Ok((supervisor, marker_identity)) => {
                    let record = pending
                        .clone()
                        .expect("session launch has a pending record")
                        .finalize(supervisor, marker_identity)
                        .map_err(string_error)?;
                    if let Err(error) = self.sessions.upsert(record.clone()) {
                        if let Some(supervisor) = record.supervisor_identity()
                            && stop_verified_supervisor(supervisor).is_err()
                        {
                            watch_action(
                                child,
                                action_key(action.id, worktree),
                                generation,
                                Arc::clone(&self.memory),
                                self.sessions.clone(),
                                Some(record),
                                self.paths.action_session_markers_directory.clone(),
                            );
                            return Err(string_error(error));
                        }
                        self.remove_marker_if_owned(&record);
                        let _ = child.wait();
                        return Err(string_error(error));
                    }
                    Some(record)
                }
                Err(error) => {
                    stop_launched_supervisor(&mut child);
                    if let Some(record) = &pending {
                        let _ = self.sessions.remove(record.id);
                        remove_owned_control(&self.paths.action_session_markers_directory, record);
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };
        let supervisor_identity = match record
            .as_ref()
            .and_then(ActionSessionRecord::supervisor_identity)
        {
            Some(identity) => identity.clone(),
            None => match launched_supervisor_identity(&child, &supervisor_executable) {
                Ok(identity) => identity,
                Err(error) => {
                    stop_launched_supervisor(&mut child);
                    return Err(error);
                }
            },
        };
        let active = ActiveRun {
            generation,
            session_id,
            supervisor_identity,
        };
        {
            let mut memory = self.lock()?;
            memory.active_runs.insert(key.clone(), active);
            memory.action_output.insert(key.clone(), String::new());
            memory.action_runs.insert(
                key.clone(),
                ActionRun {
                    action_id: action.id,
                    worktree_path: canonical_path(Path::new(worktree)),
                    phase: ActionRunPhase::Running,
                    output: String::new(),
                    exit_code: None,
                    managed: true,
                },
            );
        }
        watch_action(
            child,
            key,
            generation,
            Arc::clone(&self.memory),
            self.sessions.clone(),
            record,
            self.paths.action_session_markers_directory.clone(),
        );
        Ok(())
    }

    fn stop_action_inner(
        &self,
        action: &CustomActionDefinition,
        worktree: &str,
        phase: ActionRunPhase,
    ) -> Result<(), String> {
        let key = action_key(action.id, worktree);
        let (session_schema_version, sessions) = self.action_sessions_for_mutation()?;
        let Some(mut record) = sessions.into_iter().find(|record| {
            record.action_id == action.id
                && paths_equal(&record.worktree_path, worktree, path_flavor())
        }) else {
            self.lock()?.active_runs.remove(&key);
            return Ok(());
        };
        if record.is_pending() && session_schema_version == 2 {
            match self.resolve_pending_session(&record)? {
                PendingResolution::Finalized(finalized) => record = finalized,
                PendingResolution::Stale => {
                    self.sessions.remove(record.id).map_err(string_error)?;
                    let mut memory = self.lock()?;
                    memory.active_runs.remove(&key);
                    if let Some(run) = memory.action_runs.get_mut(&key) {
                        run.phase = ActionRunPhase::Stopped;
                        run.exit_code = None;
                    }
                    return Ok(());
                }
                PendingResolution::Unverified => {
                    return Err(
                        "action launch is still pending and cannot be safely stopped".to_owned(),
                    );
                }
            }
        }
        if !self.validate_session(&record, worktree, session_schema_version) {
            if self.exact_supervisor_is_running(&record) {
                return Err("action session exists but cannot be safely verified".to_owned());
            }
            self.remove_marker_if_owned(&record);
            self.sessions.remove(record.id).map_err(string_error)?;
            let mut memory = self.lock()?;
            remove_memory_session(&mut memory, &record);
            return Ok(());
        }
        {
            let mut memory = self.lock()?;
            if let Some(run) = memory.action_runs.get_mut(&key) {
                run.phase = phase;
            }
        }
        let supervisor = record.supervisor_identity().expect("validated identity");
        if let Err(error) = stop_verified_supervisor(supervisor) {
            if let Some(run) = self.lock()?.action_runs.get_mut(&key) {
                run.phase = ActionRunPhase::Running;
            }
            return Err(error);
        }
        self.sessions.remove(record.id).map_err(string_error)?;
        self.remove_marker_if_owned(&record);
        let mut memory = self.lock()?;
        memory.active_runs.remove(&key);
        if let Some(run) = memory.action_runs.get_mut(&key) {
            run.phase = ActionRunPhase::Stopped;
            run.exit_code = None;
        }
        Ok(())
    }

    fn remove_stale_record_for_key(&self, key: &ActionRunKey) -> Result<(), String> {
        let (session_schema_version, sessions) = self.action_sessions_for_mutation()?;
        if let Some(record) = sessions.into_iter().find(|record| {
            record.action_id == key.action_id
                && paths_equal(&record.worktree_path, &key.worktree_path, path_flavor())
        }) {
            if self.validate_session(&record, &key.worktree_path, session_schema_version) {
                return Err("action is already running for this worktree".to_owned());
            }
            if record.is_pending() || self.exact_supervisor_is_running(&record) {
                return Err("a previous action launch cannot be safely verified".to_owned());
            }
            self.remove_marker_if_owned(&record);
            self.sessions.remove(record.id).map_err(string_error)?;
        }
        Ok(())
    }

    pub fn shutdown_for_update(&self) -> Result<(), String> {
        self.shutdown_for_update_with(&supervisor_executable()?)
    }

    fn shutdown_for_update_with(&self, expected_executable: &Path) -> Result<(), String> {
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        if self.update_shutdown.swap(true, Ordering::AcqRel) {
            return Err("Runtime Atlas update shutdown is already active".to_owned());
        }
        let result = (|| {
            let (session_schema_version, stored_sessions) = self.action_sessions_for_mutation()?;
            let active_runs = self
                .lock()?
                .active_runs
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut first_error = None;
            for active in &active_runs {
                if let Err(error) =
                    stop_active_supervisor(&active.supervisor_identity, expected_executable)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            for active in &active_runs {
                if process_identity(active.supervisor_identity.pid).as_ref()
                    == Ok(&active.supervisor_identity)
                    && first_error.is_none()
                {
                    first_error = Some("Runtime Atlas managed action remains active".to_owned());
                }
            }
            for mut record in stored_sessions {
                let result = (|| {
                    if record.is_pending() && session_schema_version == 2 {
                        match self.resolve_pending_session(&record)? {
                            PendingResolution::Finalized(finalized) => record = finalized,
                            PendingResolution::Stale => {
                                self.sessions.remove(record.id).map_err(string_error)?;
                                return Ok(());
                            }
                            PendingResolution::Unverified => {
                                return Err(
                                    "action launch is still pending and cannot be safely stopped"
                                        .to_owned(),
                                );
                            }
                        }
                    }
                    if self.validate_session(&record, &record.worktree_path, session_schema_version)
                    {
                        stop_verified_supervisor(
                            record.supervisor_identity().expect("validated identity"),
                        )?;
                    } else if self.exact_supervisor_is_running(&record) {
                        return Err(
                            "action session exists but cannot be safely verified".to_owned()
                        );
                    }
                    self.remove_marker_if_owned(&record);
                    self.sessions.remove(record.id).map_err(string_error)
                })();
                if let Err(error) = result
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            if !self.action_sessions_for_mutation()?.1.is_empty() {
                return Err("Runtime Atlas managed sessions remain active".to_owned());
            }
            Ok(())
        })();
        if result.is_err() {
            self.cancel_update_shutdown();
        }
        result
    }

    pub fn cancel_update_shutdown(&self) {
        self.update_shutdown.store(false, Ordering::Release);
    }

    fn require_actions_available(&self) -> Result<(), String> {
        (!self.update_shutdown.load(Ordering::Acquire))
            .then_some(())
            .ok_or_else(|| "Runtime Atlas actions are unavailable while updating".to_owned())
    }

    fn add_repository(&self, path: &str) -> Result<(), String> {
        let path = Path::new(path);
        if !path.is_absolute() || !path.is_dir() {
            return Err("repository path must be an existing absolute directory".to_owned());
        }
        let mut command = Command::new(git_executable());
        command
            .args(["--no-optional-locks", "-C"])
            .arg(path)
            .args(["rev-parse", "--show-toplevel"]);
        let output = command_output(&mut command).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                "Git repository inspection timed out.".to_owned()
            } else {
                "Git could not inspect this repository.".to_owned()
            }
        })?;
        if !output.status.success() {
            return Err("Path is not an available Git repository.".to_owned());
        }
        let root = std::str::from_utf8(&output.stdout)
            .map_err(|_| "Git could not inspect this repository.".to_owned())?
            .trim_end_matches(['\r', '\n']);
        if root.is_empty() {
            return Err("Git could not inspect this repository.".to_owned());
        }
        self.store
            .add_repository(Path::new(root))
            .map(|_| ())
            .map_err(string_error)
    }

    fn set_worktree_order(&self, repository_id: Uuid, paths: &[String]) -> Result<(), String> {
        let configuration = self.store.load().map_err(string_error)?.value;
        let repository = inspect_repositories(
            &configuration.repositories,
            &configuration.worktree_order_by_repository,
            git_executable(),
        )
        .into_iter()
        .find(|repository| {
            repository.id == repository_id
                && repository.availability == AvailabilityState::Available
        })
        .ok_or_else(|| "repository is no longer available".to_owned())?;
        let order = expand_worktree_order(paths, &repository.worktrees)
            .ok_or_else(|| "worktree list changed; refresh before reordering".to_owned())?;
        self.store
            .set_worktree_order(repository_id, &order)
            .map_err(string_error)
    }

    fn observed_process(&self, identity: &ProcessIdentity) -> Result<ObservedProcess, String> {
        observe_processes()
            .processes
            .into_iter()
            .find(|process| &process.identity == identity)
            .ok_or_else(|| "process identity is no longer observable".to_owned())
    }

    fn verified_worktree(&self, path: &str) -> Result<String, String> {
        let configuration = self.store.load().map_err(string_error)?.value;
        inspect_repositories(
            &configuration.repositories,
            &configuration.worktree_order_by_repository,
            git_executable(),
        )
        .into_iter()
        .flat_map(|repository| repository.worktrees)
        .find(|worktree| {
            worktree.availability == AvailabilityState::Available
                && paths_equal(&worktree.path, path, path_flavor())
        })
        .map(|worktree| worktree.path)
        .ok_or_else(|| "worktree is no longer available".to_owned())
    }

    fn link_process(
        &self,
        process_identity: ProcessIdentity,
        worktree_path: &str,
    ) -> Result<(), String> {
        self.observed_process(&process_identity)?;
        let worktree_path = self.verified_worktree(worktree_path)?;
        self.store
            .link_process(UserProcessLink {
                process_identity,
                worktree_path,
            })
            .map_err(string_error)
    }

    fn unlink_process(&self, process_identity: &ProcessIdentity) -> Result<(), String> {
        if !process_identity.is_valid() {
            return Err("process identity is invalid".to_owned());
        }
        let configuration = self.store.load().map_err(string_error)?.value;
        if !configuration
            .process_links
            .iter()
            .any(|link| &link.process_identity == process_identity)
        {
            return Err("process link no longer exists".to_owned());
        }
        self.store
            .unlink_process(process_identity)
            .map_err(string_error)
    }

    fn stop_process(
        &self,
        process_identity: &ProcessIdentity,
        worktree_path: &str,
    ) -> Result<(), String> {
        let displayed = {
            let memory = self.lock()?;
            if !memory
                .worktree_paths
                .iter()
                .any(|path| paths_equal(path, worktree_path, path_flavor()))
            {
                return Err("displayed worktree is no longer available".to_owned());
            }
            memory
                .displayed_processes
                .iter()
                .find(|process| &process.identity == process_identity)
                .cloned()
                .ok_or_else(|| "displayed process snapshot is unavailable".to_owned())?
        };
        let current = self.observed_process(process_identity)?;
        let runtime_atlas_identity = current_process_identity()?;
        let plan = plan_termination(
            &TerminationSnapshot {
                process_identity: displayed.identity,
                name: displayed.name,
                cwd: displayed.cwd,
                ports: displayed.ports,
                worktree_path: canonical_path(Path::new(worktree_path)),
                path_flavor: path_flavor(),
            },
            &current,
            &runtime_atlas_identity,
        )
        .map_err(string_error)?;
        terminate_verified(&plan.process_identity)
    }

    fn advance_navigation(
        &self,
        current_path: Option<&str>,
        forward: bool,
    ) -> Result<Option<String>, String> {
        let mut memory = self.lock()?;
        let next = advance_worktree_navigation(
            &memory.worktree_paths,
            current_path,
            &memory.recent_paths,
            memory.navigation.as_ref(),
            if forward {
                WorktreeNavigationDirection::Next
            } else {
                WorktreeNavigationDirection::Previous
            },
        );
        let selected = next
            .as_ref()
            .and_then(|session| session.selected_path())
            .map(str::to_owned);
        memory.navigation = next;
        Ok(selected)
    }

    fn commit_navigation(&self) -> Result<(), String> {
        let mut memory = self.lock()?;
        let Some(selected) = memory
            .navigation
            .as_ref()
            .and_then(|session| session.selected_path())
            .map(str::to_owned)
        else {
            return Ok(());
        };
        memory.recent_paths = record_recent_worktree(&selected, &memory.recent_paths);
        memory.navigation = None;
        Ok(())
    }

    fn cancel_navigation(&self) -> Result<(), String> {
        self.lock()?.navigation = None;
        Ok(())
    }

    fn open_worktree_in_vscode(&self, path: &str) -> Result<(), String> {
        let worktree = self.verified_worktree(path)?;
        open_in_vscode(&worktree)
    }

    fn record_selection(&self, path: &str) -> Result<(), String> {
        let mut memory = self.lock()?;
        let selected = memory
            .worktree_paths
            .iter()
            .find(|candidate| paths_equal(candidate, path, path_flavor()))
            .cloned()
            .ok_or_else(|| "worktree selection is unavailable".to_owned())?;
        memory.recent_paths = record_recent_worktree(&selected, &memory.recent_paths);
        memory.navigation = None;
        Ok(())
    }
}

#[tauri::command]
pub fn runtime_atlas_status(
    state: State<'_, RuntimeAtlasState>,
) -> Result<RuntimeAtlasSnapshot, String> {
    state.status()
}

#[tauri::command]
pub fn runtime_atlas_add_repository(
    state: State<'_, RuntimeAtlasState>,
    path: String,
) -> Result<(), String> {
    state.add_repository(&path)
}

#[tauri::command]
pub fn runtime_atlas_remove_repository(
    state: State<'_, RuntimeAtlasState>,
    repository_id: String,
) -> Result<(), String> {
    let _operation = state
        .action_operation
        .lock()
        .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
    state
        .store
        .remove_repository(Uuid::parse_str(&repository_id).map_err(string_error)?)
        .map_err(string_error)
}

#[tauri::command]
pub fn runtime_atlas_set_language(
    state: State<'_, RuntimeAtlasState>,
    language: AppLanguage,
) -> Result<(), String> {
    state.store.set_app_language(language).map_err(string_error)
}

#[tauri::command]
pub fn runtime_atlas_save_action(
    state: State<'_, RuntimeAtlasState>,
    action: CustomActionDefinition,
) -> Result<(), String> {
    let _operation = state
        .action_operation
        .lock()
        .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
    state.store.save_custom_action(action).map_err(string_error)
}

#[tauri::command]
pub fn runtime_atlas_delete_action(
    state: State<'_, RuntimeAtlasState>,
    action_id: String,
) -> Result<(), String> {
    let _operation = state
        .action_operation
        .lock()
        .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
    state
        .store
        .remove_custom_action(Uuid::parse_str(&action_id).map_err(string_error)?)
        .map_err(string_error)
}

#[tauri::command]
pub fn runtime_atlas_plan_action(
    state: State<'_, RuntimeAtlasState>,
    action_id: String,
    worktree_path: String,
    values: BTreeMap<String, ActionInputValue>,
    restart: bool,
) -> Result<ActionConfirmationPlan, String> {
    state.plan_action(
        Uuid::parse_str(&action_id).map_err(string_error)?,
        &worktree_path,
        values,
        restart,
    )
}

#[tauri::command]
pub fn runtime_atlas_confirm_action(
    state: State<'_, RuntimeAtlasState>,
    confirmation_token: String,
) -> Result<(), String> {
    state.confirm_action(Uuid::parse_str(&confirmation_token).map_err(string_error)?)
}

#[tauri::command]
pub fn runtime_atlas_set_worktree_order(
    state: State<'_, RuntimeAtlasState>,
    repository_id: String,
    keys: Vec<String>,
) -> Result<(), String> {
    state.set_worktree_order(
        Uuid::parse_str(&repository_id).map_err(string_error)?,
        &keys,
    )
}

#[tauri::command]
pub fn runtime_atlas_stop_action(
    state: State<'_, RuntimeAtlasState>,
    action_id: String,
    worktree_path: String,
) -> Result<(), String> {
    state.stop_action(
        Uuid::parse_str(&action_id).map_err(string_error)?,
        &worktree_path,
    )
}

#[tauri::command]
pub fn runtime_atlas_stop_process(
    state: State<'_, RuntimeAtlasState>,
    process_identity: ProcessIdentity,
    worktree_path: String,
) -> Result<(), String> {
    state.stop_process(&process_identity, &worktree_path)
}

#[tauri::command]
pub fn runtime_atlas_link_process(
    state: State<'_, RuntimeAtlasState>,
    process_identity: ProcessIdentity,
    worktree_path: String,
) -> Result<(), String> {
    state.link_process(process_identity, &worktree_path)
}

#[tauri::command]
pub fn runtime_atlas_unlink_process(
    state: State<'_, RuntimeAtlasState>,
    process_identity: ProcessIdentity,
) -> Result<(), String> {
    state.unlink_process(&process_identity)
}

#[tauri::command]
pub fn runtime_atlas_advance_worktree_navigation(
    state: State<'_, RuntimeAtlasState>,
    current_path: Option<String>,
    forward: bool,
) -> Result<Option<String>, String> {
    state.advance_navigation(current_path.as_deref(), forward)
}

#[tauri::command]
pub fn runtime_atlas_commit_worktree_navigation(
    state: State<'_, RuntimeAtlasState>,
) -> Result<(), String> {
    state.commit_navigation()
}

#[tauri::command]
pub fn runtime_atlas_cancel_worktree_navigation(
    state: State<'_, RuntimeAtlasState>,
) -> Result<(), String> {
    state.cancel_navigation()
}

#[tauri::command]
pub fn runtime_atlas_record_worktree_selection(
    state: State<'_, RuntimeAtlasState>,
    path: String,
) -> Result<(), String> {
    state.record_selection(&path)
}

#[tauri::command]
pub fn runtime_atlas_open_worktree_in_vscode(
    state: State<'_, RuntimeAtlasState>,
    path: String,
) -> Result<(), String> {
    state.open_worktree_in_vscode(&path)
}

#[cfg(target_os = "macos")]
fn open_in_vscode(path: &str) -> Result<(), String> {
    let mut command = Command::new("/usr/bin/open");
    command.args(["-b", "com.microsoft.VSCode", "--"]).arg(path);
    let output = command_output(&mut command).map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            "VS Code launch timed out".to_owned()
        } else {
            format!("VS Code could not be opened: {error}")
        }
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err("VS Code could not be opened through its registered application identifier".to_owned())
    }
}

#[cfg(windows)]
fn open_in_vscode(path: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let uri: Vec<u16> = std::ffi::OsStr::new(&vscode_uri(path))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let operation: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            uri.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as usize > 32 {
        Ok(())
    } else {
        Err("VS Code could not be opened through the registered vscode URL handler".to_owned())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn open_in_vscode(_path: &str) -> Result<(), String> {
    Err("opening VS Code is supported only on macOS and Windows".to_owned())
}

#[cfg(any(test, windows))]
fn vscode_uri(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut uri = String::from("vscode://file/");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(uri, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    uri
}

#[cfg(target_os = "macos")]
fn git_executable() -> &'static Path {
    Path::new("/usr/bin/git")
}

#[cfg(not(target_os = "macos"))]
fn git_executable() -> &'static Path {
    Path::new("git")
}

#[cfg(target_os = "windows")]
fn path_flavor() -> PathFlavor {
    PathFlavor::Windows
}

#[cfg(not(target_os = "windows"))]
fn path_flavor() -> PathFlavor {
    PathFlavor::MacOs
}

fn current_process_identity() -> Result<ProcessIdentity, String> {
    process_identity(std::process::id())
}

#[cfg(target_os = "macos")]
fn terminate_verified(expected: &ProcessIdentity) -> Result<(), String> {
    if process_identity(expected.pid).as_ref() != Ok(expected) {
        return Err("process identity changed before termination".to_owned());
    }
    let pid = i32::try_from(expected.pid).map_err(string_error)?;
    // SAFETY: the PID and start identity were just revalidated; SIGTERM is non-forcing.
    if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(target_os = "windows")]
fn windows_identity_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    pid: u32,
) -> Result<ProcessIdentity, String> {
    use std::mem::zeroed;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let mut creation: FILETIME = unsafe { zeroed() };
    let mut exit: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    // SAFETY: all FILETIME pointers are valid writable values.
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err("process identity is unavailable".to_owned());
    }
    let created = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
    if created == 0 {
        return Err("process identity is unavailable".to_owned());
    }
    Ok(ProcessIdentity {
        pid,
        start_identity: format!("windows:{created}"),
    })
}

#[cfg(target_os = "windows")]
fn terminate_verified(expected: &ProcessIdentity) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
    };

    // SAFETY: OpenProcess receives an observed numeric PID and no borrowed pointers.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            expected.pid,
        )
    };
    if handle.is_null() {
        return Err("process could not be opened for termination".to_owned());
    }
    let result = match windows_identity_from_handle(handle, expected.pid) {
        Ok(current) if &current == expected => {
            // SAFETY: the open handle's creation time matches the expected process identity.
            (unsafe { TerminateProcess(handle, 1) } != 0)
                .then_some(())
                .ok_or_else(|| std::io::Error::last_os_error().to_string())
        }
        Ok(_) => Err("process identity changed before termination".to_owned()),
        Err(error) => Err(error),
    };
    // SAFETY: `handle` is owned by this function and closed once.
    unsafe { CloseHandle(handle) };
    result
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn terminate_verified(_expected: &ProcessIdentity) -> Result<(), String> {
    Err("process termination is unsupported on this platform".to_owned())
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(target_os = "macos")]
fn resolve_login_environment() -> Result<Vec<(OsString, OsString)>, String> {
    use std::{ffi::CStr, os::unix::ffi::OsStringExt};

    let entry = unsafe { libc::getpwuid(libc::geteuid()) };
    if entry.is_null() {
        return Err("login account is unavailable".to_owned());
    }
    let shell = unsafe { (*entry).pw_shell };
    if shell.is_null() {
        return Err("login shell is unavailable".to_owned());
    }
    let shell = OsString::from_vec(unsafe { CStr::from_ptr(shell) }.to_bytes().to_vec());
    if shell.is_empty() {
        return Err("login shell is unavailable".to_owned());
    }
    read_login_environment(&shell, Duration::from_secs(15))
}

#[cfg(target_os = "macos")]
fn read_login_environment(
    shell: &std::ffi::OsStr,
    timeout: Duration,
) -> Result<Vec<(OsString, OsString)>, String> {
    use std::os::unix::ffi::OsStringExt;

    const MAX_ENVIRONMENT_BYTES: usize = 1024 * 1024;
    let output = output_with_timeout(
        Command::new(shell).args(["-l", "-c", "/usr/bin/env -0"]),
        timeout,
    )
    .map_err(|error| format!("login shell environment could not be loaded: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "login shell environment exited with {}",
            output.status
        ));
    }
    if output.stdout.len() > MAX_ENVIRONMENT_BYTES {
        return Err("login shell environment is too large".to_owned());
    }
    if !output.stdout.is_empty() && !output.stdout.ends_with(&[0]) {
        return Err("login shell environment is not NUL-delimited".to_owned());
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let separator = entry
                .iter()
                .position(|byte| *byte == b'=')
                .ok_or_else(|| "login shell emitted non-environment output".to_owned())?;
            let key = &entry[..separator];
            if key.is_empty()
                || !key[0].is_ascii_alphabetic() && key[0] != b'_'
                || !key[1..]
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                return Err("login shell emitted an invalid environment name".to_owned());
            }
            Ok((
                OsString::from_vec(key.to_vec()),
                OsString::from_vec(entry[(separator + 1)..].to_vec()),
            ))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn apply_supervisor_environment(command: &mut Command, environment: &[(OsString, OsString)]) {
    command
        .env_clear()
        .envs(environment.iter().map(|(key, value)| (key, value)));
}

fn system_language() -> AppLanguage {
    sys_locale::get_locale()
        .map(|locale| AppLanguage::preferred(&[locale]))
        .unwrap_or_default()
}

fn action_key(action_id: Uuid, worktree_path: &str) -> ActionRunKey {
    ActionRunKey {
        action_id,
        worktree_path: canonical_path(Path::new(worktree_path)),
    }
}

fn remove_memory_session(memory: &mut RuntimeMemory, record: &ActionSessionRecord) {
    let key = action_key(record.action_id, &record.worktree_path);
    if memory
        .active_runs
        .get(&key)
        .is_some_and(|run| run.session_id == Some(record.id))
    {
        memory.active_runs.remove(&key);
        if let Some(run) = memory.action_runs.get_mut(&key)
            && matches!(
                run.phase,
                ActionRunPhase::Running | ActionRunPhase::Restarting | ActionRunPhase::Stopping
            )
        {
            run.phase = ActionRunPhase::Stopped;
        }
    }
}

fn prepare_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(string_error)?;
    let metadata = fs::symlink_metadata(path).map_err(string_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("session marker directory must be a real directory".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(string_error)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("session marker directory must not be a reparse point".to_owned());
        }
    }
    Ok(())
}

fn create_session_control(path: &Path) -> Result<(File, String), String> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    let mut file = options.open(path).map_err(string_error)?;
    let identity = marker_file_identity(&file);
    let result = identity.and_then(|identity| {
        file.write_all(&[0]).map_err(string_error)?;
        file.sync_all().map_err(string_error)?;
        file.lock_shared().map_err(string_error)?;
        Ok(identity)
    });
    match result {
        Ok(identity) => Ok((file, identity)),
        Err(error) => {
            let identity = marker_file_identity(&file).ok();
            let _ = file.unlock();
            drop(file);
            if let Some(identity) = identity {
                remove_control_with_identity(path, &identity);
            }
            Err(error)
        }
    }
}

fn wait_for_session_marker(
    path: &Path,
    session_id: Uuid,
    action_id: Uuid,
    worktree_path: &str,
    expected_executable: &Path,
    child: &mut Child,
) -> Result<(ProcessIdentity, String), String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_error = None;
    loop {
        match fs::symlink_metadata(path) {
            Ok(_) => match read_session_marker(path) {
                Ok((marker, marker_identity)) => {
                    let supervisor = ProcessIdentity {
                        pid: marker.supervisor_pid,
                        start_identity: marker.start_identity.clone(),
                    };
                    if marker.schema_version == 2
                        && marker.session_id == session_id
                        && marker
                            .registration()
                            .is_some_and(|(marker_action, marker_worktree)| {
                                marker_action == action_id
                                    && paths_equal(marker_worktree, worktree_path, path_flavor())
                            })
                        && marker.supervisor_pid == child.id()
                        && process_identity(child.id()).ok().as_ref() == Some(&supervisor)
                        && supervisor_executable_matches(&supervisor, expected_executable)
                    {
                        return Ok((supervisor, marker_identity));
                    }
                    last_error =
                        Some("session marker does not match the launched supervisor".to_owned());
                }
                Err(error) => last_error = Some(error),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if let Some(status) = child.try_wait().map_err(string_error)? {
            if status.code() == Some(71) {
                if let Some(stderr) = child.stderr.take() {
                    let mut data = Vec::new();
                    stderr
                        .take(4097)
                        .read_to_end(&mut data)
                        .map_err(string_error)?;
                    if data.len() <= 4096 {
                        let detail = sanitize_output(&String::from_utf8_lossy(&data));
                        let detail = detail.trim();
                        if !detail.is_empty() {
                            return Err(format!("action supervisor failed: {detail}"));
                        }
                    }
                }
                return Err(
                    "action executable could not be resolved safely by the supervisor".to_owned(),
                );
            }
            return Err(last_error.unwrap_or_else(|| {
                "action supervisor exited before creating its session marker".to_owned()
            }));
        }
        if Instant::now() >= deadline {
            return Err(last_error.unwrap_or_else(|| {
                "action supervisor did not create its session marker".to_owned()
            }));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn supervisor_executable() -> Result<PathBuf, String> {
    supervisor_executable_for(&std::env::current_exe().map_err(string_error)?)
}

fn supervisor_executable_for(app_executable: &Path) -> Result<PathBuf, String> {
    let directory = app_executable
        .parent()
        .ok_or_else(|| "application executable has no parent directory".to_owned())?;
    #[cfg(windows)]
    let name = "runtime-atlas-supervisor.exe";
    #[cfg(not(windows))]
    let name = "runtime-atlas-supervisor";
    Ok(directory.join(name))
}

#[cfg(windows)]
fn configure_supervisor_launch(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn configure_supervisor_launch(_command: &mut Command) {}

fn stop_launched_supervisor(child: &mut Child) {
    let stopped = process_identity(child.id())
        .and_then(|identity| stop_verified_supervisor(&identity))
        .is_ok();
    if !stopped {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn launched_supervisor_identity(
    child: &Child,
    expected_executable: &Path,
) -> Result<ProcessIdentity, String> {
    let identity = process_identity(child.id())?;
    if !supervisor_executable_matches(&identity, expected_executable) {
        return Err("launched action supervisor could not be safely verified".to_owned());
    }
    Ok(identity)
}

fn stop_active_supervisor(
    expected: &ProcessIdentity,
    expected_executable: &Path,
) -> Result<(), String> {
    if process_identity(expected.pid).as_ref() != Ok(expected) {
        return Ok(());
    }
    if !supervisor_executable_matches(expected, expected_executable) {
        return Err("active action supervisor could not be safely verified".to_owned());
    }
    match stop_verified_supervisor(expected) {
        Ok(()) => Ok(()),
        Err(_) if process_identity(expected.pid).as_ref() != Ok(expected) => Ok(()),
        Err(error) => Err(error),
    }
}

fn watch_action(
    mut child: Child,
    key: ActionRunKey,
    generation: Uuid,
    memory: Arc<Mutex<RuntimeMemory>>,
    sessions: ActionSessionStore,
    record: Option<ActionSessionRecord>,
    marker_directory: PathBuf,
) {
    let stdout = child.stdout.take().map(|stdout| {
        let memory = Arc::clone(&memory);
        let key = key.clone();
        thread::spawn(move || capture_output(stdout, &memory, &key, generation))
    });
    let stderr = child.stderr.take().map(|stderr| {
        let memory = Arc::clone(&memory);
        let key = key.clone();
        thread::spawn(move || capture_output(stderr, &memory, &key, generation))
    });
    thread::spawn(move || {
        let status = child.wait();
        if let Some(reader) = stdout {
            let _ = reader.join();
        }
        if let Some(reader) = stderr {
            let _ = reader.join();
        }
        if let Some(record) = &record {
            let _ = sessions.remove(record.id);
            remove_owned_marker(&marker_directory, record);
        }
        let Ok(mut memory) = memory.lock() else {
            return;
        };
        let same_run = memory
            .active_runs
            .get(&key)
            .is_some_and(|active| active.generation == generation);
        if !same_run {
            return;
        }
        memory.active_runs.remove(&key);
        if let Some(run) = memory.action_runs.get_mut(&key) {
            if matches!(
                run.phase,
                ActionRunPhase::Stopping | ActionRunPhase::Restarting
            ) {
                run.phase = ActionRunPhase::Stopped;
                run.exit_code = None;
            } else {
                match status {
                    Ok(status) if status.success() => {
                        run.phase = ActionRunPhase::Succeeded;
                        run.exit_code = status.code();
                    }
                    Ok(status) => {
                        run.phase = ActionRunPhase::Failed;
                        run.exit_code = status.code();
                    }
                    Err(error) => {
                        run.phase = ActionRunPhase::Failed;
                        run.exit_code = None;
                        append_bounded(
                            memory.action_output.entry(key).or_default(),
                            &error.to_string(),
                        );
                    }
                }
            }
        }
    });
}

fn capture_output(
    mut reader: impl Read,
    memory: &Arc<Mutex<RuntimeMemory>>,
    key: &ActionRunKey,
    generation: Uuid,
) {
    stream_sanitized_output(&mut reader, 4096, |text| {
        let Ok(mut memory) = memory.lock() else {
            return;
        };
        if memory
            .active_runs
            .get(key)
            .is_some_and(|active| active.generation == generation)
        {
            append_bounded(memory.action_output.entry(key.clone()).or_default(), text);
        }
    });
}

fn stream_sanitized_output(mut reader: impl Read, chunk_size: usize, mut emit: impl FnMut(&str)) {
    let mut buffer = vec![0u8; chunk_size.max(1)];
    let mut line = Vec::new();
    let mut omitted = false;
    while let Ok(size) = reader.read(&mut buffer) {
        if size == 0 {
            break;
        }
        for byte in &buffer[..size] {
            if *byte == b'\n' {
                if omitted {
                    emit("<output omitted>\n");
                } else {
                    line.push(*byte);
                    emit(&sanitize_output(&String::from_utf8_lossy(&line)));
                }
                line.clear();
                omitted = false;
            } else if !omitted {
                if line.len() < 32_000 {
                    line.push(*byte);
                } else {
                    line.clear();
                    omitted = true;
                }
            }
        }
    }
    if omitted {
        emit("<output omitted>");
    } else if !line.is_empty() {
        emit(&sanitize_output(&String::from_utf8_lossy(&line)));
    }
}

fn append_bounded(output: &mut String, value: &str) {
    output.push_str(value);
    if output.len() > 32_000 {
        let mut start = output.len() - 32_000;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        output.drain(..start);
    }
}

fn remove_owned_marker(directory: &Path, record: &ActionSessionRecord) {
    let path = directory.join(format!("{}.json", record.id));
    if let Some(expected) = record.marker_identity() {
        remove_marker_with_identity(&path, record.id, expected);
    }
    remove_owned_control(directory, record);
}

fn remove_owned_control(directory: &Path, record: &ActionSessionRecord) {
    if let Some(expected) = record.control_identity() {
        remove_control_with_identity(&directory.join(format!("{}.control", record.id)), expected);
    }
}

fn remove_control_with_identity(path: &Path, expected: &str) {
    remove_verified_file(path, |file| {
        marker_file_identity(file).as_deref() == Ok(expected)
    });
}

fn remove_marker_with_identity(path: &Path, session_id: Uuid, expected: &str) {
    remove_verified_file(path, |file| {
        if marker_file_identity(file).as_deref() != Ok(expected) {
            return false;
        }
        let mut data = Vec::new();
        file.seek(SeekFrom::Start(0)).is_ok()
            && file.take(4097).read_to_end(&mut data).is_ok()
            && data.len() <= 4096
            && serde_json::from_slice::<SessionMarker>(&data)
                .is_ok_and(|marker| marker.session_id == session_id)
    });
}

fn remove_verified_file(path: &Path, verify: impl FnOnce(&mut File) -> bool) -> bool {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
            io::{AsRawFd, FromRawFd},
        };

        let Some(parent_path) = path.parent().filter(|value| !value.as_os_str().is_empty()) else {
            return false;
        };
        let Some(name) = path.file_name() else {
            return false;
        };
        let Ok(name) = CString::new(name.as_bytes()) else {
            return false;
        };
        let mut parent_options = OpenOptions::new();
        parent_options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let Ok(parent) = parent_options.open(parent_path) else {
            return false;
        };
        let Ok(parent_metadata) = parent.metadata() else {
            return false;
        };
        if !parent_metadata.is_dir()
            || parent_metadata.uid() != unsafe { libc::geteuid() }
            || parent_metadata.mode() & 0o077 != 0
        {
            return false;
        }
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if descriptor < 0 {
            return false;
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || !verify(&mut file)
        {
            return false;
        }
        // macOS has no public unlink-by-handle API. The app-local 0700 directory,
        // single-host lease, and supervisor protocol exclude cooperating replacement;
        // the dirfd keeps ancestor changes outside this relative unlink.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return false;
        }
        let _ = parent.sync_all();
        true
    }
    #[cfg(windows)]
    {
        use std::mem::size_of;
        use std::os::windows::{fs::MetadataExt, fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, HANDLE},
            Storage::FileSystem::{
                DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
                FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
                FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                FileDispositionInfoEx, SetFileInformationByHandle,
            },
        };

        let mut options = OpenOptions::new();
        options
            .read(true)
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let Ok(mut file) = options.open(path) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !verify(&mut file)
        {
            return false;
        }
        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfoEx,
                (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            ) != 0
        }
    }
}

fn wait_until_stopped(expected: &ProcessIdentity, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if process_identity(expected.pid).as_ref() != Ok(expected) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn stop_verified_supervisor(expected: &ProcessIdentity) -> Result<(), String> {
    if process_identity(expected.pid).as_ref() != Ok(expected) {
        return Err("supervisor identity changed before termination".to_owned());
    }
    let pid = i32::try_from(expected.pid).map_err(string_error)?;
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0
        && process_identity(expected.pid).as_ref() == Ok(expected)
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    wait_until_stopped(expected, Duration::from_secs(3))
        .then_some(())
        .ok_or_else(|| "action supervisor did not stop after SIGTERM".to_owned())
}

#[cfg(target_os = "windows")]
fn stop_verified_supervisor(expected: &ProcessIdentity) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
        SetConsoleCtrlHandler,
    };

    if process_identity(expected.pid).as_ref() != Ok(expected) {
        return Err("supervisor identity changed before termination".to_owned());
    }
    let attached = unsafe { AttachConsole(expected.pid) } != 0;
    if attached {
        unsafe { SetConsoleCtrlHandler(None, TRUE) };
        let _ = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, expected.pid) };
        unsafe {
            FreeConsole();
            SetConsoleCtrlHandler(None, FALSE);
        }
    }
    if wait_until_stopped(expected, Duration::from_secs(3)) {
        return Ok(());
    }
    terminate_verified(expected)?;
    wait_until_stopped(expected, Duration::from_secs(1))
        .then_some(())
        .ok_or_else(|| "action supervisor did not stop".to_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn stop_verified_supervisor(_expected: &ProcessIdentity) -> Result<(), String> {
    Err("action supervision is unsupported on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime_atlas_core::models::{
        CustomActionInputDefinition, CustomActionInputKind, DiscoveryAvailability,
    };
    use runtime_atlas_core::relations::UserProcessLink;
    use tempfile::{TempDir, tempdir};

    fn empty_process_observation() -> ProcessObservation {
        ProcessObservation {
            availability: DiscoveryAvailability::available(),
            processes: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn empty_docker_observation() -> DockerObservation {
        DockerObservation {
            availability: DiscoveryAvailability::available(),
            containers: Vec::new(),
            notices: Vec::new(),
        }
    }

    #[test]
    #[cfg(windows)]
    fn control_creation_allows_exact_handoff_then_blocks_replacement() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{
                DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
        };

        let directory = tempdir().unwrap();
        let path = directory.path().join("session.control");
        let (host, expected) = create_session_control(&path).unwrap();
        let supervisor = OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .unwrap();
        assert_eq!(marker_file_identity(&supervisor).unwrap(), expected);

        host.unlock().unwrap();
        drop(host);
        assert!(fs::rename(&path, directory.path().join("replacement.control")).is_err());
        assert!(fs::remove_file(&path).is_err());
        assert_eq!(marker_file_identity(&supervisor).unwrap(), expected);
        assert_eq!(fs::read(&path).unwrap(), [0]);

        drop(supervisor);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn verified_cleanup_removes_owned_files_and_preserves_prior_replacements() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let control = directory.path().join("session.control");
        fs::write(&control, [0]).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
        let expected = marker_file_identity(&open_session_control(&control).unwrap()).unwrap();
        remove_control_with_identity(&control, &expected);
        assert!(!control.exists());

        fs::write(&control, [0]).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
        let owned = directory.path().join("owned.control");
        let expected = marker_file_identity(&open_session_control(&control).unwrap()).unwrap();
        fs::rename(&control, &owned).unwrap();
        fs::write(&control, [9]).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
        remove_control_with_identity(&control, &expected);
        assert_eq!(fs::read(&control).unwrap(), [9]);
        assert_eq!(fs::read(&owned).unwrap(), [0]);

        let session_id = Uuid::new_v4();
        let marker = directory.path().join("session.json");
        fs::write(
            &marker,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "sessionId": session_id,
                "actionID": null,
                "worktreePath": null,
                "supervisorPID": 1,
                "startIdentity": "fixture",
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        let expected = marker_file_identity(&File::open(&marker).unwrap()).unwrap();
        remove_marker_with_identity(&marker, Uuid::new_v4(), &expected);
        assert!(marker.exists());
        remove_marker_with_identity(&marker, session_id, &expected);
        assert!(!marker.exists());
    }

    #[test]
    #[cfg(windows)]
    fn verified_cleanup_deletes_the_open_file_not_a_racing_replacement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.control");
        let moved = directory.path().join("owned.control");
        fs::write(&path, [0]).unwrap();
        let expected = marker_file_identity(&open_session_control(&path).unwrap()).unwrap();

        assert!(remove_verified_file(&path, |file| {
            assert_eq!(marker_file_identity(file).unwrap(), expected);
            fs::rename(&path, &moved).unwrap();
            fs::write(&path, [9]).unwrap();
            true
        }));
        assert!(!moved.exists());
        assert_eq!(fs::read(&path).unwrap(), [9]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn host_caches_login_environment_and_applies_it_before_supervisor_launch() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let shell = directory.path().join("login-shell");
        fs::write(
            &shell,
            "#!/bin/sh\n[ \"$1\" = -l ] && [ \"$2\" = -c ] && [ \"$3\" = '/usr/bin/env -0' ] || exit 9\nprintf 'PATH=/fixture/bin\\0RUNTIME_ATLAS_HOST_ENV=fixture-value\\0'\n",
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        let environment =
            read_login_environment(shell.as_os_str(), Duration::from_secs(1)).unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        state.login_environment.set(Ok(environment)).unwrap();
        let first = state.login_environment().unwrap();
        let second = state.login_environment().unwrap();
        assert!(std::ptr::eq(first, second));

        let mut command = Command::new("/usr/bin/env");
        command.arg("-0");
        apply_supervisor_environment(&mut command, first);
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert!(
            output
                .stdout
                .split(|byte| *byte == 0)
                .any(|entry| entry == b"RUNTIME_ATLAS_HOST_ENV=fixture-value")
        );
    }

    fn action_confirmation_fixture() -> (TempDir, RuntimeAtlasState, CustomActionDefinition, String)
    {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository");
        assert!(
            Command::new(git_executable())
                .arg("init")
                .arg(&repository)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(git_executable())
                .arg("-C")
                .arg(&repository)
                .args([
                    "-c",
                    "user.name=Runtime Atlas Test",
                    "-c",
                    "user.email=runtime-atlas@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "initial",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        state.add_repository(repository.to_str().unwrap()).unwrap();
        let repository_id = state.store.load().unwrap().value.repositories[0].id;
        let mut action = CustomActionDefinition::new(repository_id, "Echo", "echo {{message}}");
        action.effects = vec!["Print the reviewed message".into()];
        action.inputs.push(CustomActionInputDefinition {
            id: Uuid::new_v4(),
            key: "message".into(),
            label: "Message".into(),
            kind: CustomActionInputKind::Text,
            flag_argument: None,
            is_enabled_by_default: false,
        });
        state.store.save_custom_action(action.clone()).unwrap();
        (directory, state, action, canonical_path(&repository))
    }

    #[test]
    fn temp_state_holds_the_shared_process_lease_and_composes_empty_status() {
        let directory = tempdir().unwrap();
        let data = directory.path().join("Runtime Atlas");
        let state = RuntimeAtlasState::new(data.clone(), AppLanguage::English).unwrap();
        assert!(RuntimeAtlasState::new(data.clone(), AppLanguage::English).is_err());

        let snapshot = state
            .compose_status(empty_process_observation(), empty_docker_observation())
            .unwrap();
        assert!(snapshot.repositories.is_empty());
        assert_eq!(snapshot.language, AppLanguage::English);
        drop(state);
        assert!(RuntimeAtlasState::new(data, AppLanguage::English).is_ok());
    }

    #[test]
    fn navigation_uses_only_the_last_verified_worktree_set() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        #[cfg(target_os = "windows")]
        let (first, second, unknown) = (r"C:\repo\a", r"C:\repo\b", r"C:\repo\unknown");
        #[cfg(not(target_os = "windows"))]
        let (first, second, unknown) = ("/repo/a", "/repo/b", "/repo/unknown");
        {
            let mut memory = state.lock().unwrap();
            memory.worktree_paths = vec![first.into(), second.into()];
        }
        state.commit_navigation().unwrap();
        let selected = state.advance_navigation(Some(first), true).unwrap();
        assert_eq!(selected.as_deref(), Some(second));
        state.cancel_navigation().unwrap();
        assert!(state.lock().unwrap().navigation.is_none());
        assert!(state.lock().unwrap().recent_paths.is_empty());
        let selected = state.advance_navigation(Some(first), true).unwrap();
        assert_eq!(selected.as_deref(), Some(second));
        state.commit_navigation().unwrap();
        state.commit_navigation().unwrap();
        assert_eq!(state.lock().unwrap().recent_paths[0], second);
        state.record_selection(first).unwrap();
        assert_eq!(state.lock().unwrap().recent_paths[0], first);
        assert!(state.record_selection(unknown).is_err());
    }

    #[test]
    fn vscode_uri_preserves_path_structure_and_encodes_data() {
        assert_eq!(
            vscode_uri(r"C:\repo folder\한글#branch"),
            "vscode://file/C:/repo%20folder/%ED%95%9C%EA%B8%80%23branch"
        );
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn repository_paths_keep_trailing_spaces() {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("repository ");
        std::fs::create_dir(&repository).unwrap();
        assert!(
            Command::new(git_executable())
                .arg("init")
                .arg(&repository)
                .status()
                .unwrap()
                .success()
        );
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        state.add_repository(repository.to_str().unwrap()).unwrap();
        assert_eq!(
            state.store.load().unwrap().value.repositories[0].path,
            canonical_path(&repository)
        );
    }

    #[test]
    fn action_confirmation_reviews_substitution_and_is_one_use() {
        let (_directory, state, action, worktree) = action_confirmation_fixture();
        let preview = state
            .plan_action(
                action.id,
                &worktree,
                BTreeMap::from([(
                    "message".into(),
                    ActionInputValue::Text("hello world".into()),
                )]),
                false,
            )
            .unwrap();
        assert_eq!(preview.display_command, "echo 'hello world'");
        assert_eq!(preview.worktree_path, worktree);
        assert_eq!(preview.effects, action.effects);

        let confirmation = state
            .take_action_confirmation(preview.confirmation_token)
            .unwrap();
        state.revalidate_action_confirmation(&confirmation).unwrap();
        assert!(
            state
                .take_action_confirmation(preview.confirmation_token)
                .is_err()
        );

        let expired = state
            .plan_action(
                action.id,
                &worktree,
                BTreeMap::from([("message".into(), ActionInputValue::Text("expired".into()))]),
                false,
            )
            .unwrap();
        state
            .lock()
            .unwrap()
            .action_confirmations
            .get_mut(&expired.confirmation_token)
            .unwrap()
            .expires_at = Instant::now();
        assert!(
            state
                .take_action_confirmation(expired.confirmation_token)
                .is_err()
        );
    }

    #[test]
    fn action_confirmation_rejects_a_changed_contract_and_consumes_the_token() {
        let (_directory, state, mut action, worktree) = action_confirmation_fixture();
        let preview = state
            .plan_action(
                action.id,
                &worktree,
                BTreeMap::from([("message".into(), ActionInputValue::Text("reviewed".into()))]),
                false,
            )
            .unwrap();
        action.effects.push("Changed after review".into());
        state.store.save_custom_action(action).unwrap();

        assert!(state.confirm_action(preview.confirmation_token).is_err());
        assert!(state.confirm_action(preview.confirmation_token).is_err());
    }

    #[test]
    fn malformed_sessions_fail_closed_for_stop_run_and_update() {
        let (_directory, state, mut action, worktree) = action_confirmation_fixture();
        action.kind = CustomActionKind::Session;
        state.store.save_custom_action(action.clone()).unwrap();
        let preview = state
            .plan_action(
                action.id,
                &worktree,
                BTreeMap::from([("message".into(), ActionInputValue::Text("hello".into()))]),
                false,
            )
            .unwrap();
        let malformed = b"{broken";
        fs::write(&state.paths.action_sessions_file, malformed).unwrap();
        let unchanged = |result: Result<(), String>| {
            assert_eq!(
                result.unwrap_err(),
                "action session data is damaged; managed actions cannot be changed safely"
            );
            assert_eq!(
                fs::read(&state.paths.action_sessions_file).unwrap(),
                malformed
            );
        };

        unchanged(state.stop_action(action.id, &worktree));
        unchanged(state.confirm_action(preview.confirmation_token));
        unchanged(state.shutdown_for_update());
        assert!(state.require_actions_available().is_ok());

        let status = state
            .compose_status(empty_process_observation(), empty_docker_observation())
            .unwrap();
        assert!(
            status
                .notices
                .iter()
                .any(|notice| notice.message.contains("command session file is damaged"))
        );
        assert_eq!(
            fs::read(&state.paths.action_sessions_file).unwrap(),
            malformed
        );
    }

    #[test]
    fn stale_links_can_be_removed() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let identity = ProcessIdentity {
            pid: 42,
            start_identity: "gone-process".into(),
        };
        state
            .store
            .link_process(UserProcessLink {
                process_identity: identity.clone(),
                worktree_path: canonical_path(directory.path()),
            })
            .unwrap();
        state.unlink_process(&identity).unwrap();
        assert!(state.store.load().unwrap().value.process_links.is_empty());
    }

    #[test]
    fn session_restore_rejects_a_replaced_marker_file() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let session_id = Uuid::new_v4();
        let action_id = Uuid::new_v4();
        let supervisor = current_process_identity().unwrap();
        let (_control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let marker_path = state.marker_path(session_id);
        let marker = serde_json::json!({
            "schemaVersion": 2,
            "sessionId": session_id,
            "actionID": action_id,
            "worktreePath": canonical_path(directory.path()),
            "supervisorPID": supervisor.pid,
            "startIdentity": supervisor.start_identity,
        });
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let marker_identity = read_session_marker(&marker_path).unwrap().1;
        let record = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action_id,
            directory.path(),
            control_identity,
        )
        .and_then(|record| record.finalize(supervisor, marker_identity))
        .unwrap();
        let current_executable = std::env::current_exe().unwrap();
        assert!(state.validate_session_with(
            &record,
            directory.path().to_str().unwrap(),
            2,
            &current_executable,
        ));
        assert!(!state.validate_session_with(
            &record,
            directory.path().to_str().unwrap(),
            1,
            &current_executable,
        ));

        fs::rename(&marker_path, marker_path.with_extension("old")).unwrap();
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(!state.validate_session_with(
            &record,
            directory.path().to_str().unwrap(),
            2,
            &current_executable,
        ));
    }

    #[test]
    fn sidecar_path_is_only_the_application_executable_sibling() {
        let directory = tempdir().unwrap();
        let app = directory.path().join("DevConsole");
        let expected = directory.path().join(if cfg!(windows) {
            "runtime-atlas-supervisor.exe"
        } else {
            "runtime-atlas-supervisor"
        });
        assert_eq!(supervisor_executable_for(&app).unwrap(), expected);
    }

    #[test]
    fn output_sanitization_spans_read_chunks_and_bounds_unterminated_lines() {
        let mut output = String::new();
        stream_sanitized_output(
            std::io::Cursor::new(b"Bearer private-value\ntoken = second-private"),
            7,
            |text| output.push_str(text),
        );
        assert!(!output.contains("private"));
        assert!(output.contains("Bearer <redacted>"));
        assert!(output.contains("token = <redacted>"));

        output.clear();
        stream_sanitized_output(std::io::Cursor::new(vec![b'x'; 32_001]), 4096, |text| {
            output.push_str(text)
        });
        assert_eq!(output, "<output omitted>");
    }

    #[test]
    fn output_from_an_old_generation_cannot_reach_a_restarted_run() {
        let memory = Arc::new(Mutex::new(RuntimeMemory::default()));
        let key = ActionRunKey {
            action_id: Uuid::new_v4(),
            worktree_path: canonical_path(Path::new(if cfg!(windows) {
                r"C:\repo"
            } else {
                "/repo"
            })),
        };
        let old_generation = Uuid::new_v4();
        let new_generation = Uuid::new_v4();
        memory.lock().unwrap().active_runs.insert(
            key.clone(),
            ActiveRun {
                generation: new_generation,
                session_id: None,
                supervisor_identity: ProcessIdentity {
                    pid: 42,
                    start_identity: "fixture".into(),
                },
            },
        );

        capture_output(
            std::io::Cursor::new(b"old buffered tail\n"),
            &memory,
            &key,
            old_generation,
        );
        assert!(!memory.lock().unwrap().action_output.contains_key(&key));

        capture_output(
            std::io::Cursor::new(b"new output\n"),
            &memory,
            &key,
            new_generation,
        );
        assert_eq!(
            memory.lock().unwrap().action_output.get(&key).unwrap(),
            "new output\n"
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn pending_session_is_finalized_only_from_its_exact_marker() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let session_id = Uuid::new_v4();
        let action_id = Uuid::new_v4();
        let worktree = canonical_path(directory.path());
        let supervisor = current_process_identity().unwrap();
        let marker_path = state.marker_path(session_id);
        let (_control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let mut pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action_id,
            &worktree,
            control_identity,
        )
        .unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        state.sessions.upsert(pending.clone()).unwrap();
        assert!(matches!(
            state
                .resolve_pending_session_with(&pending, &std::env::current_exe().unwrap())
                .unwrap(),
            PendingResolution::Unverified
        ));
        assert_eq!(
            state.sessions.load().unwrap().value.sessions,
            vec![pending.clone()]
        );
        let marker = serde_json::json!({
            "schemaVersion": 2,
            "sessionId": session_id,
            "actionID": action_id,
            "worktreePath": worktree,
            "supervisorPID": supervisor.pid,
            "startIdentity": supervisor.start_identity,
        });
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let PendingResolution::Finalized(finalized) = state
            .resolve_pending_session_with(&pending, &std::env::current_exe().unwrap())
            .unwrap()
        else {
            panic!("exact marker did not finalize the pending session");
        };
        assert!(!finalized.is_pending());
        assert_eq!(finalized.supervisor_identity(), Some(&supervisor));
        assert_eq!(
            state.sessions.load().unwrap().value.sessions,
            vec![finalized]
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn schema_two_orphan_marker_restores_its_registered_session() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let repository_id = Uuid::new_v4();
        let mut action = CustomActionDefinition::new(repository_id, "serve", "server");
        action.kind = CustomActionKind::Session;
        let worktree = canonical_path(directory.path());
        let repository = RepositoryStatus {
            id: repository_id,
            path: worktree.clone(),
            name: "fixture".into(),
            availability: AvailabilityState::Available,
            unavailable_reason: None,
            worktrees: vec![WorktreeStatus {
                path: worktree.clone(),
                branch: Some("main".into()),
                detached: false,
                sha: "0123456789abcdef".into(),
                short_sha: "0123456".into(),
                dirty: false,
                availability: AvailabilityState::Available,
                unavailable_reason: None,
            }],
        };
        let session_id = Uuid::new_v4();
        let supervisor = current_process_identity().unwrap();
        let marker_path = state.marker_path(session_id);
        let (_control, _) = create_session_control(&state.control_path(session_id)).unwrap();
        fs::write(
            &marker_path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "sessionId": session_id,
                "actionID": action.id,
                "worktreePath": worktree,
                "supervisorPID": supervisor.pid,
                "startIdentity": supervisor.start_identity,
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        assert_eq!(
            state
                .recover_orphan_markers_with(
                    &[action.clone()],
                    &[repository],
                    &std::env::current_exe().unwrap()
                )
                .unwrap(),
            None
        );
        let restored = state.sessions.load().unwrap().value.sessions;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, session_id);
        assert_eq!(restored[0].action_id, action.id);
        assert_eq!(restored[0].supervisor_identity(), Some(&supervisor));
    }

    #[test]
    fn explicit_stop_preserves_an_unverified_legacy_pending_launch() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let mut action = CustomActionDefinition::new(Uuid::new_v4(), "serve", "server");
        action.kind = CustomActionKind::Session;
        let worktree = canonical_path(directory.path());
        let mut pending =
            ActionSessionRecord::pending(Uuid::new_v4(), action.id, &worktree).unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        state.sessions.upsert(pending).unwrap();

        assert!(
            state
                .stop_action_inner(&action, &worktree, ActionRunPhase::Stopping)
                .is_err()
        );

        assert_eq!(state.sessions.load().unwrap().value.sessions.len(), 1);
    }

    #[test]
    fn exclusive_control_lock_cancels_a_pre_spawn_pending_launch() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let session_id = Uuid::new_v4();
        let action_id = Uuid::new_v4();
        let worktree = canonical_path(directory.path());
        let (control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let mut pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action_id,
            &worktree,
            control_identity,
        )
        .unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        control.unlock().unwrap();
        drop(control);

        assert!(matches!(
            state
                .resolve_pending_session_with(&pending, &std::env::current_exe().unwrap())
                .unwrap(),
            PendingResolution::Stale
        ));
        assert!(!state.control_path(session_id).exists());
    }

    #[test]
    fn explicit_stop_removes_only_an_exclusively_cancelled_pending_launch() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let mut action = CustomActionDefinition::new(Uuid::new_v4(), "serve", "server");
        action.kind = CustomActionKind::Session;
        let session_id = Uuid::new_v4();
        let worktree = canonical_path(directory.path());
        let (control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let mut pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action.id,
            &worktree,
            control_identity,
        )
        .unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        state.sessions.upsert(pending).unwrap();
        control.unlock().unwrap();
        drop(control);

        state
            .stop_action_inner(&action, &worktree, ActionRunPhase::Stopping)
            .unwrap();

        assert!(state.sessions.load().unwrap().value.sessions.is_empty());
        assert!(!state.control_path(session_id).exists());
    }

    #[test]
    fn explicit_stop_preserves_a_pending_launch_while_control_is_shared() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let mut action = CustomActionDefinition::new(Uuid::new_v4(), "serve", "server");
        action.kind = CustomActionKind::Session;
        let session_id = Uuid::new_v4();
        let worktree = canonical_path(directory.path());
        let (_control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let mut pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action.id,
            &worktree,
            control_identity,
        )
        .unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        state.sessions.upsert(pending).unwrap();

        assert!(
            state
                .stop_action_inner(&action, &worktree, ActionRunPhase::Stopping)
                .is_err()
        );

        assert_eq!(state.sessions.load().unwrap().value.sessions.len(), 1);
        assert_eq!(
            read_control_identity(&state.control_path(session_id))
                .unwrap()
                .1,
            0
        );
    }

    #[test]
    fn updater_shutdown_requires_every_pending_launch_to_be_safely_resolved() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let session_id = Uuid::new_v4();
        let (_control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let mut pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            Uuid::new_v4(),
            canonical_path(directory.path()),
            control_identity,
        )
        .unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        state.sessions.upsert(pending).unwrap();

        assert!(state.shutdown_for_update().is_err());
        assert!(state.require_actions_available().is_ok());
        assert_eq!(state.sessions.load().unwrap().value.sessions.len(), 1);
    }

    #[test]
    fn updater_shutdown_removes_an_exclusively_cancelled_pending_launch() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let session_id = Uuid::new_v4();
        let (control, control_identity) =
            create_session_control(&state.control_path(session_id)).unwrap();
        let mut pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            Uuid::new_v4(),
            canonical_path(directory.path()),
            control_identity,
        )
        .unwrap();
        pending.started_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        state.sessions.upsert(pending).unwrap();
        control.unlock().unwrap();
        drop(control);

        state.shutdown_for_update().unwrap();
        assert!(state.sessions.load().unwrap().value.sessions.is_empty());
        assert!(!state.control_path(session_id).exists());
        assert!(state.shutdown_for_update().is_err());
        assert_eq!(
            state
                .plan_action(Uuid::new_v4(), "", BTreeMap::new(), false)
                .unwrap_err(),
            "Runtime Atlas actions are unavailable while updating"
        );
        state.cancel_update_shutdown();
        assert!(state.require_actions_available().is_ok());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn updater_shutdown_stops_an_exact_active_task_supervisor() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(&executable)
            .args([
                "--exact",
                "runtime_atlas::tests::active_task_supervisor_fixture_process",
                "--nocapture",
            ])
            .env("RUNTIME_ATLAS_TEST_ACTIVE_TASK", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap();
        state.lock().unwrap().active_runs.insert(
            ActionRunKey {
                action_id: Uuid::new_v4(),
                worktree_path: canonical_path(directory.path()),
            },
            ActiveRun {
                generation: Uuid::new_v4(),
                session_id: None,
                supervisor_identity: identity.clone(),
            },
        );

        state.shutdown_for_update_with(&executable).unwrap();

        child.wait().unwrap();
        assert_ne!(process_identity(identity.pid).as_ref(), Ok(&identity));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn updater_shutdown_fails_while_an_exact_task_supervisor_remains() {
        let directory = tempdir().unwrap();
        let state =
            RuntimeAtlasState::new(directory.path().join("Runtime Atlas"), AppLanguage::English)
                .unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_atlas::tests::active_task_supervisor_fixture_process",
                "--nocapture",
            ])
            .env("RUNTIME_ATLAS_TEST_ACTIVE_TASK", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap();
        state.lock().unwrap().active_runs.insert(
            ActionRunKey {
                action_id: Uuid::new_v4(),
                worktree_path: canonical_path(directory.path()),
            },
            ActiveRun {
                generation: Uuid::new_v4(),
                session_id: None,
                supervisor_identity: identity.clone(),
            },
        );

        assert!(state.shutdown_for_update_with(directory.path()).is_err());
        assert_eq!(process_identity(identity.pid).as_ref(), Ok(&identity));
        assert!(state.require_actions_available().is_ok());

        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn active_task_supervisor_fixture_process() {
        if std::env::var("RUNTIME_ATLAS_TEST_ACTIVE_TASK").as_deref() == Ok("1") {
            thread::sleep(Duration::from_secs(10));
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn waits_for_an_exact_marker_from_a_sidecar_fixture() {
        run_session_marker_fixture(false);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn retries_a_transient_partial_marker_until_atomic_publication() {
        run_session_marker_fixture(true);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn supervisor_validation_exit_is_reported_before_marker_timeout() {
        let directory = tempdir().unwrap();
        prepare_private_directory(directory.path()).unwrap();
        let session_id = Uuid::new_v4();
        let action_id = Uuid::new_v4();
        let worktree = canonical_path(directory.path());
        let marker = directory.path().join(format!("{session_id}.json"));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_atlas::tests::supervisor_validation_failure_fixture_process",
                "--nocapture",
            ])
            .env("RUNTIME_ATLAS_TEST_EXIT_71", "1")
            .spawn()
            .unwrap();
        let error = wait_for_session_marker(
            &marker,
            session_id,
            action_id,
            &worktree,
            &std::env::current_exe().unwrap(),
            &mut child,
        )
        .unwrap_err();
        assert!(error.contains("could not be resolved safely"));
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn supervisor_validation_reports_its_bounded_error_detail() {
        let directory = tempdir().unwrap();
        prepare_private_directory(directory.path()).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_atlas::tests::supervisor_validation_failure_fixture_process",
                "--nocapture",
            ])
            .env("RUNTIME_ATLAS_TEST_EXIT_71", "1")
            .env("RUNTIME_ATLAS_TEST_EXIT_DETAIL", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let error = wait_for_session_marker(
            &directory.path().join("missing.json"),
            Uuid::new_v4(),
            Uuid::new_v4(),
            &canonical_path(directory.path()),
            &std::env::current_exe().unwrap(),
            &mut child,
        )
        .unwrap_err();
        assert!(error.contains("login shell environment timed out"));
    }

    #[test]
    fn supervisor_validation_failure_fixture_process() {
        if std::env::var("RUNTIME_ATLAS_TEST_EXIT_71").as_deref() == Ok("1") {
            if std::env::var("RUNTIME_ATLAS_TEST_EXIT_DETAIL").as_deref() == Ok("1") {
                eprintln!("runtime-atlas-supervisor: login shell environment timed out");
            }
            std::process::exit(71);
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn run_session_marker_fixture(partial_first: bool) {
        let directory = tempdir().unwrap();
        prepare_private_directory(directory.path()).unwrap();
        let session_id = Uuid::new_v4();
        let action_id = Uuid::new_v4();
        let worktree = canonical_path(directory.path());
        let marker = directory.path().join(format!("{session_id}.json"));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_atlas::tests::session_marker_fixture_process",
                "--nocapture",
            ])
            .env("RUNTIME_ATLAS_TEST_MARKER", &marker)
            .env("RUNTIME_ATLAS_TEST_SESSION", session_id.to_string())
            .env("RUNTIME_ATLAS_TEST_ACTION", action_id.to_string())
            .env("RUNTIME_ATLAS_TEST_WORKTREE", &worktree)
            .env(
                "RUNTIME_ATLAS_TEST_PARTIAL",
                if partial_first { "1" } else { "0" },
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (identity, marker_identity) = wait_for_session_marker(
            &marker,
            session_id,
            action_id,
            &worktree,
            &std::env::current_exe().unwrap(),
            &mut child,
        )
        .unwrap();
        assert_eq!(identity.pid, child.id());
        assert!(!marker_identity.is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn session_marker_fixture_process() {
        let (Ok(path), Ok(session), Ok(action), Ok(worktree)) = (
            std::env::var("RUNTIME_ATLAS_TEST_MARKER"),
            std::env::var("RUNTIME_ATLAS_TEST_SESSION"),
            std::env::var("RUNTIME_ATLAS_TEST_ACTION"),
            std::env::var("RUNTIME_ATLAS_TEST_WORKTREE"),
        ) else {
            return;
        };
        let session_id = Uuid::parse_str(&session).unwrap();
        let action_id = Uuid::parse_str(&action).unwrap();
        let identity = current_process_identity().unwrap();
        let marker_path = PathBuf::from(path);
        if std::env::var("RUNTIME_ATLAS_TEST_PARTIAL").as_deref() == Ok("1") {
            fs::write(&marker_path, b"{").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            thread::sleep(Duration::from_millis(30));
            fs::remove_file(&marker_path).unwrap();
        }
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let temporary = marker_path.with_extension("tmp");
        let mut file = options.open(&temporary).unwrap();
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({
                "schemaVersion": 2,
                "sessionId": session_id,
                "actionID": action_id,
                "worktreePath": worktree,
                "supervisorPID": identity.pid,
                "startIdentity": identity.start_identity,
            }),
        )
        .unwrap();
        use std::io::Write;
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        fs::hard_link(&temporary, &marker_path).unwrap();
        fs::remove_file(temporary).unwrap();
        thread::sleep(Duration::from_secs(10));
    }
}
