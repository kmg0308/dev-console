use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::command::output as command_output;
use crate::models::{
    AtlasNotice, AtlasNoticeKind, AvailabilityState, DiscoveryAvailability, RuntimeContainer,
};
use crate::relations::{ObservedProcess, ProcessIdentity};
use crate::runtime::{DockerInspectState, parse_docker_inspect};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservation {
    pub availability: DiscoveryAvailability,
    pub processes: Vec<ObservedProcess>,
    pub notices: Vec<AtlasNotice>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerObservation {
    pub availability: DiscoveryAvailability,
    pub containers: Vec<RuntimeContainer>,
    pub notices: Vec<AtlasNotice>,
}

pub fn observe_processes() -> ProcessObservation {
    platform::observe_processes()
}

/// Returns the verified parent chain, starting with the immediate parent.
/// A process exit, PID reuse, cycle, or unreadable hop rejects the whole chain.
pub fn observe_process_ancestry(process: &ProcessIdentity) -> Option<Vec<ProcessIdentity>> {
    if !process.is_valid() {
        return None;
    }
    platform::observe_process_ancestry(process)
}

pub fn resolve_docker_executable() -> Option<PathBuf> {
    resolve_executable_in_path(
        std::env::var_os("PATH").as_deref(),
        OsStr::new(if cfg!(windows) {
            "docker.exe"
        } else {
            "docker"
        }),
    )
    .or_else(docker_platform::registered_executable)
}

fn resolve_executable_in_path(path: Option<&OsStr>, name: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path?).find_map(|directory| {
        let executable = fs::canonicalize(directory.join(name)).ok()?;
        is_regular_executable(&executable).then_some(executable)
    })
}

fn is_regular_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
fn docker_from_registered_bundles(bundles: &[PathBuf]) -> Option<PathBuf> {
    let [bundle] = bundles else {
        return None;
    };
    let bundle = fs::canonicalize(bundle).ok()?;
    if !fs::symlink_metadata(&bundle).ok()?.file_type().is_dir() {
        return None;
    }
    let candidate = bundle.join("Contents/Resources/bin/docker");
    if !is_regular_executable(&candidate) {
        return None;
    }
    let executable = fs::canonicalize(candidate).ok()?;
    executable.starts_with(&bundle).then_some(executable)
}

#[cfg(target_os = "macos")]
mod docker_platform {
    use std::ffi::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    const UTF8: u32 = 0x0800_0100;

    pub(super) fn registered_executable() -> Option<PathBuf> {
        docker_from_registered_bundles(&application_urls()?)
    }

    fn application_urls() -> Option<Vec<PathBuf>> {
        // SAFETY: The bundle identifier is a static NUL-terminated UTF-8 string.
        let identifier = unsafe {
            CFStringCreateWithCString(std::ptr::null(), c"com.docker.docker".as_ptr(), UTF8)
        };
        if identifier.is_null() {
            return None;
        }
        let mut error = std::ptr::null();
        // SAFETY: `identifier` is a live CFString and `error` is writable.
        let urls = unsafe { LSCopyApplicationURLsForBundleIdentifier(identifier, &mut error) };
        // SAFETY: The create function returned this owned Core Foundation object.
        unsafe { CFRelease(identifier) };
        if !error.is_null() {
            // SAFETY: LaunchServices returned this owned error object.
            unsafe { CFRelease(error) };
            if !urls.is_null() {
                // SAFETY: LaunchServices returned this owned array object.
                unsafe { CFRelease(urls) };
            }
            return None;
        }
        if urls.is_null() {
            return None;
        }

        let paths = (|| {
            // SAFETY: `urls` remains live until after this closure.
            let count = unsafe { CFArrayGetCount(urls) };
            let mut paths = Vec::with_capacity(usize::try_from(count).ok()?);
            for index in 0..count {
                // SAFETY: `index` is within the array count.
                let url = unsafe { CFArrayGetValueAtIndex(urls, index) };
                if url.is_null() {
                    return None;
                }
                let mut buffer = vec![0u8; libc::PATH_MAX as usize + 1];
                // SAFETY: `url` comes from the live array and `buffer` is writable for its length.
                if unsafe {
                    CFURLGetFileSystemRepresentation(
                        url,
                        1,
                        buffer.as_mut_ptr(),
                        buffer.len() as isize,
                    )
                } == 0
                {
                    return None;
                }
                let length = buffer.iter().position(|byte| *byte == 0)?;
                paths.push(PathBuf::from(OsStr::from_bytes(&buffer[..length])));
            }
            Some(paths)
        })();
        // SAFETY: LaunchServices returned this owned array object.
        unsafe { CFRelease(urls) };
        paths
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyApplicationURLsForBundleIdentifier(
            bundle_identifier: *const c_void,
            error: *mut *const c_void,
        ) -> *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            string: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFURLGetFileSystemRepresentation(
            url: *const c_void,
            resolve_against_base: u8,
            buffer: *mut u8,
            maximum_length: isize,
        ) -> u8;
        fn CFRelease(value: *const c_void);
    }
}

#[cfg(not(target_os = "macos"))]
mod docker_platform {
    use super::*;

    pub(super) fn registered_executable() -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod docker_resolver_tests {
    use super::*;
    use tempfile::tempdir;

    fn make_executable(path: &Path) {
        fs::write(path, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn inherited_path_resolves_only_the_first_exact_regular_executable() {
        let temporary = tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let name = OsStr::new(if cfg!(windows) {
            "docker.exe"
        } else {
            "docker"
        });
        fs::create_dir(first.join(name)).unwrap();
        let expected = second.join(name);
        make_executable(&expected);
        fs::write(second.join("docker-helper"), b"fixture").unwrap();
        let path = std::env::join_paths([first, second]).unwrap();

        assert_eq!(
            resolve_executable_in_path(Some(&path), name),
            Some(fs::canonicalize(expected).unwrap())
        );
        assert_eq!(
            resolve_executable_in_path(Some(&path), OsStr::new("missing-docker")),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn registered_bundle_requires_one_app_with_its_regular_docker_executable() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let bundle = temporary.path().join("Docker.app");
        let executable = bundle.join("Contents/Resources/bin/docker");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        make_executable(&executable);

        assert_eq!(
            docker_from_registered_bundles(std::slice::from_ref(&bundle)),
            Some(fs::canonicalize(executable).unwrap())
        );
        assert_eq!(
            docker_from_registered_bundles(&[bundle.clone(), bundle]),
            None
        );

        let escaped_bundle = temporary.path().join("Escaped.app");
        let external = temporary.path().join("external");
        fs::create_dir_all(escaped_bundle.join("Contents")).unwrap();
        fs::create_dir_all(external.join("bin")).unwrap();
        make_executable(&external.join("bin/docker"));
        symlink(&external, escaped_bundle.join("Contents/Resources")).unwrap();
        assert_eq!(docker_from_registered_bundles(&[escaped_bundle]), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launch_services_docker_is_installed_or_absent() {
        if let Some(executable) = docker_platform::registered_executable() {
            assert!(is_regular_executable(&executable));
            assert!(executable.ends_with("Contents/Resources/bin/docker"));
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessFact {
    identity: ProcessIdentity,
    parent_pid: u32,
    started_at: u128,
}

fn verified_ancestry(
    expected: &ProcessIdentity,
    mut fact: impl FnMut(u32) -> Option<ProcessFact>,
) -> Option<Vec<ProcessIdentity>> {
    let mut current = fact(expected.pid)?;
    if current.identity != *expected {
        return None;
    }

    let mut seen = std::collections::BTreeSet::from([expected.pid]);
    let mut ancestors = Vec::new();
    while current.parent_pid > 1 {
        let parent = fact(current.parent_pid)?;
        if parent.identity.pid != current.parent_pid
            || !parent.identity.is_valid()
            || !seen.insert(parent.identity.pid)
            || parent.started_at > current.started_at
        {
            return None;
        }
        ancestors.push(parent.identity.clone());
        current = parent;
    }
    Some(ancestors)
}

/// Docker is observed only through the executable explicitly selected by the caller.
pub fn observe_docker(executable: Option<&Path>) -> DockerObservation {
    let Some(executable) = executable else {
        return DockerObservation {
            availability: unavailable("Docker executable is not configured."),
            containers: Vec::new(),
            notices: Vec::new(),
        };
    };

    let mut info = Command::new(executable);
    info.args(["info", "--format", "{{.ServerVersion}}"]);
    match command_output(&mut info) {
        Ok(output) if output.status.success() => {}
        Ok(_) => return docker_unavailable("Docker daemon is not responding."),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return docker_unavailable("Docker daemon inspection timed out.");
        }
        Err(_) => return docker_unavailable("Docker executable could not be launched."),
    }

    let mut list = Command::new(executable);
    list.args(["ps", "--quiet", "--no-trunc"]);
    let listing = match command_output(&mut list) {
        Ok(output) if output.status.success() => output,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return docker_unavailable("Docker container listing timed out.");
        }
        Ok(_) | Err(_) => return docker_unavailable("Docker containers could not be read."),
    };
    let Ok(listing) = std::str::from_utf8(&listing.stdout) else {
        return docker_unavailable("Docker container list could not be parsed.");
    };
    let identifiers: Vec<_> = listing
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    if identifiers.is_empty() {
        return DockerObservation {
            availability: DiscoveryAvailability::available(),
            containers: Vec::new(),
            notices: Vec::new(),
        };
    }

    let mut inspect = Command::new(executable);
    inspect.arg("inspect").args(&identifiers);
    let inspection = match command_output(&mut inspect) {
        Ok(output) if output.status.success() => output,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return docker_unavailable("Docker container inspection timed out.");
        }
        Ok(_) | Err(_) => {
            return docker_unavailable("Docker container details could not be read.");
        }
    };
    let Ok(inspection) = std::str::from_utf8(&inspection.stdout) else {
        return docker_unavailable("Docker container details could not be parsed.");
    };
    let parsed = match parse_docker_inspect(inspection) {
        Ok(parsed) => parsed,
        Err(_) => return docker_unavailable("Docker container details could not be parsed."),
    };
    let mut notices = Vec::new();
    if parsed.state == DockerInspectState::Partial || parsed.containers.len() != identifiers.len() {
        notices.push(warning(
            "Some Docker container details could not be verified.",
        ));
    }
    DockerObservation {
        availability: DiscoveryAvailability::available(),
        containers: parsed
            .containers
            .into_iter()
            .map(|container| RuntimeContainer {
                id: container.id,
                name: container.name,
                image: container.image,
                mount_sources: container.mount_sources,
                ports: container.ports,
            })
            .collect(),
        notices,
    }
}

fn docker_unavailable(reason: &str) -> DockerObservation {
    DockerObservation {
        availability: unavailable(reason),
        containers: Vec::new(),
        notices: Vec::new(),
    }
}

fn unavailable(reason: &str) -> DiscoveryAvailability {
    DiscoveryAvailability {
        state: AvailabilityState::Unavailable,
        reason: Some(reason.to_owned()),
    }
}

fn warning(message: impl Into<String>) -> AtlasNotice {
    AtlasNotice {
        kind: AtlasNoticeKind::Warning,
        message: message.into(),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::BTreeMap;
    use std::mem::{MaybeUninit, size_of};
    use std::process::Command;

    use crate::command::output as command_output;
    use crate::models::{DiscoveryAvailability, RuntimeProcess};
    use crate::relations::{ObservedProcess, ProcessIdentity};
    use crate::runtime::{
        LsofParseState, ParsedListeningProcess, parse_lsof_listeners,
        parse_lsof_working_directories,
    };

    use super::{ProcessObservation, unavailable, warning};

    const LSOF: &str = "/usr/sbin/lsof";

    pub(super) fn observe_processes() -> ProcessObservation {
        let mut command = Command::new(LSOF);
        command.args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]);
        let listening = match command_output(&mut command) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                return unavailable_processes("Listening TCP port inspection timed out.");
            }
            Err(_) => return unavailable_processes("Listening TCP ports could not be read."),
        };
        if listening.stdout.is_empty()
            && listening.stderr.is_empty()
            && listening.status.code() == Some(1)
        {
            return ProcessObservation {
                availability: DiscoveryAvailability::available(),
                processes: Vec::new(),
                notices: Vec::new(),
            };
        }

        let parsed = parse_lsof_listeners(&String::from_utf8_lossy(&listening.stdout));
        if parsed.processes.is_empty() && !listening.status.success() {
            return unavailable_processes("Listening TCP ports could not be read.");
        }
        let mut notices = Vec::new();
        if !listening.status.success() || parsed.state == LsofParseState::Partial {
            notices.push(warning("Some listening TCP ports could not be verified."));
        }
        if std::str::from_utf8(&listening.stdout).is_err() {
            notices.push(warning(
                "Some listening process names were not valid UTF-8.",
            ));
        }

        let directories = observe_cwds(&parsed.processes, &mut notices);
        let processes =
            assemble_processes(parsed.processes, directories, start_identity, &mut notices);
        ProcessObservation {
            availability: DiscoveryAvailability::available(),
            processes,
            notices,
        }
    }

    pub(super) fn observe_process_ancestry(
        process: &ProcessIdentity,
    ) -> Option<Vec<ProcessIdentity>> {
        super::verified_ancestry(process, process_fact)
    }

    fn observe_cwds(
        processes: &[ParsedListeningProcess],
        notices: &mut Vec<crate::models::AtlasNotice>,
    ) -> BTreeMap<u32, String> {
        if processes.is_empty() {
            return BTreeMap::new();
        }
        let pids = processes
            .iter()
            .map(|process| process.pid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut command = Command::new(LSOF);
        command.args(["-a", "-p", &pids, "-d", "cwd", "-Fn"]);
        let output = match command_output(&mut command) {
            Ok(output) => output,
            Err(error) => {
                notices.push(warning(if error.kind() == std::io::ErrorKind::TimedOut {
                    "Process working directory inspection timed out."
                } else {
                    "Some process working directories could not be verified."
                }));
                return BTreeMap::new();
            }
        };
        let parsed = parse_lsof_working_directories(&String::from_utf8_lossy(&output.stdout));
        if !output.status.success()
            || parsed.state == LsofParseState::Partial
            || parsed.directories.len() != processes.len()
        {
            notices.push(warning(
                "Some process working directories could not be verified.",
            ));
        }
        parsed.directories
    }

    fn process_fact(pid: u32) -> Option<super::ProcessFact> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: `info` points to a correctly sized writable `proc_bsdinfo` buffer.
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
        (info.pbi_start_tvsec != 0 || info.pbi_start_tvusec != 0).then(|| super::ProcessFact {
            identity: ProcessIdentity {
                pid,
                start_identity: format!(
                    "macos:{}:{:06}",
                    info.pbi_start_tvsec, info.pbi_start_tvusec
                ),
            },
            parent_pid: info.pbi_ppid,
            started_at: u128::from(info.pbi_start_tvsec) * 1_000_000
                + u128::from(info.pbi_start_tvusec),
        })
    }

    fn start_identity(pid: u32) -> Option<String> {
        process_fact(pid).map(|fact| fact.identity.start_identity)
    }

    fn assemble_processes(
        parsed: Vec<ParsedListeningProcess>,
        directories: BTreeMap<u32, String>,
        identity: impl Fn(u32) -> Option<String>,
        notices: &mut Vec<crate::models::AtlasNotice>,
    ) -> Vec<ObservedProcess> {
        let mut missing_identity = false;
        let processes = parsed
            .into_iter()
            .filter_map(|process| {
                let Some(start_identity) = identity(process.pid) else {
                    missing_identity = true;
                    return None;
                };
                let pid = process.pid;
                Some(ObservedProcess::from_runtime(
                    RuntimeProcess {
                        pid,
                        name: process.name,
                        cwd: directories.get(&pid).cloned(),
                        ports: process.ports,
                    },
                    start_identity,
                ))
            })
            .collect();
        if missing_identity {
            notices.push(warning(
                "Some listener process identities could not be verified.",
            ));
        }
        processes
    }

    fn unavailable_processes(reason: &str) -> ProcessObservation {
        ProcessObservation {
            availability: unavailable(reason),
            processes: Vec::new(),
            notices: Vec::new(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::models::{AvailabilityState, ListeningPort};

        #[test]
        fn assembles_verified_fixture_and_omits_missing_identity() {
            let parsed = vec![
                ParsedListeningProcess {
                    pid: 42,
                    name: "node".to_owned(),
                    ports: vec![ListeningPort {
                        address: "127.0.0.1".to_owned(),
                        port: 3000,
                    }],
                },
                ParsedListeningProcess {
                    pid: 84,
                    name: "gone".to_owned(),
                    ports: vec![ListeningPort {
                        address: "*".to_owned(),
                        port: 4000,
                    }],
                },
            ];
            let mut notices = Vec::new();
            let observed = assemble_processes(
                parsed,
                BTreeMap::from([(42, "/repo".to_owned())]),
                |pid| (pid == 42).then(|| "macos:1:000002".to_owned()),
                &mut notices,
            );
            assert_eq!(observed.len(), 1);
            assert_eq!(observed[0].cwd.as_deref(), Some("/repo"));
            assert_eq!(observed[0].identity.start_identity, "macos:1:000002");
            assert_eq!(notices.len(), 1);
        }

        #[test]
        fn ancestry_fixture_rejects_pid_reuse_and_broken_chains() {
            let expected = ProcessIdentity {
                pid: 42,
                start_identity: "macos:1:000042".to_owned(),
            };
            let valid = BTreeMap::from([
                (
                    42,
                    super::super::ProcessFact {
                        identity: expected.clone(),
                        parent_pid: 7,
                        started_at: 42,
                    },
                ),
                (
                    7,
                    super::super::ProcessFact {
                        identity: ProcessIdentity {
                            pid: 7,
                            start_identity: "macos:1:000007".to_owned(),
                        },
                        parent_pid: 1,
                        started_at: 7,
                    },
                ),
            ]);
            assert_eq!(
                super::super::verified_ancestry(&expected, |pid| valid.get(&pid).cloned())
                    .unwrap()
                    .iter()
                    .map(|identity| identity.pid)
                    .collect::<Vec<_>>(),
                vec![7]
            );

            let mut reused = valid.clone();
            reused.get_mut(&42).unwrap().identity.start_identity = "macos:2:000042".to_owned();
            assert!(
                super::super::verified_ancestry(&expected, |pid| reused.get(&pid).cloned())
                    .is_none()
            );
            let mut reused_parent = valid.clone();
            reused_parent.get_mut(&7).unwrap().started_at = 43;
            assert!(
                super::super::verified_ancestry(&expected, |pid| {
                    reused_parent.get(&pid).cloned()
                })
                .is_none()
            );
            assert!(
                super::super::verified_ancestry(&expected, |pid| {
                    (pid == 42).then(|| valid[&42].clone())
                })
                .is_none()
            );
        }

        #[test]
        fn observes_local_processes_without_mutation() {
            let observed = observe_processes();
            assert_eq!(observed.availability.state, AvailabilityState::Available);
            assert!(observed.processes.iter().all(|process| {
                process.identity.is_valid() && !process.name.is_empty() && !process.ports.is_empty()
            }));
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::mem::{size_of, zeroed};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::windows::ffi::OsStringExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    use crate::models::{DiscoveryAvailability, ListeningPort, RuntimeProcess};
    use crate::relations::{ObservedProcess, ProcessIdentity};

    use super::{ProcessObservation, unavailable, warning};

    pub(super) fn observe_processes() -> ProcessObservation {
        let mut ports = BTreeMap::<u32, Vec<ListeningPort>>::new();
        let mut notices = Vec::new();
        let ipv4 = read_ipv4(&mut ports);
        let ipv6 = read_ipv6(&mut ports);
        for (ok, family) in [(ipv4, "IPv4"), (ipv6, "IPv6")] {
            if !ok {
                notices.push(warning(format!(
                    "Some {family} listening TCP ports could not be verified."
                )));
            }
        }
        if !ipv4 && !ipv6 {
            return ProcessObservation {
                availability: unavailable("Listening TCP ports could not be read."),
                processes: Vec::new(),
                notices,
            };
        }

        let process_list = toolhelp_processes().unwrap_or_else(|| {
            notices.push(warning(
                "The global process list could not be fully verified.",
            ));
            BTreeMap::new()
        });
        let mut missing_identity = false;
        let processes = ports
            .into_iter()
            .filter_map(|(pid, mut ports)| {
                ports.sort();
                ports.dedup();
                match process_facts(pid, process_list.get(&pid)) {
                    Some((name, start_identity)) => Some(ObservedProcess::from_runtime(
                        RuntimeProcess {
                            pid,
                            name,
                            cwd: None,
                            ports,
                        },
                        start_identity,
                    )),
                    None => {
                        missing_identity = true;
                        None
                    }
                }
            })
            .collect();
        if missing_identity {
            notices.push(warning(
                "Some listener process identities could not be verified.",
            ));
        }
        ProcessObservation {
            availability: DiscoveryAvailability::available(),
            processes,
            notices,
        }
    }

    pub(super) fn observe_process_ancestry(
        process: &ProcessIdentity,
    ) -> Option<Vec<ProcessIdentity>> {
        let processes = toolhelp_processes()?;
        super::verified_ancestry(process, |pid| {
            let entry = processes.get(&pid)?;
            process_fact(pid, entry.parent_pid)
        })
    }

    fn read_ipv4(processes: &mut BTreeMap<u32, Vec<ListeningPort>>) -> bool {
        let Some(bytes) = tcp_table(AF_INET as u32) else {
            return false;
        };
        let Some(count) = table_count(&bytes, size_of::<MIB_TCPROW_OWNER_PID>()) else {
            return false;
        };
        let rows = unsafe { bytes.as_ptr().cast::<u8>().add(size_of::<u32>()) };
        for index in 0..count {
            // SAFETY: `table_count` verified the complete row range.
            let row = unsafe {
                rows.add(index * size_of::<MIB_TCPROW_OWNER_PID>())
                    .cast::<MIB_TCPROW_OWNER_PID>()
                    .read_unaligned()
            };
            let port = u16::from_be(row.dwLocalPort as u16);
            if row.dwOwningPid > 1 && port > 0 {
                processes
                    .entry(row.dwOwningPid)
                    .or_default()
                    .push(ListeningPort {
                        address: Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).to_string(),
                        port,
                    });
            }
        }
        true
    }

    fn read_ipv6(processes: &mut BTreeMap<u32, Vec<ListeningPort>>) -> bool {
        let Some(bytes) = tcp_table(AF_INET6 as u32) else {
            return false;
        };
        let Some(count) = table_count(&bytes, size_of::<MIB_TCP6ROW_OWNER_PID>()) else {
            return false;
        };
        let rows = unsafe { bytes.as_ptr().cast::<u8>().add(size_of::<u32>()) };
        for index in 0..count {
            // SAFETY: `table_count` verified the complete row range.
            let row = unsafe {
                rows.add(index * size_of::<MIB_TCP6ROW_OWNER_PID>())
                    .cast::<MIB_TCP6ROW_OWNER_PID>()
                    .read_unaligned()
            };
            let port = u16::from_be(row.dwLocalPort as u16);
            if row.dwOwningPid > 1 && port > 0 {
                let mut address = Ipv6Addr::from(row.ucLocalAddr).to_string();
                if row.dwLocalScopeId != 0 {
                    address.push_str(&format!("%{}", row.dwLocalScopeId));
                }
                processes
                    .entry(row.dwOwningPid)
                    .or_default()
                    .push(ListeningPort {
                        address: format!("[{address}]"),
                        port,
                    });
            }
        }
        true
    }

    fn tcp_table(family: u32) -> Option<Vec<usize>> {
        let mut size = 0u32;
        // SAFETY: A null buffer is the documented size-query form.
        let first = unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR || size < size_of::<u32>() as u32
        {
            return None;
        }
        for _ in 0..2 {
            let words = (size as usize).div_ceil(size_of::<usize>());
            let mut buffer = vec![0usize; words];
            // SAFETY: The aligned buffer is writable for at least `size` bytes.
            let result = unsafe {
                GetExtendedTcpTable(
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result == NO_ERROR {
                return Some(buffer);
            }
            if result != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
        }
        None
    }

    fn table_count(bytes: &[usize], row_size: usize) -> Option<usize> {
        if bytes.is_empty() {
            return None;
        }
        // SAFETY: A non-empty `usize` slice contains at least four initialized bytes.
        let count = unsafe { bytes.as_ptr().cast::<u32>().read() } as usize;
        (size_of::<u32>() + count.checked_mul(row_size)? <= std::mem::size_of_val(bytes))
            .then_some(count)
    }

    #[derive(Clone)]
    struct ToolhelpProcess {
        name: String,
        parent_pid: u32,
    }

    fn toolhelp_processes() -> Option<BTreeMap<u32, ToolhelpProcess>> {
        // SAFETY: No borrowed pointers are passed.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = OwnedHandle(snapshot);
        let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = BTreeMap::new();
        // SAFETY: `entry` has the documented size and lives for the whole enumeration.
        let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        while more {
            let length = entry
                .szExeFile
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(entry.szExeFile.len());
            processes.insert(
                entry.th32ProcessID,
                ToolhelpProcess {
                    name: String::from_utf16_lossy(&entry.szExeFile[..length]),
                    parent_pid: entry.th32ParentProcessID,
                },
            );
            // SAFETY: The same valid snapshot and entry buffer are reused.
            more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        Some(processes)
    }

    fn open_process(pid: u32) -> Option<OwnedHandle> {
        // SAFETY: OpenProcess receives a PID observed in the process snapshot or TCP table.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        Some(OwnedHandle(handle))
    }

    fn process_fact(pid: u32, parent_pid: u32) -> Option<super::ProcessFact> {
        let handle = open_process(pid)?;
        let started_at = process_started_at_handle(&handle)?;
        Some(super::ProcessFact {
            identity: ProcessIdentity {
                pid,
                start_identity: format!("windows:{started_at}"),
            },
            parent_pid,
            started_at: u128::from(started_at),
        })
    }

    fn process_started_at_handle(handle: &OwnedHandle) -> Option<u64> {
        let mut creation: FILETIME = unsafe { zeroed() };
        let mut exit: FILETIME = unsafe { zeroed() };
        let mut kernel: FILETIME = unsafe { zeroed() };
        let mut user: FILETIME = unsafe { zeroed() };
        // SAFETY: All FILETIME pointers are valid and writable.
        if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) }
            == 0
        {
            return None;
        }
        let created = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        if created == 0 {
            return None;
        }

        Some(created)
    }

    fn process_facts(pid: u32, fallback: Option<&ToolhelpProcess>) -> Option<(String, String)> {
        let handle = open_process(pid)?;
        let created = process_started_at_handle(&handle)?;

        let mut image = vec![0u16; 32_768];
        let mut length = image.len() as u32;
        // SAFETY: `image` is writable for `length` UTF-16 code units.
        let queried =
            unsafe { QueryFullProcessImageNameW(handle.0, 0, image.as_mut_ptr(), &mut length) }
                != 0;
        let name = queried
            .then(|| OsString::from_wide(&image[..length as usize]))
            .as_deref()
            .and_then(|path| Path::new(path).file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .or_else(|| fallback.map(|process| process.name.clone()))
            .unwrap_or_else(|| "Unknown".to_owned());
        Some((name, format!("windows:{created}")))
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: This wrapper is constructed only from owned, valid handles.
            unsafe { CloseHandle(self.0) };
        }
    }

    #[cfg(test)]
    mod tests {
        use std::net::TcpListener;

        use super::*;

        #[test]
        fn observes_the_current_windows_listener_without_guessing_a_cwd() {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let pid = std::process::id();
            let expected_identity = process_facts(pid, None).unwrap().1;

            let observed = observe_processes();
            let process = observed
                .processes
                .iter()
                .find(|process| {
                    process.identity.pid == pid
                        && process.ports.iter().any(|listener| listener.port == port)
                })
                .expect("the current process listener must be observed");

            assert!(!process.identity.start_identity.is_empty());
            assert_eq!(process.identity.start_identity, expected_identity);
            assert_eq!(process.cwd, None);
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use crate::relations::ProcessIdentity;

    use super::{ProcessObservation, unavailable};

    pub(super) fn observe_processes() -> ProcessObservation {
        ProcessObservation {
            availability: unavailable("Process observation is unsupported on this platform."),
            processes: Vec::new(),
            notices: Vec::new(),
        }
    }

    pub(super) fn observe_process_ancestry(
        _process: &ProcessIdentity,
    ) -> Option<Vec<ProcessIdentity>> {
        None
    }
}
