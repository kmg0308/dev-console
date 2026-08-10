#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() {
    use std::{
        env, fs,
        time::{Duration, Instant},
    };
    use token_meter_core::account_service::{
        CodexAccountUsageServiceError, fetch_codex_account_usage,
    };

    match env::args_os().nth(1).as_deref() {
        Some(mode) if mode == std::ffi::OsStr::new("--fixture") => {
            fixture();
            return;
        }
        Some(mode) if mode == std::ffi::OsStr::new("--hold") => {
            let ready = env::var_os("TOKENMETER_ACCOUNT_HOLD_READY").unwrap();
            fs::write(ready, std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(10));
            return;
        }
        _ => {}
    }

    let directory = tempfile::tempdir().unwrap();
    let executable = env::current_exe().unwrap();
    fs::write(
        directory.path().join("codex.cmd"),
        format!("@echo off\r\n\"{}\" --fixture %*\r\n", executable.display()),
    )
    .unwrap();
    unsafe {
        env::set_var("PATH", directory.path());
        env::set_var("PATHEXT", ".CMD;.EXE;.BAT");
    }

    let usage = fetch_codex_account_usage(None, Duration::from_secs(2)).unwrap();
    assert_eq!(usage.five_hour_window.unwrap().used_percent, 31);

    let ready = directory.path().join("hold.ready");
    unsafe {
        env::set_var("TOKENMETER_ACCOUNT_FIXTURE_MODE", "descendant");
        env::set_var("TOKENMETER_ACCOUNT_HOLD_READY", &ready);
    }
    let started = Instant::now();
    assert_eq!(
        fetch_codex_account_usage(None, Duration::from_secs(2)),
        Err(CodexAccountUsageServiceError::ConnectionClosed)
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    let pid = fs::read_to_string(ready).unwrap().parse().unwrap();
    assert_process_stopped(pid);
}

#[cfg(target_os = "windows")]
fn assert_process_stopped(pid: u32) {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if !process.is_null() {
        assert_eq!(
            unsafe { WaitForSingleObject(process, 3_000) },
            WAIT_OBJECT_0
        );
        unsafe { CloseHandle(process) };
    }
}

#[cfg(target_os = "windows")]
fn fixture() {
    use std::io::{BufRead, Write};

    assert_eq!(
        std::env::args().skip(2).collect::<Vec<_>>(),
        ["app-server", "--stdio"]
    );
    let mut input = std::io::BufReader::new(std::io::stdin());
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
    if std::env::var_os("TOKENMETER_ACCOUNT_FIXTURE_MODE").as_deref()
        == Some(std::ffi::OsStr::new("descendant"))
    {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--hold")
            .spawn()
            .unwrap();
        let ready =
            std::path::PathBuf::from(std::env::var_os("TOKENMETER_ACCOUNT_HOLD_READY").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !ready.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready.is_file());
        std::mem::forget(child);
        return;
    }
    println!(
        "{{\"id\":2,\"result\":{{\"rateLimits\":{{\"primary\":{{\"usedPercent\":31,\"windowDurationMins\":300,\"resetsAt\":1783666131}},\"secondary\":null}},\"rateLimitResetCredits\":null}}}}"
    );
}
