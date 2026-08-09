use crate::account::{CodexAccountUsage, CodexAccountUsageError, parse_rate_limits_response};
use chrono::Utc;
use serde::Deserialize;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::{env, fs};
use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

const INITIALIZE: &[u8] = br#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"tokenmeter","title":"TokenMeter","version":"0.1.0"}}}
"#;
const READ_RATE_LIMITS: &[u8] =
    b"{\"method\":\"initialized\"}\n{\"method\":\"account/rateLimits/read\",\"id\":2}\n";
const MAX_LINE_BYTES: u64 = 1024 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROCESS_EXIT_GRACE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodexAccountUsageServiceError {
    #[error("The configured Codex executable was not found.")]
    ExecutableNotFound,
    #[error("Codex could not be started.")]
    LaunchFailed,
    #[error("Codex account status timed out.")]
    TimedOut,
    #[error("Codex closed before returning account status.")]
    ConnectionClosed,
    #[error("Codex app server exited unsuccessfully (code: {0:?}).")]
    ProcessFailed(Option<i32>),
    #[error(transparent)]
    Response(#[from] CodexAccountUsageError),
}

pub fn fetch_codex_account_usage(
    configured_executable: Option<&OsStr>,
    timeout: Duration,
) -> Result<CodexAccountUsage, CodexAccountUsageServiceError> {
    let executable = account_executable(configured_executable)?;
    let mut command = Command::new(executable);
    command
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let tree = ProcessTree::prepare(&mut command)
        .map_err(|_| CodexAccountUsageServiceError::LaunchFailed)?;
    let mut child = command.spawn().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => CodexAccountUsageServiceError::ExecutableNotFound,
        _ => CodexAccountUsageServiceError::LaunchFailed,
    })?;
    if tree.attach(&child).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CodexAccountUsageServiceError::LaunchFailed);
    }
    let mut stdin = child
        .stdin
        .take()
        .ok_or(CodexAccountUsageServiceError::LaunchFailed)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(CodexAccountUsageServiceError::LaunchFailed)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(CodexAccountUsageServiceError::LaunchFailed)?;

    let readers = match OutputReaders::start(stdout, stderr) {
        Ok(readers) => readers,
        Err(_) => {
            tree.terminate(&mut child);
            return Err(CodexAccountUsageServiceError::LaunchFailed);
        }
    };

    let result = exchange(&mut child, &mut stdin, &readers.lines, timeout);
    drop(stdin);
    readers.cancel.store(true, Ordering::Release);
    tree.terminate(&mut child);
    readers.finish();
    result
}

fn account_executable(
    configured: Option<&OsStr>,
) -> Result<PathBuf, CodexAccountUsageServiceError> {
    if let Some(configured) = configured {
        let path = Path::new(configured);
        return path
            .is_absolute()
            .then(|| path.to_owned())
            .ok_or(CodexAccountUsageServiceError::ExecutableNotFound);
    }

    #[cfg(target_os = "macos")]
    return resolve_macos_account_executable();

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return Ok(PathBuf::from("codex"));

    #[cfg(target_os = "windows")]
    resolve_windows_account_executable()
}

#[cfg(target_os = "macos")]
fn resolve_macos_account_executable() -> Result<PathBuf, CodexAccountUsageServiceError> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let path = env::var_os("PATH");
    let system_candidates = [
        PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
        PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];
    resolve_macos_account_executable_from(home.as_deref(), path.as_deref(), &system_candidates)
}

#[cfg(target_os = "macos")]
fn resolve_macos_account_executable_from(
    home: Option<&Path>,
    path: Option<&OsStr>,
    system_candidates: &[PathBuf],
) -> Result<PathBuf, CodexAccountUsageServiceError> {
    use std::os::unix::fs::PermissionsExt;

    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/codex"));
        candidates.push(home.join("Applications/ChatGPT.app/Contents/Resources/codex"));
    }
    candidates.extend_from_slice(system_candidates);
    if let Some(path) = path {
        candidates.extend(env::split_paths(path).map(|directory| directory.join("codex")));
    }

    let mut visited = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| visited.insert(candidate.clone()))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or(CodexAccountUsageServiceError::ExecutableNotFound)
}

#[cfg(target_os = "windows")]
fn resolve_windows_account_executable() -> Result<PathBuf, CodexAccountUsageServiceError> {
    let path = env::var_os("PATH").ok_or(CodexAccountUsageServiceError::ExecutableNotFound)?;
    let extensions =
        env::var_os("PATHEXT").ok_or(CodexAccountUsageServiceError::ExecutableNotFound)?;
    let extensions = extensions.to_string_lossy();

    for directory in env::split_paths(&path).filter(|directory| directory.is_absolute()) {
        for extension in extensions.split(';').filter_map(account_extension) {
            let candidate = directory.join(format!("codex.{extension}"));
            if regular_non_reparse(&candidate)
                && let Ok(candidate) = fs::canonicalize(candidate)
                && regular_non_reparse(&candidate)
            {
                return Ok(candidate);
            }
        }
    }
    Err(CodexAccountUsageServiceError::ExecutableNotFound)
}

#[cfg(target_os = "windows")]
fn account_extension(extension: &str) -> Option<&'static str> {
    match extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "exe" => Some("exe"),
        "cmd" => Some("cmd"),
        "bat" => Some("bat"),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn regular_non_reparse(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    })
}

fn exchange(
    child: &mut Child,
    stdin: &mut impl Write,
    lines: &mpsc::Receiver<Result<Vec<u8>, io::Error>>,
    timeout: Duration,
) -> Result<CodexAccountUsage, CodexAccountUsageServiceError> {
    stdin
        .write_all(INITIALIZE)
        .and_then(|_| stdin.flush())
        .map_err(|_| closed(child))?;
    let started = Instant::now();
    let mut initialized = false;
    let mut successful_exit_deadline: Option<Instant> = None;

    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .ok_or(CodexAccountUsageServiceError::TimedOut)?;
        let mut wait = remaining.min(PROCESS_POLL_INTERVAL);
        if let Some(deadline) = successful_exit_deadline {
            let drain_remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(CodexAccountUsageServiceError::ConnectionClosed)?;
            wait = wait.min(drain_remaining);
        }
        let line = match lines.recv_timeout(wait) {
            Ok(Ok(line)) => line,
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(closed(child));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if successful_exit_deadline.is_some() => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => match child.try_wait() {
                Ok(Some(status)) if !status.success() => return Err(failed(status)),
                Ok(Some(_)) => {
                    successful_exit_deadline = Some(Instant::now() + PROCESS_EXIT_GRACE);
                    continue;
                }
                Ok(None) => continue,
                Err(_) => return Err(CodexAccountUsageServiceError::ConnectionClosed),
            },
        };
        let Ok(envelope) = serde_json::from_slice::<RpcEnvelope>(&line) else {
            continue;
        };

        match envelope.id {
            Some(1) if !initialized => {
                if let Some(error) = envelope.error {
                    return Err(CodexAccountUsageError::Server(error.message).into());
                }
                stdin
                    .write_all(READ_RATE_LIMITS)
                    .and_then(|_| stdin.flush())
                    .map_err(|_| closed(child))?;
                initialized = true;
            }
            Some(2) if initialized => {
                return parse_rate_limits_response(&line, Utc::now()).map_err(Into::into);
            }
            _ => {}
        }
    }
}

struct OutputReaders {
    cancel: Arc<AtomicBool>,
    lines: mpsc::Receiver<Result<Vec<u8>, io::Error>>,
    stdout: thread::JoinHandle<io::Result<()>>,
    stderr: thread::JoinHandle<io::Result<()>>,
}

impl OutputReaders {
    fn start(
        stdout: std::process::ChildStdout,
        stderr: std::process::ChildStderr,
    ) -> io::Result<Self> {
        #[cfg(unix)]
        {
            set_nonblocking(&stdout)?;
            set_nonblocking(&stderr)?;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, lines) = mpsc::channel();
        let stdout_cancel = Arc::clone(&cancel);
        let stdout = thread::spawn(move || read_lines(stdout, sender, stdout_cancel));
        let stderr_cancel = Arc::clone(&cancel);
        let stderr = thread::spawn(move || drain_stderr(stderr, stderr_cancel));
        Ok(Self {
            cancel,
            lines,
            stdout,
            stderr,
        })
    }

    fn finish(self) {
        let _ = self.stdout.join();
        let _ = self.stderr.join();
    }
}

#[cfg(unix)]
fn set_nonblocking(file: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let descriptor = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn read_lines(
    mut stdout: impl Read,
    sender: mpsc::Sender<Result<Vec<u8>, io::Error>>,
    cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        match stdout.read(&mut buffer) {
            Ok(0) => return send_pending_line(&sender, &mut pending),
            Ok(read) => {
                pending.extend_from_slice(&buffer[..read]);
                while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                    let mut line = pending.drain(..=end).collect::<Vec<_>>();
                    send_line(&sender, &mut line)?;
                }
                if pending.len() as u64 > MAX_LINE_BYTES {
                    return send_line_too_large(&sender);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(unix))]
fn read_lines(
    stdout: impl Read,
    sender: mpsc::Sender<Result<Vec<u8>, io::Error>>,
    _cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    use std::io::{BufRead, BufReader};

    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(());
        }
        send_line(&sender, &mut line)?;
    }
}

#[cfg(unix)]
fn send_pending_line(
    sender: &mpsc::Sender<Result<Vec<u8>, io::Error>>,
    pending: &mut Vec<u8>,
) -> io::Result<()> {
    if pending.is_empty() {
        Ok(())
    } else {
        send_line(sender, pending)
    }
}

fn send_line(
    sender: &mpsc::Sender<Result<Vec<u8>, io::Error>>,
    line: &mut Vec<u8>,
) -> io::Result<()> {
    if line.len() as u64 > MAX_LINE_BYTES {
        return send_line_too_large(sender);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    if !line.is_empty() {
        let _ = sender.send(Ok(std::mem::take(line)));
    }
    Ok(())
}

fn send_line_too_large(sender: &mpsc::Sender<Result<Vec<u8>, io::Error>>) -> io::Result<()> {
    let _ = sender.send(Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Codex JSON-RPC frame is too large",
    )));
    Ok(())
}

#[cfg(unix)]
fn drain_stderr(mut stderr: impl Read, cancel: Arc<AtomicBool>) -> io::Result<()> {
    let mut buffer = [0; 8192];
    while !cancel.load(Ordering::Acquire) {
        match stderr.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn drain_stderr(mut stderr: impl Read, _cancel: Arc<AtomicBool>) -> io::Result<()> {
    io::copy(&mut stderr, &mut io::sink()).map(|_| ())
}

fn closed(child: &mut Child) -> CodexAccountUsageServiceError {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => return failed(status),
            Ok(Some(_)) | Err(_) => return CodexAccountUsageServiceError::ConnectionClosed,
            Ok(None) if started.elapsed() >= PROCESS_EXIT_GRACE => {
                return CodexAccountUsageServiceError::ConnectionClosed;
            }
            Ok(None) => thread::sleep(
                PROCESS_POLL_INTERVAL.min(PROCESS_EXIT_GRACE.saturating_sub(started.elapsed())),
            ),
        }
    }
}

fn failed(status: ExitStatus) -> CodexAccountUsageServiceError {
    CodexAccountUsageServiceError::ProcessFailed(status.code())
}

#[cfg(unix)]
struct ProcessTree;

#[cfg(unix)]
impl ProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        Ok(Self)
    }

    fn attach(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    fn terminate(&self, child: &mut Child) {
        let group = i32::try_from(child.id())
            .ok()
            .and_then(|pid| pid.checked_neg());
        if group.is_none_or(|group| unsafe { libc::kill(group, libc::SIGKILL) } != 0) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

#[cfg(windows)]
struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::ffi::c_void;
        use std::mem::size_of;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(error);
        }
        command.creation_flags(CREATE_SUSPENDED);
        Ok(Self(job))
    }

    fn attach(&self, child: &Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        resume_primary_thread(child.id())
    }

    fn terminate(&self, child: &mut Child) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe { TerminateJobObject(self.0, 1) };
        let _ = child.wait();
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut found = false;
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                let resumed = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if resumed != u32::MAX {
                    found = true;
                    break;
                }
            }
        }
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
        .then_some(())
        .ok_or_else(|| io::Error::other("suspended Codex thread could not be resumed"))
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn prepare(_command: &mut Command) -> io::Result<Self> {
        Ok(Self)
    }

    fn attach(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    fn terminate(&self, child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[derive(Deserialize)]
struct RpcEnvelope {
    id: Option<i64>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};
    use tempfile::tempdir;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_resolver_keeps_configured_and_swift_candidate_order() {
        use std::os::unix::fs::PermissionsExt;

        fn executable(path: &Path) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture").unwrap();
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }

        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let local = home.join(".local/bin/codex");
        let app = home.join("Applications/ChatGPT.app/Contents/Resources/codex");
        let system = directory.path().join("system/codex");
        let path = directory.path().join("path/codex");
        for candidate in [&local, &app, &system, &path] {
            executable(candidate);
        }
        let search_path = env::join_paths([path.parent().unwrap()]).unwrap();

        assert_eq!(
            account_executable(Some(system.as_os_str())).unwrap(),
            system
        );
        assert_eq!(
            resolve_macos_account_executable_from(
                Some(&home),
                Some(&search_path),
                std::slice::from_ref(&system),
            )
            .unwrap(),
            local
        );
        fs::remove_file(&local).unwrap();
        assert_eq!(
            resolve_macos_account_executable_from(
                Some(&home),
                Some(&search_path),
                std::slice::from_ref(&system),
            )
            .unwrap(),
            app
        );
        fs::remove_file(&app).unwrap();
        assert_eq!(
            resolve_macos_account_executable_from(
                Some(&home),
                Some(&search_path),
                std::slice::from_ref(&system),
            )
            .unwrap(),
            system
        );
        fs::remove_file(&system).unwrap();
        assert_eq!(
            resolve_macos_account_executable_from(Some(&home), Some(&search_path), &[]).unwrap(),
            path
        );
    }

    #[test]
    fn app_server_success_error_timeout_and_exit_contracts() {
        let fixture = tempdir().unwrap();
        let executable = build_fixture(fixture.path());

        let success = copy_as(&executable, fixture.path(), "success");
        for _ in 0..20 {
            let usage =
                fetch_codex_account_usage(Some(success.as_os_str()), Duration::from_secs(2))
                    .unwrap();
            assert_eq!(usage.five_hour_window.unwrap().used_percent, 31);
        }

        let rpc_error = copy_as(&executable, fixture.path(), "rpc_error");
        assert_eq!(
            fetch_codex_account_usage(Some(rpc_error.as_os_str()), Duration::from_secs(2)),
            Err(CodexAccountUsageServiceError::Response(
                CodexAccountUsageError::Server("Login required".into())
            ))
        );

        let timeout = copy_as(&executable, fixture.path(), "timeout");
        assert_eq!(
            fetch_codex_account_usage(Some(timeout.as_os_str()), Duration::from_millis(50)),
            Err(CodexAccountUsageServiceError::TimedOut)
        );

        let interrupted = copy_as(&executable, fixture.path(), "interrupted");
        let started = Instant::now();
        assert_eq!(
            fetch_codex_account_usage(Some(interrupted.as_os_str()), Duration::from_secs(2)),
            Err(CodexAccountUsageServiceError::ConnectionClosed)
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        #[cfg(target_os = "macos")]
        stop_escaped_fixture(&interrupted.with_extension("pid"));

        let failed = copy_as(&executable, fixture.path(), "failed");
        for _ in 0..10 {
            assert_eq!(
                fetch_codex_account_usage(Some(failed.as_os_str()), Duration::from_secs(2)),
                Err(CodexAccountUsageServiceError::ProcessFailed(Some(7)))
            );
        }
    }

    fn copy_as(source: &Path, directory: &Path, name: &str) -> std::path::PathBuf {
        let target = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        fs::copy(source, &target).unwrap();
        target
    }

    fn build_fixture(directory: &Path) -> std::path::PathBuf {
        let source = directory.join("fake_codex.rs");
        let executable = directory.join(format!("fake_codex{}", std::env::consts::EXE_SUFFIX));
        fs::write(&source, FIXTURE).unwrap();
        let status = Command::new("rustc")
            .args(["--edition=2024", "-o"])
            .arg(&executable)
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());
        executable
    }

    #[cfg(target_os = "macos")]
    fn stop_escaped_fixture(pid_path: &Path) {
        let pid = std::fs::read_to_string(pid_path)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let identity = mac_process_identity(pid).expect("escaped fixture is missing");
        assert_eq!(
            mac_process_identity(pid).as_deref(),
            Some(identity.as_str())
        );
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        while mac_process_identity(pid).as_deref() == Some(identity.as_str())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(
            mac_process_identity(pid).as_deref(),
            Some(identity.as_str())
        );
    }

    #[cfg(target_os = "macos")]
    fn mac_process_identity(pid: u32) -> Option<String> {
        use std::mem::{MaybeUninit, size_of};

        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let read = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?,
            )
        };
        if read as usize != size_of::<libc::proc_bsdinfo>() {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(format!(
            "macos:{}:{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }

    const FIXTURE: &str = r#"
use std::{env, fs, io::{BufRead, BufReader, Write}, process::Command, thread, time::Duration};
#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;

fn main() {
    if env::var_os("TOKENMETER_KEEP_STDOUT_OPEN").is_some() {
        thread::sleep(Duration::from_secs(10));
        return;
    }
    assert_eq!(env::args().skip(1).collect::<Vec<_>>(), ["app-server", "--stdio"]);
    let mode = env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().to_string();
    let mut input = BufReader::new(std::io::stdin());
    let mut line = String::new();
    input.read_line(&mut line).unwrap();
    assert!(line.contains("\"method\":\"initialize\""));
    println!("{{\"id\":1,\"result\":{{}}}}");
    std::io::stdout().flush().unwrap();
    line.clear();
    input.read_line(&mut line).unwrap();
    assert!(line.contains("\"method\":\"initialized\""));
    line.clear();
    input.read_line(&mut line).unwrap();
    assert!(line.contains("account/rateLimits/read"));

    match mode.as_str() {
        "success" => {
            print!("{{\"id\":2,\"result\":{{\"rateLimits\":{{\"primary\":");
            std::io::stdout().flush().unwrap();
            thread::sleep(Duration::from_millis(10));
            println!("{{\"usedPercent\":31,\"windowDurationMins\":300,\"resetsAt\":1783666131}},\"secondary\":null}},\"rateLimitResetCredits\":null}}}}");
        }
        "rpc_error" => println!("{{\"id\":2,\"error\":{{\"code\":-32000,\"message\":\"Login required\"}}}}"),
        "timeout" => thread::sleep(Duration::from_secs(10)),
        "interrupted" => {
            let executable = env::current_exe().unwrap();
            let mut command = Command::new(&executable);
            command.env("TOKENMETER_KEEP_STDOUT_OPEN", "1");
            #[cfg(target_os = "macos")]
            command.process_group(0);
            let child = command.spawn().unwrap();
            fs::write(executable.with_extension("pid"), child.id().to_string()).unwrap();
        }
        "failed" => { eprintln!("secret stderr must not escape"); std::process::exit(7); }
        _ => unreachable!(),
    }
}
"#;
}
