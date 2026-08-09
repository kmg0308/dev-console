use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use runtime_atlas_core::actions::{
    CustomActionPlan, plan_custom_action, plan_custom_action_restart, sanitize_output,
};
use runtime_atlas_core::models::{
    AppLanguage, AtlasNotice, AtlasNoticeKind, AvailabilityState, CustomActionDefinition,
    CustomActionKind, RepositoryStatus, WorktreeNavigationDirection, WorktreeNavigationSession,
    WorktreeStatus, advance_worktree_navigation, reconcile_recent_worktrees,
    record_recent_worktree,
};
use runtime_atlas_core::observe::{
    DockerObservation, ProcessObservation, observe_docker, observe_process_ancestry,
    observe_processes,
};
use runtime_atlas_core::relations::{
    ManagedSessionLink, ObservedProcess, PathFlavor, ProcessIdentity, TerminationSnapshot,
    UserProcessLink, paths_equal, plan_termination,
};
use runtime_atlas_core::repository::inspect_repositories;
use runtime_atlas_core::service::{
    ActionRun, ActionRunPhase, RepositorySnapshotInput, RuntimeAtlasSnapshot,
    RuntimeAtlasSnapshotInput, build_snapshot,
};
use runtime_atlas_core::storage::{
    ActionSessionLinkState, ActionSessionRecord, ActionSessionStore, ConfigurationStore,
    RuntimeAtlasPaths, RuntimeAtlasProcessLease, canonical_path,
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
    let base = app
        .path()
        .local_data_dir()
        .map_err(string_error)?
        .join("Runtime Atlas");
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
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, RuntimeMemory>, String> {
        self.memory
            .lock()
            .map_err(|_| "Runtime Atlas state lock is poisoned".to_owned())
    }

    fn status(&self) -> Result<RuntimeAtlasSnapshot, String> {
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        self.compose_status(
            observe_processes(),
            observe_docker(Some(Path::new("docker"))),
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
        let mut managed = Vec::new();
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
                        if self
                            .registered_session(stored, actions, repositories)
                            .is_some()
                        {
                            let key = action_key(stored.action_id, &stored.worktree_path);
                            memory.action_runs.insert(
                                key,
                                ActionRun {
                                    action_id: stored.action_id,
                                    worktree_path: stored.worktree_path.clone(),
                                    phase: ActionRunPhase::Pending,
                                    output: "The previous launch could not be verified. Stop it to clear the pending launch.".to_owned(),
                                    exit_code: None,
                                    managed: true,
                                },
                            );
                        }
                        continue;
                    }
                }
            } else {
                stored
            };
            if !self.validate_session(record, &record.worktree_path) {
                if self.exact_supervisor_is_running(record) {
                    remove_memory_session(&mut memory, record);
                    continue;
                }
                self.remove_marker_if_owned(record);
                remove_memory_session(&mut memory, record);
                stale.push(record.id);
                continue;
            }
            let Some((action, worktree)) = self.registered_session(record, actions, repositories)
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
            });
            memory.action_runs.entry(key).or_insert(ActionRun {
                action_id: record.action_id,
                worktree_path: record.worktree_path.clone(),
                phase: ActionRunPhase::Running,
                output: String::new(),
                exit_code: None,
                managed: true,
            });
            if action.detects_running_worktree_listener {
                let supervisor = record.supervisor_identity().expect("validated identity");
                managed.extend(
                    observed
                        .iter()
                        .filter(|process| {
                            observe_process_ancestry(&process.identity)
                                .is_some_and(|ancestry| ancestry.contains(supervisor))
                        })
                        .map(|process| ManagedSessionLink {
                            session_id: record.id,
                            process_identity: process.identity.clone(),
                            worktree_path: worktree.path.clone(),
                        }),
                );
            }
        }
        drop(memory);
        for id in stale {
            self.sessions.remove(id).map_err(string_error)?;
        }
        let memory = self.lock()?;
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
            managed,
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
                        if marker.matches_session(record) && marker.schema_version == 2 =>
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
        let action = actions.iter().find(|action| {
            action.id == record.action_id && action.kind == CustomActionKind::Session
        })?;
        let repository = repositories.iter().find(|repository| {
            repository.id == action.repository_id
                && repository.availability == AvailabilityState::Available
        })?;
        let worktree = repository.worktrees.iter().find(|worktree| {
            worktree.availability == AvailabilityState::Available
                && paths_equal(&worktree.path, &record.worktree_path, path_flavor())
        })?;
        Some((action, worktree))
    }

    fn validate_session(&self, record: &ActionSessionRecord, worktree: &str) -> bool {
        let Ok(expected) = supervisor_executable() else {
            return false;
        };
        self.validate_session_with(record, worktree, &expected)
    }

    fn validate_session_with(
        &self,
        record: &ActionSessionRecord,
        worktree: &str,
        expected_executable: &Path,
    ) -> bool {
        let (Some(supervisor), Some(expected_marker)) =
            (record.supervisor_identity(), record.marker_identity())
        else {
            return false;
        };
        let Ok((marker, marker_identity)) = read_session_marker(&self.marker_path(record.id))
        else {
            return false;
        };
        (marker.schema_version == 1
            || (marker.schema_version == 2 && marker.matches_session(record)))
            && marker.session_id == record.id
            && marker.supervisor_pid == supervisor.pid
            && marker.start_identity == supervisor.start_identity
            && process_identity(supervisor.pid).ok().as_ref() == Some(supervisor)
            && supervisor_executable_matches(supervisor, expected_executable)
            && record.link_state(supervisor, &marker_identity, worktree)
                == ActionSessionLinkState::Verified
            && marker_identity == expected_marker
            && (marker.schema_version == 1
                || record.control_identity().is_some_and(|expected| {
                    read_control_identity(&self.control_path(record.id))
                        .is_ok_and(|(actual, state)| actual == expected && state == 0)
                }))
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
        if let (Some(expected), Ok((marker, actual))) =
            (record.marker_identity(), read_session_marker(&path))
            && actual == expected
            && marker.session_id == record.id
        {
            remove_marker_file(&path);
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
        let mut stored = self.sessions.load().map_err(string_error)?.value.sessions;
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
        let active = ActiveRun {
            generation,
            session_id,
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
        let loaded = self.sessions.load().map_err(string_error)?;
        let Some(mut record) = loaded.value.sessions.into_iter().find(|record| {
            record.action_id == action.id
                && paths_equal(&record.worktree_path, worktree, path_flavor())
        }) else {
            self.lock()?.active_runs.remove(&key);
            return Ok(());
        };
        if record.is_pending() {
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
        if !self.validate_session(&record, worktree) {
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
        let loaded = self.sessions.load().map_err(string_error)?;
        if let Some(record) = loaded.value.sessions.into_iter().find(|record| {
            record.action_id == key.action_id
                && paths_equal(&record.worktree_path, &key.worktree_path, path_flavor())
        }) {
            if self.validate_session(&record, &key.worktree_path) {
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
        let _operation = self
            .action_operation
            .lock()
            .map_err(|_| "Runtime Atlas action lock is poisoned".to_owned())?;
        if self.update_shutdown.swap(true, Ordering::AcqRel) {
            return Err("Runtime Atlas update shutdown is already active".to_owned());
        }
        let result = (|| {
            let loaded = self.sessions.load().map_err(string_error)?;
            if loaded.value.schema_version > 2 {
                return Err("action session data requires a newer Runtime Atlas version".to_owned());
            }
            let mut first_error = None;
            for mut record in loaded.value.sessions {
                let result = (|| {
                    if record.is_pending() {
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
                    if self.validate_session(&record, &record.worktree_path) {
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
            let remaining = self.sessions.load().map_err(string_error)?;
            if remaining.value.schema_version > 2 || !remaining.value.sessions.is_empty() {
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
        let output = Command::new(git_executable())
            .args(["--no-optional-locks", "-C"])
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|_| "Git could not inspect this repository.".to_owned())?;
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
    state
        .store
        .set_worktree_order(
            Uuid::parse_str(&repository_id).map_err(string_error)?,
            &keys,
        )
        .map_err(string_error)
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
pub fn runtime_atlas_record_worktree_selection(
    state: State<'_, RuntimeAtlasState>,
    path: String,
) -> Result<(), String> {
    state.record_selection(&path)
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

fn supervisor_executable_matches(identity: &ProcessIdentity, expected: &Path) -> bool {
    if process_identity(identity.pid).ok().as_ref() != Some(identity) {
        return false;
    }
    process_executable_path(identity.pid).is_ok_and(|actual| same_file_identity(&actual, expected))
        && process_identity(identity.pid).ok().as_ref() == Some(identity)
}

fn same_file_identity(actual: &Path, expected: &Path) -> bool {
    let (Ok(actual), Ok(expected)) = (File::open(actual), File::open(expected)) else {
        return false;
    };
    match (
        marker_file_identity(&actual),
        marker_file_identity(&expected),
    ) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn process_executable_path(pid: u32) -> Result<PathBuf, String> {
    let mut buffer = vec![0i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            pid.try_into().map_err(string_error)?,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return Err("supervisor executable path is unavailable".to_owned());
    }
    let bytes = buffer[..length as usize]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    Ok(PathBuf::from(std::ffi::OsString::from(
        String::from_utf8(bytes).map_err(string_error)?,
    )))
}

#[cfg(target_os = "windows")]
fn process_executable_path(pid: u32) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err("supervisor executable path is unavailable".to_owned());
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let result = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe { CloseHandle(handle) };
    if result == 0 || length == 0 {
        return Err("supervisor executable path is unavailable".to_owned());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length as usize],
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn process_executable_path(_pid: u32) -> Result<PathBuf, String> {
    Err("supervisor executable path is unsupported".to_owned())
}

#[cfg(target_os = "macos")]
fn mac_start_identity(pid: u32) -> Option<String> {
    use std::mem::{MaybeUninit, size_of};

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: `info` is a correctly sized writable `proc_bsdinfo` buffer.
    let read = unsafe {
        libc::proc_pidinfo(
            pid.try_into().ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>().try_into().ok()?,
        )
    };
    if read as usize != size_of::<libc::proc_bsdinfo>() {
        return None;
    }
    // SAFETY: `proc_pidinfo` filled the entire buffer as checked above.
    let info = unsafe { info.assume_init() };
    (info.pbi_start_tvsec != 0 || info.pbi_start_tvusec != 0).then(|| {
        format!(
            "macos:{}:{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        )
    })
}

#[cfg(target_os = "macos")]
fn current_process_identity() -> Result<ProcessIdentity, String> {
    let pid = std::process::id();
    Ok(ProcessIdentity {
        pid,
        start_identity: mac_start_identity(pid)
            .ok_or_else(|| "Runtime Atlas process identity is unavailable".to_owned())?,
    })
}

#[cfg(target_os = "macos")]
fn terminate_verified(expected: &ProcessIdentity) -> Result<(), String> {
    if mac_start_identity(expected.pid).as_deref() != Some(&expected.start_identity) {
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
fn current_process_identity() -> Result<ProcessIdentity, String> {
    windows_identity(std::process::id())
}

#[cfg(target_os = "windows")]
fn windows_identity(pid: u32) -> Result<ProcessIdentity, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: OpenProcess receives an observed numeric PID and no borrowed pointers.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err("process identity is unavailable".to_owned());
    }
    let identity = windows_identity_from_handle(handle, pid);
    // SAFETY: `handle` is owned by this function and closed once.
    unsafe { CloseHandle(handle) };
    identity
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
fn current_process_identity() -> Result<ProcessIdentity, String> {
    Err("process termination is unsupported on this platform".to_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn terminate_verified(_expected: &ProcessIdentity) -> Result<(), String> {
    Err("process termination is unsupported on this platform".to_owned())
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn system_language() -> AppLanguage {
    sys_locale::get_locale()
        .map(|locale| AppLanguage::preferred(&[locale]))
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct SessionMarker {
    schema_version: u32,
    session_id: Uuid,
    #[serde(rename = "actionID")]
    action_id: Option<Uuid>,
    worktree_path: Option<String>,
    #[serde(rename = "supervisorPID")]
    supervisor_pid: u32,
    start_identity: String,
}

impl SessionMarker {
    fn registration(&self) -> Option<(Uuid, &str)> {
        let action_id = self.action_id.filter(|id| !id.is_nil())?;
        let worktree = self.worktree_path.as_deref()?;
        (!worktree.is_empty()
            && Path::new(worktree).is_absolute()
            && canonical_path(Path::new(worktree)) == worktree)
            .then_some((action_id, worktree))
    }

    fn matches_session(&self, record: &ActionSessionRecord) -> bool {
        self.session_id == record.id
            && self.registration().is_some_and(|(action_id, worktree)| {
                action_id == record.action_id
                    && paths_equal(worktree, &record.worktree_path, path_flavor())
            })
    }
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

fn read_session_marker(path: &Path) -> Result<(SessionMarker, String), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(string_error)?;
    let metadata = file.metadata().map_err(string_error)?;
    if !metadata.is_file() {
        return Err("session marker must be a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("session marker must be private to the current user".to_owned());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("session marker must not be a reparse point".to_owned());
        }
    }
    let identity = marker_file_identity(&file)?;
    let mut data = Vec::new();
    file.take(4097)
        .read_to_end(&mut data)
        .map_err(string_error)?;
    if data.len() > 4096 {
        return Err("session marker is too large".to_owned());
    }
    let marker = serde_json::from_slice(&data).map_err(string_error)?;
    Ok((marker, identity))
}

fn create_session_control(path: &Path) -> Result<(File, String), String> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
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

fn open_session_control(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(string_error)?;
    let metadata = file.metadata().map_err(string_error)?;
    if !metadata.is_file() {
        return Err("session control must be a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("session control must be private to the current user".to_owned());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("session control must not be a reparse point".to_owned());
        }
    }
    Ok(file)
}

fn read_control_identity(path: &Path) -> Result<(String, u8), String> {
    let mut file = open_session_control(path)?;
    let identity = marker_file_identity(&file)?;
    let mut state = [0u8; 1];
    file.read_exact(&mut state).map_err(string_error)?;
    if file.metadata().map_err(string_error)?.len() != 1 || !matches!(state[0], 0 | 1) {
        return Err("session control has invalid content".to_owned());
    }
    Ok((identity, state[0]))
}

#[cfg(unix)]
fn marker_file_identity(file: &File) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(string_error)?;
    Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn marker_file_identity(file: &File) -> Result<String, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Ok(format!("windows:{}:{index}", info.dwVolumeSerialNumber))
}

#[cfg(not(any(unix, windows)))]
fn marker_file_identity(_file: &File) -> Result<String, String> {
    Err("session markers are unsupported on this platform".to_owned())
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
    if let (Some(expected), Ok((marker, actual))) =
        (record.marker_identity(), read_session_marker(&path))
        && actual == expected
        && marker.session_id == record.id
    {
        remove_marker_file(&path);
    }
    remove_owned_control(directory, record);
}

fn remove_owned_control(directory: &Path, record: &ActionSessionRecord) {
    if let Some(expected) = record.control_identity() {
        remove_control_with_identity(&directory.join(format!("{}.control", record.id)), expected);
    }
}

fn remove_control_with_identity(path: &Path, expected: &str) {
    if open_session_control(path)
        .and_then(|file| marker_file_identity(&file))
        .is_ok_and(|actual| actual == expected)
    {
        remove_marker_file(path);
    }
}

fn remove_marker_with_identity(path: &Path, session_id: Uuid, expected: &str) {
    if let Ok((marker, actual)) = read_session_marker(path)
        && marker.session_id == session_id
        && actual == expected
    {
        remove_marker_file(path);
    }
}

fn remove_marker_file(path: &Path) {
    if fs::remove_file(path).is_ok() {
        #[cfg(unix)]
        if let Some(parent) = path.parent()
            && let Ok(directory) = File::open(parent)
        {
            let _ = directory.sync_all();
        }
    }
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    Ok(ProcessIdentity {
        pid,
        start_identity: mac_start_identity(pid)
            .ok_or_else(|| "process identity is unavailable".to_owned())?,
    })
}

#[cfg(target_os = "windows")]
fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    windows_identity(pid)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn process_identity(_pid: u32) -> Result<ProcessIdentity, String> {
    Err("process identity is unsupported on this platform".to_owned())
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
        state.commit_navigation().unwrap();
        state.commit_navigation().unwrap();
        assert_eq!(state.lock().unwrap().recent_paths[0], second);
        state.record_selection(first).unwrap();
        assert_eq!(state.lock().unwrap().recent_paths[0], first);
        assert!(state.record_selection(unknown).is_err());
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
        let supervisor = current_process_identity().unwrap();
        let marker_path = state.marker_path(session_id);
        let marker = serde_json::json!({
            "schemaVersion": 1,
            "sessionId": session_id,
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
        let record = ActionSessionRecord::new(
            session_id,
            Uuid::new_v4(),
            directory.path(),
            supervisor,
            marker_identity,
        )
        .unwrap();
        let current_executable = std::env::current_exe().unwrap();
        assert!(state.validate_session_with(
            &record,
            directory.path().to_str().unwrap(),
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
    fn supervisor_validation_failure_fixture_process() {
        if std::env::var("RUNTIME_ATLAS_TEST_EXIT_71").as_deref() == Ok("1") {
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
