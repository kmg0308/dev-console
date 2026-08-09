#![cfg(unix)]

use std::{
    fs::{self, OpenOptions, TryLockError},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

fn supervisor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_runtime-atlas-supervisor"))
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(1));
    }
    None
}

fn executable_fixture(directory: &Path, contents: &str) -> PathBuf {
    let fixture = directory.join("fixture");
    fs::write(&fixture, contents).unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o755)).unwrap();
    fixture
}

fn active_control_file(directory: &Path) -> PathBuf {
    let control = directory.join("session.control");
    fs::write(&control, [0]).unwrap();
    fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
    control
}

#[cfg(target_os = "macos")]
fn mac_start_identity(pid: u32) -> String {
    use std::mem::{MaybeUninit, size_of};

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    assert_eq!(read as usize, size_of::<libc::proc_bsdinfo>());
    let info = unsafe { info.assume_init() };
    format!(
        "macos:{}:{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )
}

#[test]
fn reports_usage_with_the_contract_exit_code() {
    let output = supervisor().output().unwrap();
    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("usage: runtime-atlas-supervisor"));
}

#[test]
fn rejects_incomplete_or_invalid_managed_session_arguments_as_usage() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("session.json");
    let control = active_control_file(directory.path());
    let incomplete = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert_eq!(incomplete.status.code(), Some(64));

    let invalid = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .args(["--control-file"])
        .arg(&control)
        .args([
            "--action-id",
            "00000000-0000-0000-0000-000000000000",
            "--worktree",
            "relative",
            "--cwd",
        ])
        .arg(directory.path())
        .args(["--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(64));
}

#[test]
fn reports_launch_failure_with_the_contract_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let output = supervisor()
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/path/that/does/not/exist"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(71));
}

#[test]
fn preserves_cwd_arguments_streams_and_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = executable_fixture(
        directory.path(),
        "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$PWD\" \"$1\" \"$2\"\nprintf 'fixture-error\\n' >&2\nexit 7\n",
    );

    let output = supervisor()
        .arg("--cwd")
        .arg(directory.path())
        .arg("--")
        .arg(&fixture)
        .args(["first argument", "--literal"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "{}\nfirst argument\n--literal\n",
            directory.path().canonicalize().unwrap().display()
        )
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "fixture-error\n");
}

#[test]
fn immediate_sigterm_is_never_lost_during_startup() {
    for attempt in 0..1_000 {
        let mut child = supervisor()
            .args(["--cwd", "/tmp", "--", "/bin/sleep", "5"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        if wait_for_exit(&mut child, Duration::from_millis(500)).is_none() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = wait_for_exit(&mut child, Duration::from_secs(3));
            let _ = child.kill();
            let _ = child.wait();
            panic!("SIGTERM was lost during startup attempt {attempt}");
        }
    }
}

#[test]
fn termination_removes_descendants_that_ignore_term() {
    let directory = tempfile::tempdir().unwrap();
    let descendant_pid_file = directory.path().join("descendant.pid");
    let ready_file = directory.path().join("ready");
    let fixture = executable_fixture(
        directory.path(),
        "#!/bin/sh\n/bin/sh -c 'trap \"\" TERM INT; pid_file=$1; printf \"%s\\n\" \"$$\" > \"${pid_file}.tmp\"; mv \"${pid_file}.tmp\" \"$pid_file\"; while :; do sleep 1; done' ignored \"$1\" &\ntrap 'exit 0' TERM INT\nprintf ready > \"$2\"\nwhile :; do sleep 1; done\n",
    );
    let mut supervisor = supervisor()
        .args(["--cwd"])
        .arg(directory.path())
        .arg("--")
        .arg(fixture)
        .arg(&descendant_pid_file)
        .arg(&ready_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!ready_file.exists() || !descendant_pid_file.exists()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready_file.exists() && descendant_pid_file.exists());
    let descendant_pid: i32 = fs::read_to_string(&descendant_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) },
        0
    );
    if wait_for_exit(&mut supervisor, Duration::from_secs(4)).is_none() {
        let group = unsafe { libc::getpgid(descendant_pid) };
        if group > 0 {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        panic!("supervisor did not finish bounded process-group cleanup");
    }
    assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, -1);
}

#[test]
fn normal_root_exit_does_not_orphan_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let descendant_pid_file = directory.path().join("descendant.pid");
    let fixture = executable_fixture(
        directory.path(),
        "#!/bin/sh\n/bin/sh -c 'trap \"\" TERM INT; pid_file=$1; printf \"%s\\n\" \"$$\" > \"${pid_file}.tmp\"; mv \"${pid_file}.tmp\" \"$pid_file\"; while :; do sleep 1; done' ignored \"$1\" &\nwhile [ ! -s \"$1\" ]; do sleep 0.01; done\nexit 0\n",
    );
    let mut supervisor = supervisor()
        .args(["--cwd"])
        .arg(directory.path())
        .arg("--")
        .arg(fixture)
        .arg(&descendant_pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !descendant_pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(descendant_pid_file.exists());
    let descendant_pid: i32 = fs::read_to_string(&descendant_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    assert!(wait_for_exit(&mut supervisor, Duration::from_secs(4)).is_some());
    assert_eq!(unsafe { libc::kill(descendant_pid, 0) }, -1);
}

#[cfg(target_os = "macos")]
#[test]
fn writes_exact_session_marker_before_launch_and_cleans_it_after_exit() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let marker = directory.path().join("session.json");
    let control = active_control_file(directory.path());
    let session_id = "018f47a2-bbab-7de0-8000-0123456789ab";
    let action_id = "018f47a2-bbab-7de0-8000-0123456789ac";
    let mut supervisor = supervisor()
        .args(["--session-id", session_id, "--session-file"])
        .arg(&marker)
        .arg("--control-file")
        .arg(&control)
        .args(["--action-id", action_id, "--worktree"])
        .arg(directory.path())
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/bin/sleep", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(50));
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(document.as_object().unwrap().len(), 6);
    assert_eq!(document["schemaVersion"], 2);
    assert_eq!(document["sessionId"], session_id);
    assert_eq!(document["actionID"], action_id);
    assert_eq!(
        document["worktreePath"],
        directory
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(document["supervisorPID"], supervisor.id());
    assert_eq!(
        document["startIdentity"],
        mac_start_identity(supervisor.id())
    );

    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) },
        0
    );
    assert!(wait_for_exit(&mut supervisor, Duration::from_secs(3)).is_some());
    assert!(!marker.exists());
    assert!(!control.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn immediate_sigterm_after_marker_publication_cleans_the_marker() {
    for attempt in 0..200 {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let marker = directory.path().join("session.json");
        let control = active_control_file(directory.path());
        let mut supervisor = supervisor()
            .args([
                "--session-id",
                "018f47a2-bbab-7de0-8000-0123456789ab",
                "--session-file",
            ])
            .arg(&marker)
            .arg("--control-file")
            .arg(&control)
            .args([
                "--action-id",
                "018f47a2-bbab-7de0-8000-0123456789ac",
                "--worktree",
            ])
            .arg(directory.path())
            .arg("--cwd")
            .arg(directory.path())
            .args(["--", "/bin/sleep", "5"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !marker.exists() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(
            marker.exists(),
            "marker was not published on attempt {attempt}"
        );
        assert_eq!(
            unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) },
            0
        );
        assert!(wait_for_exit(&mut supervisor, Duration::from_secs(3)).is_some());
        assert!(!marker.exists(), "marker remained on attempt {attempt}");
        assert!(!control.exists(), "control remained on attempt {attempt}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn never_overwrites_an_existing_session_marker() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let marker = directory.path().join("session.json");
    let control = active_control_file(directory.path());
    fs::write(&marker, b"existing marker\n").unwrap();
    let output = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .arg("--control-file")
        .arg(&control)
        .args([
            "--action-id",
            "018f47a2-bbab-7de0-8000-0123456789ac",
            "--worktree",
        ])
        .arg(directory.path())
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(71));
    assert_eq!(fs::read(&marker).unwrap(), b"existing marker\n");
    assert!(!control.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn cancelled_control_never_publishes_a_marker_or_starts_the_child() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let marker = directory.path().join("session.json");
    let control = directory.path().join("session.control");
    fs::write(&control, [1]).unwrap();
    fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
    let side_effect = directory.path().join("child-started");
    let fixture = executable_fixture(
        directory.path(),
        "#!/bin/sh\nprintf started > \"$1\"\nexit 0\n",
    );

    let output = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .arg("--control-file")
        .arg(&control)
        .args([
            "--action-id",
            "018f47a2-bbab-7de0-8000-0123456789ac",
            "--worktree",
        ])
        .arg(directory.path())
        .arg("--cwd")
        .arg(directory.path())
        .arg("--")
        .arg(fixture)
        .arg(&side_effect)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(71));
    assert!(!marker.exists());
    assert!(!control.exists());
    assert!(!side_effect.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn control_shared_lock_is_held_for_the_supervisor_lifetime() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let marker = directory.path().join("session.json");
    let control = active_control_file(directory.path());
    let mut supervisor = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .arg("--control-file")
        .arg(&control)
        .args([
            "--action-id",
            "018f47a2-bbab-7de0-8000-0123456789ac",
            "--worktree",
        ])
        .arg(directory.path())
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/bin/sleep", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists());

    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&control)
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) },
        0
    );
    assert!(wait_for_exit(&mut supervisor, Duration::from_secs(3)).is_some());
    contender.lock().unwrap();
    contender.unlock().unwrap();
    assert!(!marker.exists());
    assert!(!control.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn rejects_a_control_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let marker = directory.path().join("session.json");
    let target = directory.path().join("target.control");
    let control = directory.path().join("session.control");
    fs::write(&target, [0]).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &control).unwrap();

    let output = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .arg("--control-file")
        .arg(&control)
        .args([
            "--action-id",
            "018f47a2-bbab-7de0-8000-0123456789ac",
            "--worktree",
        ])
        .arg(directory.path())
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/usr/bin/true"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(71));
    assert!(!marker.exists());
    assert_eq!(fs::read(&target).unwrap(), [0]);
}

#[cfg(target_os = "macos")]
#[test]
fn never_removes_a_replacement_control_file() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let marker = directory.path().join("session.json");
    let control = active_control_file(directory.path());
    let mut supervisor = supervisor()
        .args([
            "--session-id",
            "018f47a2-bbab-7de0-8000-0123456789ab",
            "--session-file",
        ])
        .arg(&marker)
        .arg("--control-file")
        .arg(&control)
        .args([
            "--action-id",
            "018f47a2-bbab-7de0-8000-0123456789ac",
            "--worktree",
        ])
        .arg(directory.path())
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "/bin/sleep", "5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists());

    fs::remove_file(&control).unwrap();
    fs::write(&control, [9]).unwrap();
    fs::set_permissions(&control, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) },
        0
    );
    assert!(wait_for_exit(&mut supervisor, Duration::from_secs(3)).is_some());
    assert_eq!(fs::read(&control).unwrap(), [9]);
}
