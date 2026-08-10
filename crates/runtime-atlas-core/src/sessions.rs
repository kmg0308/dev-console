use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use uuid::Uuid;

use crate::models::{
    AvailabilityState, CustomActionDefinition, CustomActionKind, RepositoryStatus, WorktreeStatus,
};
use crate::observe::observe_process_ancestry;
use crate::relations::{
    ManagedSessionLink, ObservedProcess, PathFlavor, ProcessIdentity, paths_equal,
};
use crate::service::{ActionRun, ActionRunPhase};
use crate::storage::{
    ActionSessionLinkState, ActionSessionRecord, ActionSessionStore, RuntimeAtlasPaths,
    canonical_path,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionSessionStatus {
    pub managed_sessions: Vec<ManagedSessionLink>,
    pub action_runs: Vec<ActionRun>,
}

pub fn reconcile_action_sessions(
    paths: &RuntimeAtlasPaths,
    actions: &[CustomActionDefinition],
    repositories: &[RepositoryStatus],
    observed: &[ObservedProcess],
    expected_supervisor: &Path,
    path_flavor: PathFlavor,
) -> Result<ActionSessionStatus, String> {
    let document = ActionSessionStore::new(paths)
        .load()
        .map_err(|error| error.to_string())?
        .value;
    let session_schema_version = document.schema_version;
    let stored = document.sessions;
    let mut records = stored
        .iter()
        .cloned()
        .filter_map(|record| {
            observe_action_session(
                paths,
                record,
                session_schema_version,
                expected_supervisor,
                path_flavor,
            )
        })
        .collect::<Vec<_>>();
    if let Ok(entries) = fs::read_dir(&paths.action_session_markers_directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(id_text) = file_name.strip_suffix(".json") else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(id_text) else {
                continue;
            };
            if file_name != format!("{id}.json")
                || stored.iter().any(|record| record.id == id)
                || records.iter().any(|record| record.id == id)
            {
                continue;
            }
            let Ok((marker, marker_identity)) = read_session_marker(&path) else {
                continue;
            };
            let Some((action_id, worktree)) = marker.registration() else {
                continue;
            };
            if marker.schema_version != 2 || marker.session_id != id {
                continue;
            }
            let supervisor = ProcessIdentity {
                pid: marker.supervisor_pid,
                start_identity: marker.start_identity.clone(),
            };
            if process_identity(supervisor.pid).ok().as_ref() != Some(&supervisor)
                || !supervisor_executable_matches(&supervisor, expected_supervisor)
            {
                continue;
            }
            let control_path = paths
                .action_session_markers_directory
                .join(format!("{id}.control"));
            let Ok((control_identity, 0)) = read_control_identity(&control_path) else {
                continue;
            };
            let Ok(record) = ActionSessionRecord::pending_with_control_identity(
                id,
                action_id,
                worktree,
                control_identity,
            )
            .and_then(|record| record.finalize(supervisor, marker_identity)) else {
                continue;
            };
            if registered_action_session(&record, actions, repositories, path_flavor).is_none()
                || stored.iter().chain(&records).any(|existing| {
                    existing.action_id == record.action_id
                        && paths_equal(&existing.worktree_path, &record.worktree_path, path_flavor)
                })
            {
                continue;
            }
            records.push(record);
        }
    }
    let mut managed_sessions = Vec::new();
    let mut action_runs = Vec::new();
    for record in &records {
        let Some((action, worktree)) =
            registered_action_session(record, actions, repositories, path_flavor)
        else {
            continue;
        };
        if record.is_pending() {
            let Some(expected_control) = record.control_identity() else {
                continue;
            };
            let control_path = paths
                .action_session_markers_directory
                .join(format!("{}.control", record.id));
            if !read_control_identity(&control_path)
                .is_ok_and(|(actual, state)| actual == expected_control && state == 0)
            {
                continue;
            }
            action_runs.push(ActionRun {
                action_id: record.action_id,
                worktree_path: record.worktree_path.clone(),
                phase: ActionRunPhase::Pending,
                output: "The previous launch could not be verified. Stop it to clear the pending launch.".to_owned(),
                exit_code: None,
                managed: true,
            });
            continue;
        }
        if !validate_action_session(
            paths,
            record,
            &record.worktree_path,
            session_schema_version,
            expected_supervisor,
            path_flavor,
        ) {
            continue;
        }
        action_runs.push(ActionRun {
            action_id: record.action_id,
            worktree_path: record.worktree_path.clone(),
            phase: ActionRunPhase::Running,
            output: String::new(),
            exit_code: None,
            managed: true,
        });
        if action.detects_running_worktree_listener {
            let supervisor = record.supervisor_identity().expect("validated identity");
            managed_sessions.extend(
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
    Ok(ActionSessionStatus {
        managed_sessions,
        action_runs,
    })
}

fn observe_action_session(
    paths: &RuntimeAtlasPaths,
    record: ActionSessionRecord,
    session_schema_version: u32,
    expected_supervisor: &Path,
    path_flavor: PathFlavor,
) -> Option<ActionSessionRecord> {
    if !record.is_pending() || session_schema_version != 2 {
        return Some(record);
    }
    let Ok((marker, marker_identity)) = read_session_marker(
        &paths
            .action_session_markers_directory
            .join(format!("{}.json", record.id)),
    ) else {
        return Some(record);
    };
    if marker.schema_version != 2 || !marker.matches_session(&record, path_flavor) {
        return Some(record);
    }
    let Some(expected_control) = record.control_identity() else {
        return Some(record);
    };
    let control_path = paths
        .action_session_markers_directory
        .join(format!("{}.control", record.id));
    if !read_control_identity(&control_path)
        .is_ok_and(|(actual, state)| actual == expected_control && state == 0)
    {
        return Some(record);
    }
    let supervisor = ProcessIdentity {
        pid: marker.supervisor_pid,
        start_identity: marker.start_identity,
    };
    match process_identity(supervisor.pid) {
        Ok(current)
            if current == supervisor
                && supervisor_executable_matches(&supervisor, expected_supervisor) =>
        {
            record.finalize(supervisor, marker_identity).ok()
        }
        Ok(_) => None,
        Err(_) => Some(record),
    }
}

pub fn registered_action_session<'a>(
    record: &ActionSessionRecord,
    actions: &'a [CustomActionDefinition],
    repositories: &'a [RepositoryStatus],
    path_flavor: PathFlavor,
) -> Option<(&'a CustomActionDefinition, &'a WorktreeStatus)> {
    let action = actions
        .iter()
        .find(|action| action.id == record.action_id && action.kind == CustomActionKind::Session)?;
    let repository = repositories.iter().find(|repository| {
        repository.id == action.repository_id
            && repository.availability == AvailabilityState::Available
    })?;
    let worktree = repository.worktrees.iter().find(|worktree| {
        worktree.availability == AvailabilityState::Available
            && paths_equal(&worktree.path, &record.worktree_path, path_flavor)
    })?;
    Some((action, worktree))
}

pub fn validate_action_session(
    paths: &RuntimeAtlasPaths,
    record: &ActionSessionRecord,
    worktree: &str,
    session_schema_version: u32,
    expected_supervisor: &Path,
    path_flavor: PathFlavor,
) -> bool {
    if session_schema_version != 2 {
        return false;
    }
    let (Some(supervisor), Some(expected_marker)) =
        (record.supervisor_identity(), record.marker_identity())
    else {
        return false;
    };
    let Ok((marker, marker_identity)) = read_session_marker(
        &paths
            .action_session_markers_directory
            .join(format!("{}.json", record.id)),
    ) else {
        return false;
    };
    marker.schema_version == 2
        && marker.matches_session(record, path_flavor)
        && marker.session_id == record.id
        && marker.supervisor_pid == supervisor.pid
        && marker.start_identity == supervisor.start_identity
        && process_identity(supervisor.pid).ok().as_ref() == Some(supervisor)
        && supervisor_executable_matches(supervisor, expected_supervisor)
        && record.link_state(supervisor, &marker_identity, worktree)
            == ActionSessionLinkState::Verified
        && marker_identity == expected_marker
        && record.control_identity().is_some_and(|expected| {
            read_control_identity(
                &paths
                    .action_session_markers_directory
                    .join(format!("{}.control", record.id)),
            )
            .is_ok_and(|(actual, state)| actual == expected && state == 0)
        })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct SessionMarker {
    pub schema_version: u32,
    pub session_id: Uuid,
    #[serde(rename = "actionID")]
    pub action_id: Option<Uuid>,
    pub worktree_path: Option<String>,
    #[serde(rename = "supervisorPID")]
    pub supervisor_pid: u32,
    pub start_identity: String,
}

impl SessionMarker {
    pub fn registration(&self) -> Option<(Uuid, &str)> {
        let action_id = self.action_id.filter(|id| !id.is_nil())?;
        let worktree = self.worktree_path.as_deref()?;
        (!worktree.is_empty()
            && Path::new(worktree).is_absolute()
            && canonical_path(Path::new(worktree)) == worktree)
            .then_some((action_id, worktree))
    }

    pub fn matches_session(&self, record: &ActionSessionRecord, path_flavor: PathFlavor) -> bool {
        self.session_id == record.id
            && self.registration().is_some_and(|(action_id, worktree)| {
                action_id == record.action_id
                    && paths_equal(worktree, &record.worktree_path, path_flavor)
            })
    }
}

pub fn read_session_marker(path: &Path) -> Result<(SessionMarker, String), String> {
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
    validate_private_file(&file, "session marker")?;
    let identity = file_identity(&file)?;
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

pub fn open_session_control(path: &Path) -> Result<File, String> {
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
    validate_private_file(&file, "session control")?;
    Ok(file)
}

pub fn read_control_identity(path: &Path) -> Result<(String, u8), String> {
    let mut file = open_session_control(path)?;
    let identity = file_identity(&file)?;
    let mut state = [0u8; 1];
    file.read_exact(&mut state).map_err(string_error)?;
    if file.metadata().map_err(string_error)?.len() != 1 || !matches!(state[0], 0 | 1) {
        return Err("session control has invalid content".to_owned());
    }
    Ok((identity, state[0]))
}

fn validate_private_file(file: &File, role: &str) -> Result<(), String> {
    let metadata = file.metadata().map_err(string_error)?;
    if !metadata.is_file() {
        return Err(format!("{role} must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(format!("{role} must be private to the current user"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!("{role} must not be a reparse point"));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn file_identity(file: &File) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(string_error)?;
    Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
pub fn file_identity(file: &File) -> Result<String, String> {
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
pub fn file_identity(_file: &File) -> Result<String, String> {
    Err("session markers are unsupported on this platform".to_owned())
}

pub fn supervisor_executable_matches(identity: &ProcessIdentity, expected: &Path) -> bool {
    if process_identity(identity.pid).ok().as_ref() != Some(identity) {
        return false;
    }
    let path_matches = process_executable_path(identity.pid)
        .is_ok_and(|actual| same_file_identity(&actual, expected));
    #[cfg(target_os = "macos")]
    let image_matches = process_image_matches(identity.pid, expected);
    #[cfg(not(target_os = "macos"))]
    let image_matches = true;
    path_matches && image_matches && process_identity(identity.pid).ok().as_ref() == Some(identity)
}

fn same_file_identity(actual: &Path, expected: &Path) -> bool {
    let (Ok(actual), Ok(expected)) = (File::open(actual), File::open(expected)) else {
        return false;
    };
    file_identity(&actual).is_ok_and(|actual| file_identity(&expected).as_ref() == Ok(&actual))
}

#[cfg(target_os = "macos")]
fn process_image_matches(pid: u32, expected: &Path) -> bool {
    use core_foundation::base::TCFType;
    use security_framework::os::macos::code_signing::{Flags, GuestAttributes, SecCode};

    let mut attributes = GuestAttributes::new();
    let Ok(pid) = pid.try_into() else {
        return false;
    };
    attributes.set_pid(pid);
    let Ok(code) = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE) else {
        return false;
    };
    dynamic_code_is_valid(code.as_CFTypeRef())
        && code_hash(code.as_CFTypeRef())
            .zip(static_code_hash(expected))
            .is_some_and(|(actual, expected)| actual == expected)
        && dynamic_code_is_valid(code.as_CFTypeRef())
}

#[cfg(target_os = "macos")]
fn dynamic_code_is_valid(code: core_foundation::base::CFTypeRef) -> bool {
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecCodeCheckValidity(
            code: core_foundation::base::CFTypeRef,
            flags: u32,
            requirement: *const std::ffi::c_void,
        ) -> i32;
    }

    unsafe { SecCodeCheckValidity(code, 0, std::ptr::null()) == 0 }
}

#[cfg(target_os = "macos")]
fn static_code_hash(path: &Path) -> Option<Vec<u8>> {
    use core_foundation::base::TCFType;
    use core_foundation::url::CFURL;
    use security_framework::os::macos::code_signing::{Flags, SecStaticCode};

    let url = CFURL::from_path(path, false)?;
    let code = SecStaticCode::from_path(&url, Flags::NONE).ok()?;
    static_code_is_valid(code.as_CFTypeRef()).then(|| code_hash(code.as_CFTypeRef()))?
}

#[cfg(target_os = "macos")]
fn static_code_is_valid(code: core_foundation::base::CFTypeRef) -> bool {
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecStaticCodeCheckValidity(
            code: core_foundation::base::CFTypeRef,
            flags: u32,
            requirement: *const std::ffi::c_void,
        ) -> i32;
    }

    unsafe { SecStaticCodeCheckValidity(code, 0, std::ptr::null()) == 0 }
}

#[cfg(target_os = "macos")]
fn code_hash(code: core_foundation::base::CFTypeRef) -> Option<Vec<u8>> {
    use std::ffi::c_void;

    use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef};
    use core_foundation::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID};
    use core_foundation::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
    use core_foundation::string::CFStringRef;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecCodeInfoUnique: CFStringRef;
        fn SecCodeCopySigningInformation(
            code: CFTypeRef,
            flags: u32,
            information: *mut CFDictionaryRef,
        ) -> i32;
    }

    let mut information = std::ptr::null();
    if unsafe { SecCodeCopySigningInformation(code, 0, &mut information) } != 0
        || information.is_null()
    {
        return None;
    }
    let value = unsafe { CFDictionaryGetValue(information, kSecCodeInfoUnique.cast::<c_void>()) };
    let hash = if !value.is_null() && unsafe { CFGetTypeID(value.cast()) == CFDataGetTypeID() } {
        let length = unsafe { CFDataGetLength(value.cast()) };
        let bytes = unsafe { CFDataGetBytePtr(value.cast()) };
        (!bytes.is_null() && length > 0)
            .then(|| unsafe { std::slice::from_raw_parts(bytes, length as usize) }.to_vec())
    } else {
        None
    };
    unsafe { CFRelease(information.cast()) };
    hash
}

#[cfg(target_os = "macos")]
pub fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    use std::mem::{MaybeUninit, size_of};

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let read = unsafe {
        libc::proc_pidinfo(
            pid.try_into().map_err(string_error)?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>()
                .try_into()
                .map_err(string_error)?,
        )
    };
    if read as usize != size_of::<libc::proc_bsdinfo>() {
        return Err("process identity is unavailable".to_owned());
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_start_tvsec == 0 && info.pbi_start_tvusec == 0 {
        return Err("process identity is unavailable".to_owned());
    }
    Ok(ProcessIdentity {
        pid,
        start_identity: format!(
            "macos:{}:{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ),
    })
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

#[cfg(windows)]
pub fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    use std::mem::zeroed;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err("process identity is unavailable".to_owned());
    }
    let mut creation: FILETIME = unsafe { zeroed() };
    let mut exit: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    let result =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe { CloseHandle(handle) };
    if result == 0 {
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

#[cfg(windows)]
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

#[cfg(not(any(target_os = "macos", windows)))]
pub fn process_identity(_pid: u32) -> Result<ProcessIdentity, String> {
    Err("process identity is unsupported on this platform".to_owned())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn process_executable_path(_pid: u32) -> Result<PathBuf, String> {
    Err("supervisor executable path is unsupported on this platform".to_owned())
}

fn string_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;
    use crate::models::{CustomActionRisk, CustomActionWorkingDirectory, ListeningPort};
    use crate::storage::ActionSessionRecord;

    #[test]
    fn supervisor_process_fixture() {
        if std::env::var_os("RUNTIME_ATLAS_SUPERVISOR_FIXTURE").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn rejects_live_replaced_supervisor_executable() {
        let directory = tempdir().unwrap();
        let supervisor_path = directory.path().join("runtime-atlas-supervisor");
        let replacement_path = directory.path().join("replacement");
        fs::copy(std::env::current_exe().unwrap(), &supervisor_path).unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&supervisor_path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let mut child = Command::new(&supervisor_path)
            .args(["--exact", "sessions::tests::supervisor_process_fixture"])
            .env("RUNTIME_ATLAS_SUPERVISOR_FIXTURE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !process_executable_path(child.id())
            .is_ok_and(|actual| same_file_identity(&actual, &supervisor_path))
        {
            assert!(child.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let identity = process_identity(child.id()).unwrap();
        let before = supervisor_executable_matches(&identity, &supervisor_path);

        fs::copy("/bin/cat", &replacement_path).unwrap();
        fs::rename(&replacement_path, &supervisor_path).unwrap();
        let path_only_after = process_executable_path(identity.pid)
            .is_ok_and(|actual| same_file_identity(&actual, &supervisor_path));
        let after = supervisor_executable_matches(&identity, &supervisor_path);

        let _ = child.kill();
        let _ = child.wait();
        assert!(before);
        assert!(path_only_after);
        assert!(!after);
    }

    #[test]
    fn unsigned_expected_executable_fails_closed() {
        let directory = tempdir().unwrap();
        let expected = directory.path().join("unsigned");
        fs::write(&expected, b"unsigned executable fixture").unwrap();
        assert!(static_code_hash(&expected).is_none());
    }

    #[test]
    fn reconciles_exact_schema_two_pending_and_orphan_sessions_and_fails_closed() {
        let directory = tempdir().unwrap();
        let paths = RuntimeAtlasPaths::new(directory.path());
        fs::create_dir_all(&paths.action_session_markers_directory).unwrap();
        let worktree_path = canonical_path(directory.path());
        let repository_id = Uuid::new_v4();
        let mut action = CustomActionDefinition::new(repository_id, "Server", "npm run dev");
        action.kind = CustomActionKind::Session;
        action.risk = CustomActionRisk::Normal;
        action.working_directory = CustomActionWorkingDirectory::SelectedWorktree;
        action.detects_running_worktree_listener = true;
        let repository = RepositoryStatus {
            id: repository_id,
            path: worktree_path.clone(),
            name: "fixture".to_owned(),
            availability: AvailabilityState::Available,
            unavailable_reason: None,
            worktrees: vec![WorktreeStatus {
                path: worktree_path.clone(),
                branch: Some("main".to_owned()),
                detached: false,
                sha: "0".repeat(40),
                short_sha: "0000000".to_owned(),
                dirty: false,
                availability: AvailabilityState::Available,
                unavailable_reason: None,
            }],
        };
        let session_id = Uuid::new_v4();
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
                "worktreePath": worktree_path,
                "supervisorPID": supervisor.pid,
                "startIdentity": supervisor.start_identity.clone(),
            }),
        )
        .unwrap();
        marker.sync_all().unwrap();
        let marker_identity = file_identity(&marker).unwrap();
        drop(marker);
        let pending_record = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action.id,
            directory.path(),
            control_identity,
        )
        .unwrap();
        let finalized_record = pending_record
            .clone()
            .finalize(supervisor.clone(), marker_identity)
            .unwrap();
        let session_store = ActionSessionStore::new(&paths);
        fs::write(
            &paths.action_sessions_file,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "sessions": [finalized_record.clone()],
            }))
            .unwrap(),
        )
        .unwrap();
        let down_version_document = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(down_version_document.action_runs.is_empty());
        assert!(down_version_document.managed_sessions.is_empty());
        session_store.upsert(pending_record).unwrap();

        let mut child = Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let child_identity = process_identity(child.id()).unwrap();
        let observed = ObservedProcess {
            identity: child_identity.clone(),
            name: "listener".to_owned(),
            cwd: None,
            ports: vec![ListeningPort {
                address: "127.0.0.1".to_owned(),
                port: 3000,
            }],
        };
        let status = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[observed],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert_eq!(status.action_runs.len(), 1);
        assert_eq!(status.action_runs[0].phase, ActionRunPhase::Running);
        assert!(status.action_runs[0].managed);
        assert_eq!(status.managed_sessions[0].process_identity, child_identity);

        session_store.remove(session_id).unwrap();
        let orphan = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert_eq!(orphan.action_runs.len(), 1);
        assert_eq!(orphan.action_runs[0].phase, ActionRunPhase::Running);

        session_store.upsert(finalized_record).unwrap();
        fs::write(
            &marker_path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "sessionId": session_id,
                "actionID": action.id,
                "worktreePath": worktree_path,
                "supervisorPID": supervisor.pid,
                "startIdentity": supervisor.start_identity,
            }))
            .unwrap(),
        )
        .unwrap();
        let down_version_marker = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(down_version_marker.action_runs.is_empty());
        assert!(down_version_marker.managed_sessions.is_empty());
        fs::write(
            &marker_path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "sessionId": session_id,
                "actionID": action.id,
                "worktreePath": worktree_path,
                "supervisorPID": supervisor.pid,
                "startIdentity": supervisor.start_identity,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o644)).unwrap();
        let public_marker = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(public_marker.action_runs.is_empty());
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();

        fs::write(&marker_path, b"{").unwrap();
        let closed = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(closed.action_runs.is_empty());
        assert!(closed.managed_sessions.is_empty());

        let reused_pid = process_identity(std::process::id()).unwrap();
        fs::write(
            &marker_path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "sessionId": session_id,
                "actionID": action.id,
                "worktreePath": worktree_path,
                "supervisorPID": reused_pid.pid,
                "startIdentity": "reused-process",
            }))
            .unwrap(),
        )
        .unwrap();
        ActionSessionStore::new(&paths)
            .upsert(
                ActionSessionRecord::pending_with_control_identity(
                    session_id,
                    action.id,
                    directory.path(),
                    file_identity(&File::open(&control_path).unwrap()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let stale = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(stale.action_runs.is_empty());

        let orphan_id = Uuid::new_v4();
        let orphan_control_path = paths
            .action_session_markers_directory
            .join(format!("{orphan_id}.control"));
        let mut orphan_control = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&orphan_control_path)
            .unwrap();
        orphan_control.write_all(&[0]).unwrap();
        drop(orphan_control);
        let orphan_marker_path = paths
            .action_session_markers_directory
            .join(format!("{orphan_id}.json"));
        fs::write(
            &orphan_marker_path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "sessionId": orphan_id,
                "actionID": action.id,
                "worktreePath": worktree_path,
                "supervisorPID": reused_pid.pid,
                "startIdentity": reused_pid.start_identity,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&orphan_marker_path, fs::Permissions::from_mode(0o600)).unwrap();
        let occupied = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(occupied.action_runs.is_empty());
        fs::remove_file(orphan_marker_path).unwrap();
        fs::remove_file(orphan_control_path).unwrap();

        fs::write(&marker_path, b"{").unwrap();
        let invalid_pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action.id,
            directory.path(),
            "wrong-control-identity".to_owned(),
        )
        .unwrap();
        ActionSessionStore::new(&paths)
            .upsert(invalid_pending)
            .unwrap();
        let closed_pending = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert!(closed_pending.action_runs.is_empty());
        let valid_pending = ActionSessionRecord::pending_with_control_identity(
            session_id,
            action.id,
            directory.path(),
            file_identity(&File::open(&control_path).unwrap()).unwrap(),
        )
        .unwrap();
        ActionSessionStore::new(&paths)
            .upsert(valid_pending)
            .unwrap();
        let pending = reconcile_action_sessions(
            &paths,
            std::slice::from_ref(&action),
            std::slice::from_ref(&repository),
            &[],
            &std::env::current_exe().unwrap(),
            PathFlavor::MacOs,
        )
        .unwrap();
        assert_eq!(pending.action_runs.len(), 1);
        assert_eq!(pending.action_runs[0].phase, ActionRunPhase::Pending);

        fs::write(
            &paths.action_sessions_file,
            br#"{"schemaVersion":3,"sessions":[]}"#,
        )
        .unwrap();
        assert!(
            reconcile_action_sessions(
                &paths,
                &[],
                &[],
                &[],
                &std::env::current_exe().unwrap(),
                PathFlavor::MacOs,
            )
            .is_err()
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
