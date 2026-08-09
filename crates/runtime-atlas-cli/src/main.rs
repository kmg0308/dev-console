use std::io::{self, Write};
use std::path::{Path, PathBuf};

use runtime_atlas_core::models::{AppLanguage, CustomActionDefinition};
use runtime_atlas_core::observe::{observe_docker, observe_processes};
use runtime_atlas_core::relations::PathFlavor;
use runtime_atlas_core::service::build_observed_snapshot;
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
        let loaded = ConfigurationStore::new(&paths).load().map_err(|_| ())?;
        let snapshot = build_observed_snapshot(
            loaded.value,
            loaded.recovery_notice,
            system_language(),
            git_executable(),
            path_flavor(),
            observe_processes(),
            observe_docker(Some(Path::new("docker"))),
        );
        write_json(stdout, &snapshot)
    })();
    match result {
        Ok(()) => 0,
        Err(()) => write(stderr, "Runtime Atlas status could not be read.\n", 1),
    }
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionCatalog {
    schema_version: u32,
    actions: Vec<CustomActionDefinition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime_atlas_core::STATUS_SCHEMA_VERSION;
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
        assert_eq!(status["schemaVersion"], STATUS_SCHEMA_VERSION);
        assert!(status["repositories"].is_array());
    }
}
