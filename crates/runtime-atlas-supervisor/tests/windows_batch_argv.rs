#[cfg(windows)]
use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Write},
    path::Path,
    process::{Command, Output},
};

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    if env::args().nth(1).as_deref() == Some("--capture-argv") {
        capture_argv();
    } else {
        verify_batch_argv();
    }
}

#[cfg(windows)]
fn capture_argv() {
    let mut stdout = io::stdout().lock();
    for argument in env::args().skip(2) {
        stdout.write_all(argument.as_bytes()).unwrap();
        stdout.write_all(&[0]).unwrap();
    }
}

#[cfg(windows)]
fn path_with_first(directory: &Path) -> OsString {
    let mut paths = vec![directory.to_owned()];
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path));
    }
    env::join_paths(paths).unwrap()
}

#[cfg(windows)]
fn captured_arguments(label: &str, output: Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "{label} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = output
        .stdout
        .strip_suffix(&[0])
        .unwrap_or_else(|| panic!("{label} argv capture must end with NUL"));
    bytes
        .split(|byte| *byte == 0)
        .map(|argument| String::from_utf8(argument.to_vec()).unwrap())
        .collect()
}

#[cfg(windows)]
fn verify_batch_argv() {
    let directory = tempfile::tempdir().unwrap();
    let fixture_directory = directory.path().join("fixture (space)");
    fs::create_dir(&fixture_directory).unwrap();
    fs::copy(
        env::current_exe().unwrap(),
        fixture_directory.join("capture.exe"),
    )
    .unwrap();
    let shim = fixture_directory.join("npm-like.cmd");
    fs::write(
        &shim,
        "@ECHO off\r\n\"%~dp0capture.exe\" --capture-argv %*\r\nEXIT /b %ERRORLEVEL%\r\n",
    )
    .unwrap();
    let path = path_with_first(&fixture_directory);
    let side_effect = directory.path().join("owned");
    let arguments = vec![
        "".to_owned(),
        "two words".to_owned(),
        "한글".to_owned(),
        "(parentheses)".to_owned(),
        "space trailing\\".to_owned(),
        "%PATH%".to_owned(),
        "!UNEXPANDED!".to_owned(),
        "^".to_owned(),
        "&".to_owned(),
        "|".to_owned(),
        "<".to_owned(),
        ">".to_owned(),
        "quote\"inside".to_owned(),
        "slashes\\\\\"quote".to_owned(),
        "& echo injected>owned".to_owned(),
    ];

    let standard = Command::new(&shim)
        .env("PATH", &path)
        .env("PATHEXT", ".CMD;.EXE;.BAT")
        .current_dir(directory.path())
        .args(&arguments)
        .output()
        .unwrap();
    let standard_arguments = captured_arguments("std::process::Command", standard);
    assert_eq!(standard_arguments, arguments, "std::process::Command argv");
    assert!(
        !side_effect.exists(),
        "std::process::Command injected a command"
    );

    let supervised = Command::new(env!("CARGO_BIN_EXE_runtime-atlas-supervisor"))
        .env("PATH", &path)
        .env("PATHEXT", ".CMD;.EXE;.BAT")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--")
        .arg(&shim)
        .args(&arguments)
        .output()
        .unwrap();
    let supervised_arguments = captured_arguments("supervisor", supervised);
    assert_eq!(supervised_arguments, arguments, "supervisor argv");
    assert!(!side_effect.exists(), "supervisor injected a command");
}
