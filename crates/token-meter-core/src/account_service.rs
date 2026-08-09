use crate::account::{CodexAccountUsage, CodexAccountUsageError, parse_rate_limits_response};
use chrono::Utc;
use serde::Deserialize;
use std::{
    ffi::OsStr,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
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
    executable: &OsStr,
    timeout: Duration,
) -> Result<CodexAccountUsage, CodexAccountUsageServiceError> {
    let mut command = Command::new(executable);
    command
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => CodexAccountUsageServiceError::ExecutableNotFound,
        _ => CodexAccountUsageServiceError::LaunchFailed,
    })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or(CodexAccountUsageServiceError::LaunchFailed)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(CodexAccountUsageServiceError::LaunchFailed)?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or(CodexAccountUsageServiceError::LaunchFailed)?;

    let (lines, receiver) = mpsc::channel();
    let stdout_thread = thread::spawn(move || read_lines(stdout, lines));
    let stderr_thread = thread::spawn(move || io::copy(&mut stderr, &mut io::sink()));

    let result = exchange(&mut child, &mut stdin, receiver, timeout);
    drop(stdin);
    stop(&mut child);
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    result
}

fn exchange(
    child: &mut Child,
    stdin: &mut impl Write,
    lines: mpsc::Receiver<Result<Vec<u8>, io::Error>>,
    timeout: Duration,
) -> Result<CodexAccountUsage, CodexAccountUsageServiceError> {
    stdin
        .write_all(INITIALIZE)
        .and_then(|_| stdin.flush())
        .map_err(|_| closed(child))?;
    let started = Instant::now();
    let mut initialized = false;

    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .ok_or(CodexAccountUsageServiceError::TimedOut)?;
        let line = match lines.recv_timeout(remaining.min(PROCESS_POLL_INTERVAL)) {
            Ok(Ok(line)) => line,
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(closed(child));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => match child.try_wait() {
                Ok(Some(status)) if !status.success() => return Err(failed(status)),
                Ok(Some(_)) => {
                    return Err(CodexAccountUsageServiceError::ConnectionClosed);
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

fn read_lines(
    stdout: impl Read,
    sender: mpsc::Sender<Result<Vec<u8>, io::Error>>,
) -> io::Result<()> {
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
        if line.len() as u64 > MAX_LINE_BYTES {
            let _ = sender.send(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex JSON-RPC frame is too large",
            )));
            return Ok(());
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if !line.is_empty() && sender.send(Ok(line)).is_err() {
            return Ok(());
        }
    }
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

fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
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

    #[test]
    fn app_server_success_error_timeout_and_exit_contracts() {
        let fixture = tempdir().unwrap();
        let executable = build_fixture(fixture.path());

        let success = copy_as(&executable, fixture.path(), "success");
        let usage = fetch_codex_account_usage(success.as_os_str(), Duration::from_secs(2)).unwrap();
        assert_eq!(usage.five_hour_window.unwrap().used_percent, 31);

        let rpc_error = copy_as(&executable, fixture.path(), "rpc_error");
        assert_eq!(
            fetch_codex_account_usage(rpc_error.as_os_str(), Duration::from_secs(2)),
            Err(CodexAccountUsageServiceError::Response(
                CodexAccountUsageError::Server("Login required".into())
            ))
        );

        let timeout = copy_as(&executable, fixture.path(), "timeout");
        assert_eq!(
            fetch_codex_account_usage(timeout.as_os_str(), Duration::from_millis(50)),
            Err(CodexAccountUsageServiceError::TimedOut)
        );

        let interrupted = copy_as(&executable, fixture.path(), "interrupted");
        assert_eq!(
            fetch_codex_account_usage(interrupted.as_os_str(), Duration::from_secs(2)),
            Err(CodexAccountUsageServiceError::ConnectionClosed)
        );

        let failed = copy_as(&executable, fixture.path(), "failed");
        for _ in 0..10 {
            assert_eq!(
                fetch_codex_account_usage(failed.as_os_str(), Duration::from_secs(2)),
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

    const FIXTURE: &str = r#"
use std::{env, io::{BufRead, BufReader, Write}, thread, time::Duration};

fn main() {
    if env::var_os("TOKENMETER_KEEP_STDOUT_OPEN").is_some() {
        thread::sleep(Duration::from_millis(500));
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
            std::process::Command::new(env::current_exe().unwrap())
                .env("TOKENMETER_KEEP_STDOUT_OPEN", "1")
                .spawn()
                .unwrap();
        }
        "failed" => { eprintln!("secret stderr must not escape"); std::process::exit(7); }
        _ => unreachable!(),
    }
}
"#;
}
