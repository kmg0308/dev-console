use std::io::{self, Write};
use std::path::{Path, PathBuf};

use runtime_atlas_core::models::{
    AppLanguage, AtlasNotice, CustomActionDefinition, DiscoveryAvailability, RepositoryStatus,
    RuntimeContainer, RuntimeProcess,
};
use runtime_atlas_core::observe::{observe_docker, observe_processes, resolve_docker_executable};
use runtime_atlas_core::relations::PathFlavor;
use runtime_atlas_core::service::{
    ObservedSnapshotInput, RuntimeAtlasSnapshot, build_observed_snapshot,
};
use runtime_atlas_core::storage::{ConfigurationStore, RuntimeAtlasPaths};
use serde::Serialize;

const USAGE: &str = "Runtime Atlas reads local worktree and runtime state.\n\nUsage:\n  runtime-atlas status --json\n  runtime-atlas actions --json\n";

fn main() {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    std::process::exit(run(
        std::env::args().skip(1),
        None,
        &mut stdout,
        &mut stderr,
    ));
}

fn run(
    arguments: impl IntoIterator<Item = String>,
    data_directory: Option<PathBuf>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    match arguments.as_slice() {
        [command] if matches!(command.as_str(), "help" | "--help" | "-h") => {
            write(stdout, USAGE, 0)
        }
        [command, format] if command == "status" && format == "--json" => {
            status(data_directory, stdout, stderr)
        }
        [command, format] if command == "actions" && format == "--json" => {
            actions(data_directory, stdout, stderr)
        }
        [command, ..] if command == "status" => {
            write(stderr, "Usage: runtime-atlas status --json\n", 64)
        }
        [command, ..] if command == "actions" => {
            write(stderr, "Usage: runtime-atlas actions --json\n", 64)
        }
        [] => write(stderr, USAGE, 64),
        [command, ..] => write(
            stderr,
            &format!("Unknown command: {command}\n\n{USAGE}"),
            64,
        ),
    }
}

fn status(
    data_directory: Option<PathBuf>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let result = (|| {
        let paths = RuntimeAtlasPaths::new(resolve_data_directory(data_directory)?);
        let snapshot = status_snapshot(
            &paths,
            &supervisor_executable()?,
            observe_processes(),
            observe_docker(resolve_docker_executable().as_deref()),
        )?;
        write_json(stdout, &PublicStatus::from(snapshot))
    })();
    match result {
        Ok(()) => 0,
        Err(()) => write(stderr, "Runtime Atlas status could not be read.\n", 1),
    }
}

fn status_snapshot(
    paths: &RuntimeAtlasPaths,
    expected_supervisor: &Path,
    process_observation: runtime_atlas_core::observe::ProcessObservation,
    docker_observation: runtime_atlas_core::observe::DockerObservation,
) -> Result<RuntimeAtlasSnapshot, ()> {
    let loaded = ConfigurationStore::new(paths).load().map_err(|_| ())?;
    build_observed_snapshot(ObservedSnapshotInput {
        paths,
        configuration: loaded.value,
        recovery_notice: loaded.recovery_notice,
        default_language: system_language(),
        git_executable: git_executable(),
        expected_supervisor,
        path_flavor: path_flavor(),
        process_observation,
        docker_observation,
    })
    .map_err(|_| ())
}

fn actions(
    data_directory: Option<PathBuf>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let result = (|| {
        let paths = RuntimeAtlasPaths::new(resolve_data_directory(data_directory)?);
        let actions = ConfigurationStore::new(&paths)
            .load()
            .map_err(|_| ())?
            .value
            .custom_actions;
        write_json(
            stdout,
            &ActionCatalog {
                schema_version: 1,
                actions,
            },
        )
    })();
    match result {
        Ok(()) => 0,
        Err(()) => write(stderr, "Runtime Atlas actions could not be read.\n", 1),
    }
}

fn write_json(output: &mut impl Write, value: &impl Serialize) -> Result<(), ()> {
    let value = serde_json::to_value(value).map_err(|_| ())?;
    serde_json::to_writer_pretty(&mut *output, &value).map_err(|_| ())?;
    output.write_all(b"\n").map_err(|_| ())
}

fn write(output: &mut impl Write, text: &str, exit: i32) -> i32 {
    if output.write_all(text.as_bytes()).is_ok() {
        exit
    } else {
        1
    }
}

fn resolve_data_directory(override_path: Option<PathBuf>) -> Result<PathBuf, ()> {
    if let Some(path) = override_path.or_else(|| nonempty_var("RUNTIME_ATLAS_HOME")) {
        return Ok(path);
    }
    platform_data_directory().ok_or(())
}

fn nonempty_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| {
            !value.is_empty() && value.to_str().is_none_or(|value| !value.trim().is_empty())
        })
        .map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn platform_data_directory() -> Option<PathBuf> {
    nonempty_var("HOME").map(|home| home.join("Library/Application Support/Runtime Atlas"))
}

#[cfg(target_os = "windows")]
fn platform_data_directory() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

    let mut raw = null_mut();
    // SAFETY: Windows allocates a NUL-terminated path for the current user's known folder.
    if unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, null_mut(), &mut raw) } != S_OK
        || raw.is_null()
    {
        return None;
    }
    // SAFETY: successful SHGetKnownFolderPath returns a readable NUL-terminated UTF-16 string.
    let length = unsafe { (0..).find(|&index| *raw.add(index) == 0).unwrap() };
    // SAFETY: `length` was found within the API-owned NUL-terminated buffer.
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    // SAFETY: SHGetKnownFolderPath allocated `raw` with the COM task allocator.
    unsafe { CoTaskMemFree(raw.cast()) };
    Some(path.join("Runtime Atlas"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_data_directory() -> Option<PathBuf> {
    None
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

fn system_language() -> AppLanguage {
    sys_locale::get_locale()
        .map(|locale| AppLanguage::preferred(&[locale]))
        .unwrap_or_default()
}

fn supervisor_executable() -> Result<PathBuf, ()> {
    supervisor_executable_for(&std::env::current_exe().map_err(|_| ())?)
}

fn supervisor_executable_for(executable: &Path) -> Result<PathBuf, ()> {
    let directory = executable.parent().ok_or(())?;
    #[cfg(windows)]
    let name = "runtime-atlas-supervisor.exe";
    #[cfg(not(windows))]
    let name = "runtime-atlas-supervisor";
    #[cfg(target_os = "macos")]
    if directory.file_name().is_some_and(|name| name == "Helpers") {
        return directory
            .parent()
            .map(|contents| contents.join("MacOS").join(name))
            .ok_or(());
    }
    #[cfg(target_os = "macos")]
    if executable == Path::new("/usr/local/bin/runtime-atlas") {
        return Ok(PathBuf::from(
            "/Applications/RuntimeAtlas.app/Contents/MacOS/runtime-atlas-supervisor",
        ));
    }
    Ok(directory.join(name))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionCatalog {
    schema_version: u32,
    actions: Vec<CustomActionDefinition>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicStatus {
    schema_version: u32,
    #[serde(serialize_with = "serialize_iso8601_seconds")]
    generated_at: chrono::DateTime<chrono::Utc>,
    process_discovery: DiscoveryAvailability,
    docker_discovery: DiscoveryAvailability,
    notices: Vec<AtlasNotice>,
    repositories: Vec<PublicRepositoryStatus>,
}

fn serialize_iso8601_seconds<S>(
    value: &chrono::DateTime<chrono::Utc>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicRepositoryStatus {
    id: uuid::Uuid,
    path: String,
    name: String,
    availability: runtime_atlas_core::models::AvailabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    worktrees: Vec<PublicWorktreeStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicWorktreeStatus {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    detached: bool,
    sha: String,
    #[serde(rename = "shortSHA")]
    short_sha: String,
    dirty: bool,
    availability: runtime_atlas_core::models::AvailabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    processes: Vec<RuntimeProcess>,
    containers: Vec<RuntimeContainer>,
}

impl From<RuntimeAtlasSnapshot> for PublicStatus {
    fn from(snapshot: RuntimeAtlasSnapshot) -> Self {
        let repositories = snapshot
            .repositories
            .iter()
            .map(|repository| PublicRepositoryStatus::from_snapshot(repository, &snapshot))
            .collect();
        Self {
            schema_version: 1,
            generated_at: snapshot.generated_at,
            process_discovery: snapshot.process_discovery,
            docker_discovery: snapshot.docker_discovery,
            notices: snapshot.notices,
            repositories,
        }
    }
}

impl PublicRepositoryStatus {
    fn from_snapshot(repository: &RepositoryStatus, snapshot: &RuntimeAtlasSnapshot) -> Self {
        Self {
            id: repository.id,
            path: repository.path.clone(),
            name: repository.name.clone(),
            availability: repository.availability,
            unavailable_reason: repository.unavailable_reason.clone(),
            worktrees: repository
                .worktrees
                .iter()
                .map(|worktree| {
                    let mut processes = snapshot
                        .relations
                        .iter()
                        .filter(|relation| {
                            relation.worktree_path.as_deref() == Some(&worktree.path)
                        })
                        .filter_map(|relation| {
                            snapshot
                                .processes
                                .iter()
                                .find(|process| process.identity == relation.process_identity)
                        })
                        .map(|process| RuntimeProcess {
                            pid: process.identity.pid,
                            name: process.name.clone(),
                            cwd: process.cwd.clone(),
                            ports: process.ports.clone(),
                        })
                        .collect::<Vec<_>>();
                    processes.sort_by(|left, right| {
                        left.name.cmp(&right.name).then(left.pid.cmp(&right.pid))
                    });
                    let mut containers = snapshot
                        .containers
                        .iter()
                        .filter(|container| {
                            container
                                .worktree_links
                                .iter()
                                .any(|link| link.worktree_path == worktree.path)
                        })
                        .map(|container| container.container.clone())
                        .collect::<Vec<_>>();
                    containers.sort_by(|left, right| {
                        left.name.cmp(&right.name).then(left.id.cmp(&right.id))
                    });
                    PublicWorktreeStatus {
                        path: worktree.path.clone(),
                        branch: worktree.branch.clone(),
                        detached: worktree.detached,
                        sha: worktree.sha.clone(),
                        short_sha: worktree.short_sha.clone(),
                        dirty: worktree.dirty,
                        availability: worktree.availability,
                        unavailable_reason: worktree.unavailable_reason.clone(),
                        processes,
                        containers,
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::fs::{self, OpenOptions};
    #[cfg(target_os = "macos")]
    use std::io::Write;
    #[cfg(target_os = "macos")]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(target_os = "macos")]
    use std::process::Command;

    use super::*;
    #[cfg(target_os = "macos")]
    use runtime_atlas_core::models::{
        AvailabilityState, CustomActionKind, CustomActionRisk, CustomActionWorkingDirectory,
        DiscoveryAvailability, ListeningPort,
    };
    #[cfg(target_os = "macos")]
    use runtime_atlas_core::observe::{DockerObservation, ProcessObservation};
    #[cfg(target_os = "macos")]
    use runtime_atlas_core::relations::{ObservedProcess, ProcessRelationKind};
    #[cfg(target_os = "macos")]
    use runtime_atlas_core::sessions::{file_identity, process_identity};
    #[cfg(target_os = "macos")]
    use runtime_atlas_core::storage::{ActionSessionRecord, ActionSessionStore, canonical_path};
    use tempfile::tempdir;

    fn invoke(arguments: &[&str], directory: &Path) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run(
            arguments.iter().map(ToString::to_string),
            Some(directory.to_owned()),
            &mut stdout,
            &mut stderr,
        );
        (
            exit,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn preserves_help_usage_and_json_contracts() {
        let directory = tempdir().unwrap();
        let (exit, output, error) = invoke(&["--help"], directory.path());
        assert_eq!((exit, output.as_str(), error.as_str()), (0, USAGE, ""));

        let (exit, output, error) = invoke(&["status"], directory.path());
        assert_eq!(exit, 64);
        assert!(output.is_empty());
        assert_eq!(error, "Usage: runtime-atlas status --json\n");

        let (exit, output, error) = invoke(&["unknown"], directory.path());
        assert_eq!(exit, 64);
        assert!(output.is_empty());
        assert!(error.starts_with("Unknown command: unknown\n\n"));

        let (exit, output, error) = invoke(&["actions", "--json"], directory.path());
        assert_eq!((exit, error.as_str()), (0, ""));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            serde_json::json!({"actions": [], "schemaVersion": 1})
        );

        let (exit, output, error) = invoke(&["status", "--json"], directory.path());
        assert_eq!((exit, error.as_str()), (0, ""));
        let status: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(status["schemaVersion"], 1);
        let mut keys = status
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "dockerDiscovery",
                "generatedAt",
                "notices",
                "processDiscovery",
                "repositories",
                "schemaVersion",
            ]
        );
    }

    #[test]
    fn public_status_uses_seconds_only_iso8601_wire_time() {
        let generated_at = chrono::DateTime::parse_from_rfc3339("2026-08-10T01:02:03.456789Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let status = PublicStatus {
            schema_version: 1,
            generated_at,
            process_discovery: runtime_atlas_core::models::DiscoveryAvailability::available(),
            docker_discovery: runtime_atlas_core::models::DiscoveryAvailability::available(),
            notices: Vec::new(),
            repositories: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(status).unwrap()["generatedAt"],
            "2026-08-10T01:02:03Z"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn status_projects_exact_pending_and_orphan_sessions_and_fails_closed() {
        let directory = tempdir().unwrap();
        let data = directory.path().join("data");
        let repository = directory.path().join("repository");
        fs::create_dir_all(&repository).unwrap();
        assert!(
            Command::new("/usr/bin/git")
                .args(["init", "--quiet"])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        fs::write(repository.join("README.md"), "fixture\n").unwrap();
        assert!(
            Command::new("/usr/bin/git")
                .args(["add", "README.md"])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("/usr/bin/git")
                .args([
                    "-c",
                    "user.name=Runtime Atlas Test",
                    "-c",
                    "user.email=runtime-atlas@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture",
                ])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );

        let paths = RuntimeAtlasPaths::new(&data);
        let configuration = ConfigurationStore::new(&paths);
        let repository_id = configuration.add_repository(&repository).unwrap();
        let mut action = CustomActionDefinition::new(repository_id, "Server", "npm run dev");
        action.kind = CustomActionKind::Session;
        action.risk = CustomActionRisk::Normal;
        action.working_directory = CustomActionWorkingDirectory::SelectedWorktree;
        action.detects_running_worktree_listener = true;
        configuration.save_custom_action(action.clone()).unwrap();

        fs::create_dir_all(&paths.action_session_markers_directory).unwrap();
        let session_id = repository_id;
        let supervisor = process_identity(std::process::id()).unwrap();
        let control_path = paths
            .action_session_markers_directory
            .join(format!("{session_id}.control"));
        let mut control = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&control_path)
            .unwrap();
        control.write_all(&[0]).unwrap();
        let control_identity = file_identity(&control).unwrap();
        drop(control);
        let marker_path = paths
            .action_session_markers_directory
            .join(format!("{session_id}.json"));
        let mut marker = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&marker_path)
            .unwrap();
        serde_json::to_writer(
            &mut marker,
            &serde_json::json!({
                "schemaVersion": 2,
                "sessionId": session_id,
                "actionID": action.id,
                "worktreePath": canonical_path(&repository),
                "supervisorPID": supervisor.pid,
                "startIdentity": supervisor.start_identity.clone(),
            }),
        )
        .unwrap();
        marker.sync_all().unwrap();
        let marker_identity = file_identity(&marker).unwrap();
        drop(marker);
        let pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action.id,
            &repository,
            control_identity,
        )
        .unwrap();
        let finalized = pending
            .clone()
            .finalize(supervisor, marker_identity)
            .unwrap();
        let sessions = ActionSessionStore::new(&paths);
        sessions.upsert(pending).unwrap();

        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let child_identity = process_identity(child.id()).unwrap();
        let observations = || ProcessObservation {
            availability: DiscoveryAvailability::available(),
            processes: vec![ObservedProcess {
                identity: child_identity.clone(),
                name: "listener".to_owned(),
                cwd: None,
                ports: vec![ListeningPort {
                    address: "127.0.0.1".to_owned(),
                    port: 3000,
                }],
            }],
            notices: Vec::new(),
        };
        let docker = || DockerObservation {
            availability: DiscoveryAvailability {
                state: AvailabilityState::Unavailable,
                reason: Some("fixture".to_owned()),
            },
            containers: Vec::new(),
            notices: Vec::new(),
        };
        let snapshot = status_snapshot(
            &paths,
            &std::env::current_exe().unwrap(),
            observations(),
            docker(),
        )
        .unwrap();
        let public = serde_json::to_value(PublicStatus::from(snapshot.clone())).unwrap();
        let worktree = &public["repositories"][0]["worktrees"][0];
        assert_eq!(worktree["processes"][0]["name"], "listener");
        assert!(worktree.get("relations").is_none());
        assert!(public.get("language").is_none());
        assert!(public.get("actions").is_none());
        assert!(public.get("actionRuns").is_none());
        assert_eq!(snapshot.action_runs.len(), 1);
        assert_eq!(
            snapshot.action_runs[0].phase,
            runtime_atlas_core::service::ActionRunPhase::Running
        );
        assert!(snapshot.action_runs[0].managed);
        assert_eq!(snapshot.processes[0].cwd, None);
        assert_eq!(
            snapshot.relations[0].kind,
            ProcessRelationKind::ManagedSession
        );

        sessions.remove(session_id).unwrap();
        let orphan = status_snapshot(
            &paths,
            &std::env::current_exe().unwrap(),
            observations(),
            docker(),
        )
        .unwrap();
        assert_eq!(orphan.action_runs, snapshot.action_runs);
        assert_eq!(orphan.relations, snapshot.relations);

        sessions.upsert(finalized).unwrap();
        fs::write(&marker_path, b"{").unwrap();
        let malformed = status_snapshot(
            &paths,
            &std::env::current_exe().unwrap(),
            observations(),
            docker(),
        )
        .unwrap();
        assert!(malformed.action_runs.is_empty());
        assert!(
            malformed
                .relations
                .iter()
                .all(|relation| relation.kind != ProcessRelationKind::ManagedSession)
        );

        fs::write(&paths.action_sessions_file, b"{").unwrap();
        let malformed_sessions = status_snapshot(
            &paths,
            &std::env::current_exe().unwrap(),
            observations(),
            docker(),
        )
        .unwrap();
        assert!(malformed_sessions.action_runs.is_empty());
        assert!(
            malformed_sessions
                .relations
                .iter()
                .all(|relation| relation.kind != ProcessRelationKind::ManagedSession)
        );

        fs::write(
            &paths.action_sessions_file,
            br#"{"schemaVersion":3,"sessions":[]}"#,
        )
        .unwrap();
        assert!(
            status_snapshot(
                &paths,
                &std::env::current_exe().unwrap(),
                observations(),
                docker(),
            )
            .is_err()
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn public_app_helper_resolves_the_bundled_supervisor() {
        assert_eq!(
            supervisor_executable_for(Path::new(
                "/Applications/RuntimeAtlas.app/Contents/Helpers/runtime-atlas"
            )),
            Ok(PathBuf::from(
                "/Applications/RuntimeAtlas.app/Contents/MacOS/runtime-atlas-supervisor"
            ))
        );
        assert_eq!(
            supervisor_executable_for(Path::new("/usr/local/bin/runtime-atlas")),
            Ok(PathBuf::from(
                "/Applications/RuntimeAtlas.app/Contents/MacOS/runtime-atlas-supervisor"
            ))
        );
    }
}
