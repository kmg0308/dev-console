use std::{fs, io, path::Path};

#[cfg(not(target_os = "windows"))]
pub(crate) fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)?;
    let directory = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(directory)?.sync_all()
}

#[cfg(target_os = "windows")]
pub(crate) fn replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};

    if !destination.exists() {
        fs::rename(source, destination)?;
        return sync_windows_file(destination);
    }
    replace_windows_with(source, destination, |source, destination, backup| {
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let backup: Vec<u16> = backup.as_os_str().encode_wide().chain(Some(0)).collect();
        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn ReplaceFileW(
                replaced_file_name: *const u16,
                replacement_file_name: *const u16,
                backup_file_name: *const u16,
                replace_flags: u32,
                exclude: *mut std::ffi::c_void,
                reserved: *mut std::ffi::c_void,
            ) -> i32;
        }
        // SAFETY: all paths are NUL-terminated UTF-16 buffers valid for this call.
        let replaced = unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                backup.as_ptr(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if replaced == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

#[cfg(target_os = "windows")]
fn replace_windows_with(
    source: &Path,
    destination: &Path,
    attempt: impl FnOnce(&Path, &Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let backup = windows_backup_sibling(source);
    match attempt(source, destination, &backup) {
        Ok(()) => finish_windows_replacement(destination, &backup),
        Err(error) => {
            if destination.exists()
                || (!is_windows_move_failure(error.raw_os_error()) && !backup.exists())
            {
                return Err(error);
            }

            if source.exists() && fs::rename(source, destination).is_ok() {
                return finish_windows_replacement(destination, &backup);
            }
            if backup.exists() && fs::rename(&backup, destination).is_ok() {
                let _ = sync_windows_file(destination);
            }
            Err(error)
        }
    }
}

#[cfg(target_os = "windows")]
fn finish_windows_replacement(destination: &Path, backup: &Path) -> io::Result<()> {
    // File::sync_all maps to the public FlushFileBuffers contract. Windows has
    // no supported std API for flushing the containing directory entry.
    sync_windows_file(destination)?;
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn sync_windows_file(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

#[cfg(target_os = "windows")]
fn windows_backup_sibling(source: &Path) -> std::path::PathBuf {
    let mut name = source.as_os_str().to_owned();
    name.push(".replaced-backup");
    name.into()
}

#[cfg(any(test, target_os = "windows"))]
fn is_windows_move_failure(code: Option<i32>) -> bool {
    matches!(code, Some(1176 | 1177))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_documented_replace_file_move_failures() {
        assert!(is_windows_move_failure(Some(1176)));
        assert!(is_windows_move_failure(Some(1177)));
        assert!(!is_windows_move_failure(Some(1175)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_1176_keeps_both_original_names_with_a_backup_argument() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("new.tmp");
        let destination = directory.path().join("settings.json");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();

        let error = replace_windows_with(&source, &destination, |_, _, backup| {
            assert!(!backup.exists());
            Err(io::Error::from_raw_os_error(1176))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(1176));
        assert_eq!(fs::read(&source).unwrap(), b"new");
        assert_eq!(fs::read(&destination).unwrap(), b"old");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_1177_recovers_the_new_canonical_file_and_keeps_no_partial_name() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("new.tmp");
        let destination = directory.path().join("settings.json");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();

        replace_windows_with(&source, &destination, |_, destination, backup| {
            fs::rename(destination, backup)?;
            Err(io::Error::from_raw_os_error(1177))
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!source.exists());
        assert!(!windows_backup_sibling(&source).exists());
    }
}
