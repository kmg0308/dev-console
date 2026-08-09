#![cfg(windows)]

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FALSE, STILL_ACTIVE},
    System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    },
};

fn supervisor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_runtime-atlas-supervisor"))
}

fn system32() -> PathBuf {
    PathBuf::from(env::var_os("SystemRoot").unwrap()).join("System32")
}

fn path_with_first(directory: &Path) -> OsString {
    let mut paths = vec![directory.to_owned()];
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path));
    }
    env::join_paths(paths).unwrap()
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

fn process_is_active(pid: u32) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0;
    let active = unsafe { GetExitCodeProcess(process, &mut exit_code) } != 0
        && exit_code == STILL_ACTIVE as u32;
    unsafe { CloseHandle(process) };
    active
}

fn stop_process(pid: u32) {
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) };
    if !process.is_null() {
        unsafe {
            TerminateProcess(process, 1);
            CloseHandle(process);
        }
    }
}

#[test]
fn preserves_windows_cwd_arguments_streams_and_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture.ps1");
    fs::write(
        &fixture,
        "[IO.File]::WriteAllText('.runtime-atlas-cwd-proof', 'same-directory')\n[Console]::Out.WriteLine($args[0])\n[Console]::Out.WriteLine($args[1])\n[Console]::Error.WriteLine('fixture-error')\nexit 7\n",
    )
    .unwrap();

    let output = supervisor()
        .arg("--cwd")
        .arg(directory.path())
        .args([
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&fixture)
        .args(["first argument", "--literal"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    let lines: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines, ["first argument", "--literal"]);
    assert_eq!(
        fs::read_to_string(directory.path().join(".runtime-atlas-cwd-proof")).unwrap(),
        "same-directory"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "fixture-error"
    );
}

#[test]
fn propagates_batch_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("exit.cmd");
    fs::write(&fixture, "@EXIT /b 17\r\n").unwrap();

    let output = supervisor()
        .arg("--cwd")
        .arg(directory.path())
        .arg("--")
        .arg(&fixture)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(17));
}

#[test]
fn rejects_batch_newlines_and_oversized_commands_before_spawn() {
    let directory = tempfile::tempdir().unwrap();
    let side_effect = directory.path().join("was-run");
    let fixture = directory.path().join("fixture.cmd");
    fs::write(&fixture, "@ECHO ran>was-run\r\n").unwrap();

    for argument in [
        OsString::from("line\nvalue"),
        OsString::from("line\rvalue"),
        OsString::from("x".repeat(9_000)),
    ] {
        let output = supervisor()
            .arg("--cwd")
            .arg(directory.path())
            .arg("--")
            .arg(&fixture)
            .arg(argument)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(71));
        assert!(!side_effect.exists());
    }
}

#[test]
fn resolves_bare_executables_from_path_without_searching_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let path_directory = directory.path().join("path");
    let cwd = directory.path().join("cwd");
    fs::create_dir_all(&path_directory).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let system32 = system32();
    let executable_name = "runtime-atlas-path-fixture";
    fs::copy(
        system32.join("where.exe"),
        path_directory.join(format!("{executable_name}.exe")),
    )
    .unwrap();
    fs::copy(
        system32.join("whoami.exe"),
        cwd.join(format!("{executable_name}.exe")),
    )
    .unwrap();

    let output = supervisor()
        .env("PATH", &path_directory)
        .env("PATHEXT", ".EXE;.CMD;.BAT")
        .arg("--cwd")
        .arg(&cwd)
        .args(["--", executable_name, executable_name])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains(&format!("{executable_name}.exe"))
    );
}

#[test]
fn resolves_each_path_directory_before_the_next_pathext_candidate() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    fs::write(first.join("chooser.cmd"), "@EXIT /b 11\r\n").unwrap();
    fs::copy(system32().join("where.exe"), second.join("chooser.exe")).unwrap();

    let output = supervisor()
        .env("PATH", env::join_paths([&first, &second]).unwrap())
        .env("PATHEXT", ".EXE;.CMD;.BAT")
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "chooser"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(11));
}

#[test]
fn rejects_an_empty_path_instead_of_falling_back_to_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let system32 = system32();
    let executable_name = "runtime-atlas-cwd-only.exe";
    fs::copy(
        system32.join("where.exe"),
        directory.path().join(executable_name),
    )
    .unwrap();

    let output = supervisor()
        .env("PATH", "")
        .env("PATHEXT", ".EXE;.CMD;.BAT")
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", executable_name, executable_name])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(71));
}

#[test]
fn resolves_an_explicit_supported_extension_without_pathext() {
    let directory = tempfile::tempdir().unwrap();
    let path_directory = directory.path().join("path");
    fs::create_dir(&path_directory).unwrap();
    let executable_name = "runtime-atlas-explicit.exe";
    fs::copy(
        system32().join("where.exe"),
        path_directory.join(executable_name),
    )
    .unwrap();

    let output = supervisor()
        .env("PATH", &path_directory)
        .env_remove("PATHEXT")
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", executable_name, executable_name])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn rejects_verbatim_batch_paths_before_cmd_evaluation() {
    let directory = tempfile::tempdir().unwrap();
    let side_effect = directory.path().join("was-run");
    let fixture = directory.path().join("fixture.cmd");
    fs::write(&fixture, "@ECHO ran>was-run\r\n").unwrap();
    let verbatim = PathBuf::from(format!(r"\\?\{}", fixture.display()));

    let output = supervisor()
        .arg("--cwd")
        .arg(directory.path())
        .arg("--")
        .arg(verbatim)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(71));
    assert!(!side_effect.exists());
}

#[test]
fn rejects_relative_executable_paths_with_separators() {
    let directory = tempfile::tempdir().unwrap();
    fs::copy(
        system32().join("where.exe"),
        directory.path().join("fixture.exe"),
    )
    .unwrap();

    let output = supervisor()
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", r".\fixture.exe", "fixture.exe"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(71));
}

#[test]
fn killing_the_supervisor_closes_the_job_and_removes_batch_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let fixture_directory = directory.path().join("fixture");
    fs::create_dir(&fixture_directory).unwrap();
    let script = fixture_directory.join("descendant.ps1");
    fs::write(
        &script,
        "$child = Start-Process -FilePath \"$env:SystemRoot\\System32\\ping.exe\" -ArgumentList \"-t\",\"127.0.0.1\" -PassThru\n[IO.File]::WriteAllText($args[0], [string]$child.Id)\n$child.WaitForExit()\n",
    )
    .unwrap();
    let fixture = fixture_directory.join("descendant.cmd");
    fs::write(
        &fixture,
        "@ECHO off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0descendant.ps1\" %*\r\n",
    )
    .unwrap();
    let pid_file = directory.path().join("descendant.pid");
    let mut supervisor = supervisor()
        .env("PATH", path_with_first(&fixture_directory))
        .env("PATHEXT", ".CMD;.EXE;.BAT")
        .arg("--cwd")
        .arg(directory.path())
        .args(["--", "descendant"])
        .arg(&pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let descendant_pid = loop {
        if let Ok(value) = fs::read_to_string(&pid_file)
            && let Ok(pid) = value.parse::<u32>()
        {
            break pid;
        }
        if Instant::now() >= deadline {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            panic!("batch descendant did not start");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(process_is_active(descendant_pid));

    supervisor.kill().unwrap();
    assert!(wait_for_exit(&mut supervisor, Duration::from_secs(3)).is_some());
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_is_active(descendant_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if process_is_active(descendant_pid) {
        stop_process(descendant_pid);
        panic!("batch descendant survived the supervisor Job Object");
    }
}
