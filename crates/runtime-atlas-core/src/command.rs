use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(target_os = "macos"))]
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub fn output(command: &mut Command) -> io::Result<Output> {
    output_with_timeout(command, DEFAULT_COMMAND_TIMEOUT)
}

pub fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let tree = ProcessTree::prepare(command)?;
    let mut child = command.spawn()?;
    if let Err(error) = tree.attach(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let mut stdout = match drain(child.stdout.take().expect("piped stdout is missing")) {
        Ok(stdout) => stdout,
        Err(error) => {
            tree.terminate(&mut child);
            return Err(error);
        }
    };
    let mut stderr = match drain(child.stderr.take().expect("piped stderr is missing")) {
        Ok(stderr) => stderr,
        Err(error) => {
            tree.terminate(&mut child);
            return Err(error);
        }
    };
    let deadline = Instant::now() + timeout;
    let mut status = None;

    loop {
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    tree.terminate(&mut child);
                    return Err(error);
                }
            };
        }
        let drained = poll_drain(&mut stdout).and_then(|stdout_done| {
            poll_drain(&mut stderr).map(|stderr_done| stdout_done && stderr_done)
        });
        let drained = match drained {
            Ok(drained) => drained,
            Err(error) => {
                tree.terminate(&mut child);
                return Err(error);
            }
        };
        if Instant::now() >= deadline {
            tree.terminate(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "external command timed out",
            ));
        }
        if status.is_some() && drained {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    Ok(Output {
        status: status.expect("completed child has no status"),
        stdout: finish_drain(stdout)?,
        stderr: finish_drain(stderr)?,
    })
}

#[cfg(not(target_os = "macos"))]
struct PipeDrain {
    receiver: Receiver<io::Result<Vec<u8>>>,
    output: Option<Vec<u8>>,
}

#[cfg(not(target_os = "macos"))]
fn drain(mut pipe: impl Read + Send + 'static) -> io::Result<PipeDrain> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    Ok(PipeDrain {
        receiver,
        output: None,
    })
}

#[cfg(not(target_os = "macos"))]
fn poll_drain(drain: &mut PipeDrain) -> io::Result<bool> {
    if drain.output.is_some() {
        return Ok(true);
    }
    match drain.receiver.try_recv() {
        Ok(result) => drain.output = Some(result?),
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            return Err(io::Error::other("command output reader failed"));
        }
    }
    Ok(drain.output.is_some())
}

#[cfg(not(target_os = "macos"))]
fn finish_drain(drain: PipeDrain) -> io::Result<Vec<u8>> {
    drain
        .output
        .ok_or_else(|| io::Error::other("command output reader did not finish"))
}

#[cfg(target_os = "macos")]
struct PipeDrain<R> {
    pipe: R,
    output: Vec<u8>,
    finished: bool,
}

#[cfg(target_os = "macos")]
fn drain<R: Read + std::os::fd::AsRawFd>(pipe: R) -> io::Result<PipeDrain<R>> {
    let descriptor = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(PipeDrain {
        pipe,
        output: Vec::new(),
        finished: false,
    })
}

#[cfg(target_os = "macos")]
fn poll_drain<R: Read>(drain: &mut PipeDrain<R>) -> io::Result<bool> {
    if drain.finished {
        return Ok(true);
    }
    let mut buffer = [0; 4096];
    for _ in 0..16 {
        match drain.pipe.read(&mut buffer) {
            Ok(0) => {
                drain.finished = true;
                break;
            }
            Ok(size) => drain.output.extend_from_slice(&buffer[..size]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(drain.finished)
}

#[cfg(target_os = "macos")]
fn finish_drain<R>(drain: PipeDrain<R>) -> io::Result<Vec<u8>> {
    drain
        .finished
        .then_some(drain.output)
        .ok_or_else(|| io::Error::other("command output reader did not finish"))
}

#[cfg(target_os = "macos")]
struct ProcessTree;

#[cfg(target_os = "macos")]
impl ProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        Ok(Self)
    }

    fn attach(&self, _child: &std::process::Child) -> io::Result<()> {
        Ok(())
    }

    fn terminate(&self, child: &mut std::process::Child) {
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

    fn attach(&self, child: &std::process::Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        resume_primary_thread(child.id())
    }

    fn terminate(&self, child: &mut std::process::Child) {
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
        .ok_or_else(|| io::Error::other("suspended command thread could not be resumed"))
}

#[cfg(not(any(target_os = "macos", windows)))]
struct ProcessTree;

#[cfg(not(any(target_os = "macos", windows)))]
impl ProcessTree {
    fn prepare(_command: &mut Command) -> io::Result<Self> {
        Ok(Self)
    }

    fn attach(&self, _child: &std::process::Child) -> io::Result<()> {
        Ok(())
    }

    fn terminate(&self, child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;

    use super::*;

    const FIXTURE_MODE: &str = "RUNTIME_ATLAS_COMMAND_TIMEOUT_FIXTURE";
    const FIXTURE_PID: &str = "RUNTIME_ATLAS_COMMAND_TIMEOUT_PID";

    #[test]
    fn timeout_fixture() {
        let Ok(mode) = std::env::var(FIXTURE_MODE) else {
            return;
        };
        match mode.as_str() {
            "parent" => {
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "command::tests::timeout_fixture", "--nocapture"])
                    .env(FIXTURE_MODE, "leaf")
                    .spawn()
                    .unwrap();
                std::fs::write(
                    std::env::var_os(FIXTURE_PID).unwrap(),
                    child.id().to_string(),
                )
                .unwrap();
                let _ = child.wait();
            }
            #[cfg(target_os = "macos")]
            "escape_parent" => {
                use std::os::unix::process::CommandExt;

                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "command::tests::timeout_fixture", "--nocapture"])
                    .env(FIXTURE_MODE, "escaped")
                    .env(FIXTURE_PID, std::env::var_os(FIXTURE_PID).unwrap());
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let mut child = command.spawn().unwrap();
                let path = std::env::var_os(FIXTURE_PID).unwrap();
                while !std::path::Path::new(&path).exists() {
                    thread::sleep(Duration::from_millis(5));
                }
                let _ = child.try_wait();
            }
            #[cfg(target_os = "macos")]
            "escaped" => {
                let identity = mac_process_identity(std::process::id()).unwrap();
                std::fs::write(
                    std::env::var_os(FIXTURE_PID).unwrap(),
                    format!("{}\n{identity}\n", std::process::id()),
                )
                .unwrap();
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
            }
            _ => {
                std::io::stdout()
                    .write_all(&vec![b'o'; 256 * 1024])
                    .unwrap();
                std::io::stderr()
                    .write_all(&vec![b'e'; 256 * 1024])
                    .unwrap();
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
            }
        }
    }

    #[test]
    fn stuck_command_and_descendant_are_bounded_and_terminated() {
        let temporary = tempdir().unwrap();
        let pid_path = temporary.path().join("descendant.pid");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "command::tests::timeout_fixture", "--nocapture"])
            .env(FIXTURE_MODE, "parent")
            .env(FIXTURE_PID, &pid_path);

        let started = Instant::now();
        let error = output_with_timeout(&mut command, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid: u32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(pid), "descendant {pid} survived timeout");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn escaped_process_holding_pipes_cannot_extend_the_deadline() {
        let temporary = tempdir().unwrap();
        let pid_path = temporary.path().join("escaped.pid");
        let worker_path = pid_path.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "command::tests::timeout_fixture", "--nocapture"])
                .env(FIXTURE_MODE, "escape_parent")
                .env(FIXTURE_PID, worker_path);
            let started = Instant::now();
            let result = output_with_timeout(&mut command, Duration::from_millis(500));
            let _ = sender.send((started.elapsed(), result));
        });

        let result = receiver.recv_timeout(Duration::from_secs(2));
        let (pid, identity) = read_fixture_identity(&pid_path);
        stop_exact_fixture(pid, &identity);
        let (elapsed, result) = result.expect("bounded runner did not return by its deadline");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(elapsed < Duration::from_secs(1));
    }

    #[cfg(target_os = "macos")]
    fn read_fixture_identity(path: &std::path::Path) -> (u32, String) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(value) = std::fs::read_to_string(path) {
                let mut lines = value.lines();
                let pid = lines.next().unwrap().parse().unwrap();
                let identity = lines.next().unwrap().to_owned();
                return (pid, identity);
            }
            assert!(Instant::now() < deadline, "escaped fixture did not start");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(target_os = "macos")]
    fn stop_exact_fixture(pid: u32, expected_identity: &str) {
        assert_eq!(
            mac_process_identity(pid).as_deref(),
            Some(expected_identity),
            "escaped fixture identity changed before cleanup"
        );
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        while mac_process_identity(pid).as_deref() == Some(expected_identity)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(
            mac_process_identity(pid).as_deref(),
            Some(expected_identity)
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

    #[cfg(target_os = "macos")]
    fn process_exists(pid: u32) -> bool {
        i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }

    #[cfg(windows)]
    fn process_exists(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            return false;
        }
        let exists = unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0;
        unsafe { CloseHandle(process) };
        exists
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn process_exists(_pid: u32) -> bool {
        false
    }
}
