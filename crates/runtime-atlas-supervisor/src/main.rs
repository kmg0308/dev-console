use serde::Serialize;
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    process,
};
#[cfg(unix)]
use std::{
    ffi::{CString, OsStr},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawFd, io::FromRawFd},
};
#[cfg(windows)]
use std::{
    mem::size_of,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
};
use uuid::Uuid;

const EX_USAGE: i32 = 64;
const EX_OSERR: i32 = 71;
const USAGE: &str = "usage: runtime-atlas-supervisor [--session-id <uuid> --session-file <absolute path> --control-file <absolute path> --control-identity <identity> --action-id <uuid> --worktree <absolute path>] --cwd <absolute path> -- <executable> [args...]";

struct SessionSpec {
    id: Uuid,
    path: PathBuf,
    control_path: PathBuf,
    control_identity: String,
    action_id: Uuid,
    worktree_path: PathBuf,
}

struct Invocation {
    session: Option<SessionSpec>,
    cwd: PathBuf,
    executable: OsString,
    arguments: Vec<OsString>,
}

fn parse(arguments: Vec<OsString>) -> Option<Invocation> {
    let mut index = 0;
    let session = if arguments
        .first()
        .is_some_and(|value| value == "--session-id")
    {
        let id = arguments.get(1)?.to_str()?.parse::<Uuid>().ok()?;
        let path = Path::new(arguments.get(3)?);
        let control_path = Path::new(arguments.get(5)?);
        let control_identity = arguments.get(7)?.to_str()?;
        let action_id = arguments.get(9)?.to_str()?.parse::<Uuid>().ok()?;
        let worktree = Path::new(arguments.get(11)?);
        if id.is_nil()
            || action_id.is_nil()
            || arguments.get(2)? != "--session-file"
            || !path.is_absolute()
            || arguments.get(4)? != "--control-file"
            || !control_path.is_absolute()
            || arguments.get(6)? != "--control-identity"
            || control_identity.is_empty()
            || arguments.get(8)? != "--action-id"
            || arguments.get(10)? != "--worktree"
            || !worktree.is_absolute()
        {
            return None;
        }
        let worktree_path = fs::canonicalize(worktree).ok()?;
        if !worktree_path.is_dir() {
            return None;
        }
        index = 12;
        Some(SessionSpec {
            id,
            path: path.to_owned(),
            control_path: control_path.to_owned(),
            control_identity: control_identity.to_owned(),
            action_id,
            worktree_path,
        })
    } else {
        None
    };

    let cwd = Path::new(arguments.get(index + 1)?);
    if arguments.get(index)? != "--cwd"
        || !cwd.is_absolute()
        || arguments.get(index + 2)? != "--"
        || arguments
            .get(index + 3)
            .is_none_or(|value| value.is_empty())
    {
        return None;
    }

    Some(Invocation {
        session,
        cwd: cwd.to_owned(),
        executable: arguments[index + 3].clone(),
        arguments: arguments[(index + 4)..].to_vec(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionMarker<'a> {
    schema_version: u32,
    session_id: Uuid,
    #[serde(rename = "actionID")]
    action_id: Uuid,
    worktree_path: &'a Path,
    #[serde(rename = "supervisorPID")]
    supervisor_pid: u32,
    start_identity: &'a str,
}

struct SessionMarkerGuard {
    file: File,
    #[cfg(unix)]
    parent: File,
    #[cfg(windows)]
    _parent: File,
    #[cfg(unix)]
    name: OsString,
}

struct ControlFileGuard {
    file: File,
    parent: File,
    #[cfg(unix)]
    name: OsString,
}

impl ControlFileGuard {
    fn open(path: &Path, expected_identity: &str) -> io::Result<Self> {
        let (parent, name) = open_private_parent(path, "control file")?;
        #[cfg(windows)]
        let _ = &name;

        #[cfg(unix)]
        let file = open_at(
            &parent,
            &name,
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        #[cfg(windows)]
        let file = {
            let file = open_windows_path(path, false)?;
            if !windows_parent_matches(&parent, path.parent().expect("validated parent"))? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "control file parent was replaced while opening",
                ));
            }
            file
        };
        validate_private_control_file(&file.metadata()?)?;
        if file_identity(&file)? != expected_identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control file identity does not match the pending session",
            ));
        }
        file.lock_shared()?;
        let mut guard = Self {
            file,
            parent,
            #[cfg(unix)]
            name,
        };
        guard.file.rewind()?;
        let mut value = [0; 2];
        let read = guard.file.read(&mut value)?;
        match (read, value[0]) {
            (1, 0) => Ok(guard),
            (1, 1) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "managed action was cancelled before launch",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control file must contain exactly one active or cancelled byte",
            )),
        }
    }
}

impl Drop for ControlFileGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = remove_at_if_same(&self.parent, &self.name, &self.file);
            let _ = self.parent.sync_all();
        }
        #[cfg(windows)]
        let _ = remove_windows_file(&self.file);
    }
}

impl SessionMarkerGuard {
    fn create(spec: &SessionSpec, control_parent: &File) -> io::Result<Self> {
        let (parent, name) = open_private_parent(&spec.path, "session file")?;
        #[cfg(windows)]
        let _ = &name;
        if !same_file(&parent, control_parent)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session marker and control file parents do not match",
            ));
        }

        let start_identity = platform::start_identity()?;
        let mut data = serde_json::to_vec(&SessionMarker {
            schema_version: 2,
            session_id: spec.id,
            action_id: spec.action_id,
            worktree_path: &spec.worktree_path,
            supervisor_pid: process::id(),
            start_identity: &start_identity,
        })?;
        data.push(b'\n');
        let temporary_name = OsString::from(format!(
            ".runtime-atlas-session-{}-{}.tmp",
            spec.id,
            process::id()
        ));
        #[cfg(windows)]
        let temporary_path = spec
            .path
            .parent()
            .expect("validated parent")
            .join(&temporary_name);
        #[cfg(unix)]
        let mut file = open_at(
            &parent,
            &temporary_name,
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        #[cfg(windows)]
        let mut file = {
            let file = open_windows_path(&temporary_path, true)?;
            if !matches!(
                windows_parent_matches(&parent, spec.path.parent().expect("validated parent")),
                Ok(true)
            ) {
                let _ = remove_windows_file(&file);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "session file parent was replaced while creating a temporary marker",
                ));
            }
            file
        };
        #[cfg(unix)]
        let mut linked = false;
        let result = (|| {
            file.write_all(&data)?;
            file.sync_all()?;
            #[cfg(unix)]
            link_at(&parent, &temporary_name, &name)?;
            #[cfg(windows)]
            {
                let parent_path = spec.path.parent().expect("validated parent");
                if !windows_parent_matches(&parent, parent_path)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "session file parent was replaced before marker publication",
                    ));
                }
                rename_windows_at(&file, &parent, &spec.path)?;
            }
            #[cfg(unix)]
            {
                linked = true;
            }
            #[cfg(unix)]
            {
                if !same_file_at(&file, &parent, &name)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "published session marker was replaced",
                    ));
                }
                parent.sync_all()?;
                if !remove_at_if_same(&parent, &temporary_name, &file)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "temporary session marker was replaced",
                    ));
                }
                parent.sync_all()
            }
            #[cfg(windows)]
            {
                let parent_path = spec.path.parent().expect("validated parent");
                if !windows_parent_matches(&parent, parent_path)? {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "session file parent was replaced before marker verification",
                    ));
                }
                let published_file = open_windows_probe(&spec.path)?;
                if !windows_parent_matches(&parent, parent_path)?
                    || !same_file(&file, &published_file)?
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "published session marker was replaced",
                    ));
                }
                Ok(())
            }
        })();
        if let Err(error) = result {
            #[cfg(unix)]
            {
                let _ = remove_at_if_same(&parent, &temporary_name, &file);
                if linked {
                    let _ = remove_at_if_same(&parent, &name, &file);
                }
                let _ = parent.sync_all();
            }
            #[cfg(windows)]
            let _ = remove_windows_file(&file);
            return Err(error);
        }
        Ok(Self {
            file,
            #[cfg(unix)]
            parent,
            #[cfg(windows)]
            _parent: parent,
            #[cfg(unix)]
            name,
        })
    }
}

impl Drop for SessionMarkerGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = remove_at_if_same(&self.parent, &self.name, &self.file);
            let _ = self.parent.sync_all();
        }
        #[cfg(windows)]
        let _ = remove_windows_file(&self.file);
    }
}

#[cfg(unix)]
fn open_private_parent(path: &Path, label: &str) -> io::Result<(File, OsString)> {
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} has no parent"),
            )
        })?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{label} has no name")))?
        .to_owned();
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let parent = options.open(parent_path)?;
    validate_private_parent(&parent.metadata()?)?;
    Ok((parent, name))
}

#[cfg(unix)]
fn relative_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name contains NUL"))
}

#[cfg(unix)]
fn open_at(parent: &File, name: &OsStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let name = relative_name(name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn link_at(parent: &File, source: &OsStr, destination: &OsStr) -> io::Result<()> {
    let source = relative_name(source)?;
    let destination = relative_name(destination)?;
    if unsafe {
        libc::linkat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_at(file: &File, parent: &File, name: &OsStr) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let path_file = open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let open = file.metadata()?;
    let path = path_file.metadata()?;
    Ok(open.dev() == path.dev() && open.ino() == path.ino())
}

#[cfg(unix)]
fn remove_at_if_same(parent: &File, name: &OsStr, file: &File) -> io::Result<bool> {
    if !same_file_at(file, parent, name)? {
        return Ok(false);
    }
    let name = relative_name(name)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(unix)]
fn validate_private_parent(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session file parent must be owned by the current user and mode 0700 or stricter",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_control_file(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control file must be a private regular file owned by the current user",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_parent(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session file parent must not be a reparse point",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_control_file(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control file must be a regular, non-reparse file",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn open_private_parent(path: &Path, label: &str) -> io::Result<(File, OsString)> {
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} has no parent"),
            )
        })?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{label} has no name")))?
        .to_owned();
    Ok((open_windows_parent(parent_path, label)?, name))
}

#[cfg(windows)]
fn open_windows_parent(path: &Path, label: &str) -> io::Result<File> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let parent = options.open(path)?;
    let metadata = parent.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{label} parent must be an existing non-reparse directory"),
        ));
    }
    validate_private_parent(&metadata)?;
    Ok(parent)
}

#[cfg(windows)]
fn windows_parent_matches(expected: &File, path: &Path) -> io::Result<bool> {
    open_windows_parent(path, "session file").and_then(|actual| same_file(expected, &actual))
}

#[cfg(windows)]
fn open_windows_path(path: &Path, create_new: bool) -> io::Result<File> {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    if create_new {
        options.create_new(true);
    }
    options.open(path)
}

#[cfg(windows)]
fn open_windows_probe(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(windows)]
fn rename_windows_at(file: &File, _parent: &File, path: &Path) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle},
    };

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session marker path must be absolute",
        ));
    }
    let name = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if name.is_empty() || name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session marker name is invalid",
        ));
    }
    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session marker name is too long",
            )
        })?;
    let buffer_bytes = size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes as usize)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session marker name is too long",
            )
        })?;
    let buffer_words = buffer_bytes.div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; buffer_words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        info.write(FILE_RENAME_INFO::default());
        (*info).Anonymous.ReplaceIfExists = false;
        // Win32 resolves relative names against the process CWD, so use the exact final path.
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        if SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfo,
            info.cast(),
            u32::try_from(buffer_bytes).map_err(io::Error::other)?,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn same_file(first: &File, second: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let first = first.metadata()?;
    let second = second.metadata()?;
    Ok(first.dev() == second.dev() && first.ino() == second.ino())
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<String> {
    let (volume, index) = windows_file_identity(file)?;
    Ok(format!("windows:{volume}:{index}"))
}

#[cfg(windows)]
fn remove_windows_file(file: &File) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
            FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx,
            SetFileInformationByHandle,
        },
    };

    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn same_file(first: &File, second: &File) -> io::Result<bool> {
    Ok(windows_file_identity(first)? == windows_file_identity(second)?)
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u32, u64)> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

fn main() {
    let Some(mut invocation) = parse(std::env::args_os().skip(1).collect()) else {
        eprintln!("{USAGE}");
        process::exit(EX_USAGE);
    };

    let platform = match platform::prepare() {
        Ok(platform) => platform,
        Err(error) => {
            eprintln!("runtime-atlas-supervisor: {error}");
            process::exit(EX_OSERR);
        }
    };
    if let Err(error) = platform::resolve(&mut invocation) {
        platform::cancel(platform);
        eprintln!("runtime-atlas-supervisor: {error}");
        process::exit(EX_OSERR);
    }
    let control = match invocation.session.as_ref() {
        Some(spec) => match ControlFileGuard::open(&spec.control_path, &spec.control_identity) {
            Ok(control) => Some(control),
            Err(error) => {
                platform::cancel(platform);
                eprintln!("runtime-atlas-supervisor: {error}");
                process::exit(EX_OSERR);
            }
        },
        None => None,
    };
    let marker = match invocation.session.take() {
        Some(spec) => match SessionMarkerGuard::create(
            &spec,
            &control
                .as_ref()
                .expect("managed session has control")
                .parent,
        ) {
            Ok(marker) => Some(marker),
            Err(error) => {
                platform::cancel(platform);
                drop(control);
                eprintln!("runtime-atlas-supervisor: {error}");
                process::exit(EX_OSERR);
            }
        },
        None => None,
    };
    let result = platform::run(invocation, platform);
    drop(marker);
    drop(control);
    match result {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("runtime-atlas-supervisor: {error}");
            process::exit(EX_OSERR);
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::Invocation;
    use std::{
        io,
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Child, Command, ExitStatus},
        sync::atomic::{AtomicBool, AtomicI32, Ordering},
        thread,
        time::Duration,
    };

    static CHILD_PROCESS_GROUP: AtomicI32 = AtomicI32::new(0);
    static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

    pub struct State {
        original_signal_mask: libc::sigset_t,
    }

    #[cfg(target_os = "macos")]
    pub fn start_identity() -> io::Result<String> {
        use std::mem::{MaybeUninit, size_of};

        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let read = unsafe {
            libc::proc_pidinfo(
                std::process::id().try_into().map_err(io::Error::other)?,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size_of::<libc::proc_bsdinfo>()
                    .try_into()
                    .map_err(io::Error::other)?,
            )
        };
        if read as usize != size_of::<libc::proc_bsdinfo>() {
            return Err(io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_start_tvsec == 0 && info.pbi_start_tvusec == 0 {
            return Err(io::Error::other("process start identity is unavailable"));
        }
        Ok(format!(
            "macos:{}:{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn start_identity() -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime-atlas-supervisor supports macOS and Windows",
        ))
    }

    extern "C" fn forward_signal(signal: libc::c_int) {
        let group = CHILD_PROCESS_GROUP.load(Ordering::Acquire);
        if group > 0 {
            let first = !TERMINATION_REQUESTED.swap(true, Ordering::AcqRel);
            unsafe { libc::kill(-group, signal) };
            if first && (signal == libc::SIGTERM || signal == libc::SIGINT) {
                unsafe { libc::alarm(2) };
            }
        }
    }

    extern "C" fn force_stop(_: libc::c_int) {
        let group = CHILD_PROCESS_GROUP.load(Ordering::Acquire);
        if group > 0 {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }

    fn block_supervisor_signals() -> io::Result<libc::sigset_t> {
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGTERM);
            libc::sigaddset(&mut blocked, libc::SIGINT);
            libc::sigaddset(&mut blocked, libc::SIGALRM);
        }
        if unsafe { libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(previous)
    }

    fn restore_signal_mask(mask: &libc::sigset_t) -> io::Result<()> {
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn install_handler(signal: libc::c_int, handler: extern "C" fn(libc::c_int)) -> io::Result<()> {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler as usize;
        action.sa_flags = libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn process_group_exists(group: i32) -> io::Result<bool> {
        if unsafe { libc::kill(-group, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error),
        }
    }

    fn fail_wait(
        child: &mut Child,
        previous_mask: Option<&libc::sigset_t>,
        error: io::Error,
    ) -> io::Result<ExitStatus> {
        let group = CHILD_PROCESS_GROUP.load(Ordering::Acquire);
        if group > 0 {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = child.wait();
        CHILD_PROCESS_GROUP.store(0, Ordering::Release);
        unsafe { libc::alarm(0) };
        if let Some(mask) = previous_mask {
            let _ = restore_signal_mask(mask);
        }
        Err(error)
    }

    fn wait_for_child(child: &mut Child) -> io::Result<ExitStatus> {
        let mut status = None;
        loop {
            let previous = match block_supervisor_signals() {
                Ok(previous) => previous,
                Err(error) => return fail_wait(child, None, error),
            };
            if status.is_none() {
                status = match child.try_wait() {
                    Ok(status) => status,
                    Err(error) => return fail_wait(child, Some(&previous), error),
                };
            }
            let group = CHILD_PROCESS_GROUP.load(Ordering::Acquire);
            let descendants_remain = if status.is_some() {
                let exists = match process_group_exists(group) {
                    Ok(exists) => exists,
                    Err(error) => return fail_wait(child, Some(&previous), error),
                };
                if exists && !TERMINATION_REQUESTED.swap(true, Ordering::AcqRel) {
                    unsafe {
                        libc::kill(-group, libc::SIGTERM);
                        libc::alarm(2);
                    }
                }
                exists
            } else {
                false
            };
            if !descendants_remain && let Some(exit_status) = status.take() {
                CHILD_PROCESS_GROUP.store(0, Ordering::Release);
                unsafe { libc::alarm(0) };
                restore_signal_mask(&previous)?;
                return Ok(exit_status);
            }
            if let Err(error) = restore_signal_mask(&previous) {
                return fail_wait(child, None, error);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn prepare() -> io::Result<State> {
        let original_signal_mask = block_supervisor_signals()?;
        if unsafe { libc::setpgid(0, 0) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EPERM)
                || unsafe { libc::getpgrp() != libc::getpid() }
            {
                restore_signal_mask(&original_signal_mask)?;
                return Err(error);
            }
        }
        for (signal, handler) in [
            (libc::SIGTERM, forward_signal as extern "C" fn(_)),
            (libc::SIGINT, forward_signal as extern "C" fn(_)),
            (libc::SIGALRM, force_stop as extern "C" fn(_)),
        ] {
            if let Err(error) = install_handler(signal, handler) {
                restore_signal_mask(&original_signal_mask)?;
                return Err(error);
            }
        }
        Ok(State {
            original_signal_mask,
        })
    }

    pub fn cancel(state: State) {
        let _ = restore_signal_mask(&state.original_signal_mask);
    }

    pub fn resolve(_invocation: &mut Invocation) -> io::Result<()> {
        Ok(())
    }

    pub fn run(invocation: Invocation, state: State) -> io::Result<i32> {
        let original_signal_mask = state.original_signal_mask;

        let mut command = Command::new(invocation.executable);
        command
            .args(invocation.arguments)
            .current_dir(invocation.cwd);
        let child_signal_mask = original_signal_mask;
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let mut default_action: libc::sigaction = std::mem::zeroed();
                default_action.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut default_action.sa_mask);
                for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGALRM] {
                    if libc::sigaction(signal, &default_action, std::ptr::null_mut()) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if libc::sigprocmask(libc::SIG_SETMASK, &child_signal_mask, std::ptr::null_mut())
                    == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                restore_signal_mask(&original_signal_mask)?;
                return Err(error);
            }
        };
        CHILD_PROCESS_GROUP.store(child.id() as i32, Ordering::Release);
        if let Err(error) = restore_signal_mask(&original_signal_mask) {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            CHILD_PROCESS_GROUP.store(0, Ordering::Release);
            return Err(error);
        }
        let status = wait_for_child(&mut child)?;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }
}

#[cfg(windows)]
mod platform {
    use super::Invocation;
    use std::{
        env,
        ffi::{OsStr, OsString, c_void},
        fs, io,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Component, Path, PathBuf, Prefix},
        sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, FILETIME, HANDLE,
            INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
        },
        Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, SearchPathW},
        System::{
            Console::{
                AllocConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT,
                CTRL_SHUTDOWN_EVENT, GenerateConsoleCtrlEvent, GetConsoleCP, GetConsoleWindow,
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                SetConsoleCtrlHandler,
            },
            Environment::GetEnvironmentVariableW,
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            SystemInformation::GetSystemDirectoryW,
            Threading::{
                CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CreateProcessW, GetCurrentProcess,
                GetExitCodeProcess, GetProcessTimes, INFINITE, PROCESS_INFORMATION, ResumeThread,
                STARTF_USESTDHANDLES, STARTUPINFOW, Sleep, TerminateProcess, WaitForSingleObject,
            },
        },
        UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
    };

    static JOB: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static CHILD_PROCESS_GROUP: AtomicU32 = AtomicU32::new(0);
    static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);
    static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);
    static SHUTDOWN_COMPLETE: AtomicBool = AtomicBool::new(false);

    pub struct State;

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct ChildProcess {
        process: OwnedHandle,
        thread: OwnedHandle,
        id: u32,
    }

    pub fn start_identity() -> io::Result<String> {
        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        if unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let created = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        if created == 0 {
            return Err(io::Error::other("process start identity is unavailable"));
        }
        Ok(format!("windows:{created}"))
    }

    fn close_job() {
        let job = JOB.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !job.is_null() {
            unsafe { CloseHandle(job) };
        }
    }

    fn begin_shutdown() {
        let group = CHILD_PROCESS_GROUP.load(Ordering::Acquire);
        if group == 0 || SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
            return;
        }
        if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) } != 0 {
            unsafe { Sleep(2_000) };
        }
        close_job();
        SHUTDOWN_COMPLETE.store(true, Ordering::Release);
    }

    unsafe extern "system" fn forward_console_event(event: u32) -> i32 {
        if !matches!(
            event,
            CTRL_C_EVENT
                | CTRL_BREAK_EVENT
                | CTRL_CLOSE_EVENT
                | CTRL_LOGOFF_EVENT
                | CTRL_SHUTDOWN_EVENT
        ) {
            return FALSE;
        }

        TERMINATION_REQUESTED.store(true, Ordering::Release);
        begin_shutdown();
        TRUE
    }

    fn ensure_console() -> io::Result<()> {
        if unsafe { GetConsoleCP() } == 0 {
            if unsafe { AllocConsole() } == 0 {
                return Err(io::Error::last_os_error());
            }
            let window = unsafe { GetConsoleWindow() };
            if !window.is_null() {
                unsafe { ShowWindow(window, SW_HIDE) };
            }
        }
        Ok(())
    }

    fn duplicate_standard_handle(kind: u32) -> io::Result<OwnedHandle> {
        let source = unsafe { GetStdHandle(kind) };
        if source.is_null() || source == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let current = unsafe { GetCurrentProcess() };
        let mut duplicated = std::ptr::null_mut();
        if unsafe {
            DuplicateHandle(
                current,
                source,
                current,
                &mut duplicated,
                0,
                TRUE,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(duplicated))
    }

    fn wide_nul(value: &OsStr) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = value.encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows process arguments cannot contain NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    fn append_quoted(argument: &OsStr, output: &mut Vec<u16>) -> io::Result<()> {
        let argument: Vec<u16> = argument.encode_wide().collect();
        if argument.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows process arguments cannot contain NUL",
            ));
        }
        let needs_quotes = argument.is_empty()
            || argument
                .iter()
                .any(|unit| matches!(*unit, 0x20 | 0x09 | 0x22));
        if !needs_quotes {
            output.extend(argument);
            return Ok(());
        }

        output.push(0x22);
        let mut backslashes = 0;
        for unit in argument {
            if unit == 0x5c {
                backslashes += 1;
            } else {
                output.extend(std::iter::repeat_n(
                    0x5c,
                    if unit == 0x22 {
                        backslashes * 2 + 1
                    } else {
                        backslashes
                    },
                ));
                backslashes = 0;
                output.push(unit);
            }
        }
        output.extend(std::iter::repeat_n(0x5c, backslashes * 2));
        output.push(0x22);
        Ok(())
    }

    fn command_line(executable: &OsStr, arguments: &[OsString]) -> io::Result<Vec<u16>> {
        let mut command_line = Vec::new();
        append_quoted(executable, &mut command_line)?;
        for argument in arguments {
            command_line.push(0x20);
            append_quoted(argument, &mut command_line)?;
        }
        command_line.push(0);
        Ok(command_line)
    }

    fn environment_variable(name: &str) -> io::Result<OsString> {
        let name = wide_nul(OsStr::new(name))?;
        let mut value = vec![0; 256];
        loop {
            let length = unsafe {
                GetEnvironmentVariableW(name.as_ptr(), value.as_mut_ptr(), value.len() as u32)
            };
            if length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "required Windows process environment variable is empty or undefined",
                ));
            }
            if (length as usize) < value.len() {
                value.truncate(length as usize);
                return Ok(OsString::from_wide(&value));
            }
            value.resize(length as usize + 1, 0);
        }
    }

    fn executable_extension(path: &Path) -> Option<&'static str> {
        let extension = path.extension()?;
        ["exe", "cmd", "bat"]
            .into_iter()
            .find(|candidate| extension.eq_ignore_ascii_case(candidate))
    }

    fn validate_executable_file(path: &Path) -> io::Result<()> {
        use std::os::windows::fs::MetadataExt;

        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || executable_extension(path).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows executable must be a regular, non-reparse .exe, .cmd, or .bat file",
            ));
        }
        Ok(())
    }

    fn search_path(
        path: &OsStr,
        executable: &OsStr,
        extension: Option<&str>,
    ) -> io::Result<PathBuf> {
        let path = wide_nul(path)?;
        let executable = wide_nul(executable)?;
        let extension = extension
            .map(|extension| wide_nul(OsStr::new(extension)))
            .transpose()?;
        let mut resolved = vec![0; 260];
        loop {
            let length = unsafe {
                SearchPathW(
                    path.as_ptr(),
                    executable.as_ptr(),
                    extension
                        .as_ref()
                        .map_or(std::ptr::null(), |value| value.as_ptr()),
                    resolved.len() as u32,
                    resolved.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            };
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            if (length as usize) < resolved.len() {
                resolved.truncate(length as usize);
                return Ok(PathBuf::from(OsString::from_wide(&resolved)));
            }
            resolved.resize(length as usize + 1, 0);
        }
    }

    fn pathext_order(value: &OsStr) -> io::Result<Vec<&'static str>> {
        let value = value.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows PATHEXT is not Unicode",
            )
        })?;
        let mut extensions = Vec::new();
        for value in value.split(';') {
            let extension = [".exe", ".cmd", ".bat"]
                .into_iter()
                .find(|candidate| value.eq_ignore_ascii_case(candidate));
            if let Some(extension) = extension
                && !extensions.contains(&extension)
            {
                extensions.push(extension);
            }
        }
        if extensions.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Windows PATHEXT contains no supported executable extension",
            ));
        }
        Ok(extensions)
    }

    fn resolve_executable(executable: &OsStr) -> io::Result<PathBuf> {
        let requested = Path::new(executable);
        if requested.is_absolute() {
            validate_executable_file(requested)?;
            return Ok(requested.to_owned());
        }
        let mut components = requested.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "relative Windows executable paths are not supported",
            ));
        }

        let path = environment_variable("PATH")?;
        let directories: Vec<_> = env::split_paths(&path)
            .filter(|directory| directory.is_absolute())
            .collect();
        if directories.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Windows PATH contains no absolute directory",
            ));
        }
        let extensions: Vec<_> = match executable_extension(requested) {
            Some(_) => vec![None],
            None if requested.extension().is_some() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Windows executable extension is not supported",
                ));
            }
            None => pathext_order(&environment_variable("PATHEXT")?)?
                .into_iter()
                .map(Some)
                .collect(),
        };
        let mut resolved = None;
        let mut last_error = None;
        for directory in directories {
            for extension in &extensions {
                match search_path(directory.as_os_str(), executable, *extension) {
                    Ok(path) => {
                        resolved = Some(path);
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if resolved.is_some() {
                break;
            }
        }
        let resolved = resolved.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Windows executable was not found")
            })
        })?;
        if !resolved.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SearchPathW returned a non-absolute path",
            ));
        }
        validate_executable_file(&resolved)?;
        Ok(resolved)
    }

    fn append_batch_argument(command_line: &mut Vec<u16>, argument: &OsStr) -> io::Result<()> {
        let argument = argument.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows batch arguments must be valid Unicode",
            )
        })?;
        if argument.contains(['\0', '\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows batch arguments cannot contain NUL, CR, or LF",
            ));
        }

        const UNQUOTED: &str = r"#$*+-./:?@\_";
        let quote = argument.is_empty()
            || argument.ends_with('\\')
            || argument.chars().any(|character| {
                (character.is_ascii()
                    && !(character.is_ascii_alphanumeric() || UNQUOTED.contains(character)))
                    || character.is_control()
            });
        if quote {
            command_line.push('"' as u16);
        }
        let mut backslashes = 0;
        for unit in argument.encode_utf16() {
            if unit == '\\' as u16 {
                backslashes += 1;
            } else {
                if unit == '"' as u16 {
                    command_line.extend(std::iter::repeat_n('\\' as u16, backslashes));
                    command_line.push('"' as u16);
                } else if unit == '%' as u16 {
                    command_line.extend("%%cd:~,".encode_utf16());
                }
                backslashes = 0;
            }
            command_line.push(unit);
        }
        if quote {
            command_line.extend(std::iter::repeat_n('\\' as u16, backslashes));
            command_line.push('"' as u16);
        }
        Ok(())
    }

    fn system_command_processor() -> io::Result<PathBuf> {
        let mut directory = vec![0; 260];
        loop {
            let length = unsafe {
                GetSystemDirectoryW(directory.as_mut_ptr(), directory.len().try_into().unwrap())
            };
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            if (length as usize) < directory.len() {
                directory.truncate(length as usize);
                let command = PathBuf::from(OsString::from_wide(&directory)).join("cmd.exe");
                validate_executable_file(&command)?;
                return Ok(command);
            }
            directory.resize(length as usize + 1, 0);
        }
    }

    fn batch_command_line(script: &Path, arguments: &[OsString]) -> io::Result<Vec<u16>> {
        // lpApplicationName pins the verified System32 executable; keep the command-line
        // prefix aligned with Rust std's batch parser contract.
        let mut command_line: Vec<u16> = "cmd.exe /e:ON /v:OFF /d /c \"".encode_utf16().collect();
        if matches!(
            script.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(
                    prefix.kind(),
                    Prefix::Verbatim(_) | Prefix::VerbatimUNC(_, _) | Prefix::VerbatimDisk(_)
                )
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows cmd.exe does not support verbatim batch script paths",
            ));
        }
        let script = script.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows batch script path must be valid Unicode",
            )
        })?;
        if script.contains(['\0', '\r', '\n', '"']) || script.ends_with('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows batch script path cannot contain NUL, CR, LF, quote, or end in backslash",
            ));
        }
        command_line.push('"' as u16);
        command_line.extend(script.encode_utf16());
        command_line.push('"' as u16);
        for argument in arguments {
            command_line.push(0x20);
            append_batch_argument(&mut command_line, argument)?;
        }
        command_line.push('"' as u16);
        if command_line.len() > 8_191 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows batch command line exceeds 8191 UTF-16 code units",
            ));
        }
        command_line.push(0);
        Ok(command_line)
    }

    pub fn resolve(invocation: &mut Invocation) -> io::Result<()> {
        invocation.executable = resolve_executable(&invocation.executable)?.into_os_string();
        let executable = Path::new(&invocation.executable);
        if matches!(executable_extension(executable), Some("cmd" | "bat")) {
            system_command_processor()?;
            batch_command_line(executable, &invocation.arguments)?;
        } else {
            command_line(&invocation.executable, &invocation.arguments)?;
        }
        Ok(())
    }

    fn create_child(invocation: Invocation) -> io::Result<ChildProcess> {
        let executable = Path::new(&invocation.executable);
        let is_batch = matches!(executable_extension(executable), Some("cmd" | "bat"));
        let command = if is_batch {
            system_command_processor()?
        } else {
            executable.to_owned()
        };
        let application = wide_nul(command.as_os_str())?;
        let mut command_line = if is_batch {
            batch_command_line(executable, &invocation.arguments)?
        } else {
            command_line(&invocation.executable, &invocation.arguments)?
        };
        let current_directory = wide_nul(Path::new(&invocation.cwd).as_os_str())?;
        let stdin = duplicate_standard_handle(STD_INPUT_HANDLE)?;
        let stdout = duplicate_standard_handle(STD_OUTPUT_HANDLE)?;
        let stderr = duplicate_standard_handle(STD_ERROR_HANDLE)?;
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: stdin.0,
            hStdOutput: stdout.0,
            hStdError: stderr.0,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                TRUE,
                CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED,
                std::ptr::null(),
                current_directory.as_ptr(),
                &startup,
                &mut process,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(ChildProcess {
            process: OwnedHandle(process.hProcess),
            thread: OwnedHandle(process.hThread),
            id: process.dwProcessId,
        })
    }

    fn stop_child(child: &ChildProcess) {
        unsafe {
            TerminateProcess(child.process.0, super::EX_OSERR as u32);
            WaitForSingleObject(child.process.0, INFINITE);
        }
    }

    pub fn prepare() -> io::Result<State> {
        ensure_console()?;
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
            unsafe { CloseHandle(job) };
            return Err(error);
        }
        JOB.store(job, Ordering::Release);
        if unsafe { SetConsoleCtrlHandler(Some(forward_console_event), TRUE) } == 0 {
            let error = io::Error::last_os_error();
            close_job();
            return Err(error);
        }
        Ok(State)
    }

    pub fn cancel(_state: State) {
        unsafe { SetConsoleCtrlHandler(Some(forward_console_event), FALSE) };
        close_job();
    }

    pub fn run(invocation: Invocation, state: State) -> io::Result<i32> {
        let child = match create_child(invocation) {
            Ok(child) => child,
            Err(error) => {
                cancel(state);
                return Err(error);
            }
        };
        let job = JOB.load(Ordering::Acquire);
        if job.is_null() || unsafe { AssignProcessToJobObject(job, child.process.0) } == 0 {
            let error = io::Error::last_os_error();
            stop_child(&child);
            cancel(state);
            return Err(error);
        }
        CHILD_PROCESS_GROUP.store(child.id, Ordering::Release);
        if unsafe { ResumeThread(child.thread.0) } == u32::MAX {
            let error = io::Error::last_os_error();
            close_job();
            unsafe { WaitForSingleObject(child.process.0, INFINITE) };
            unsafe { SetConsoleCtrlHandler(Some(forward_console_event), FALSE) };
            return Err(error);
        }
        if TERMINATION_REQUESTED.load(Ordering::Acquire) {
            begin_shutdown();
        }

        let waited = unsafe { WaitForSingleObject(child.process.0, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            let error = io::Error::last_os_error();
            close_job();
            unsafe { SetConsoleCtrlHandler(Some(forward_console_event), FALSE) };
            return Err(error);
        }
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(child.process.0, &mut exit_code) } == 0 {
            let error = io::Error::last_os_error();
            close_job();
            unsafe { SetConsoleCtrlHandler(Some(forward_console_event), FALSE) };
            return Err(error);
        }
        unsafe { SetConsoleCtrlHandler(Some(forward_console_event), FALSE) };
        if TERMINATION_REQUESTED.load(Ordering::Acquire) {
            begin_shutdown();
        }
        while TERMINATION_REQUESTED.load(Ordering::Acquire)
            && !SHUTDOWN_COMPLETE.load(Ordering::Acquire)
        {
            unsafe { Sleep(10) };
        }
        CHILD_PROCESS_GROUP.store(0, Ordering::Release);
        close_job();
        Ok(exit_code as i32)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_windows_arguments_without_shell_evaluation() {
            let cases = [
                ("plain", "plain"),
                ("two words", r#""two words""#),
                (r#"quote"inside"#, r#""quote\"inside""#),
                (r#"space slash\"#, r#""space slash\\""#),
            ];
            for (input, expected) in cases {
                let mut output = Vec::new();
                append_quoted(OsStr::new(input), &mut output).unwrap();
                assert_eq!(String::from_utf16(&output).unwrap(), expected);
            }
        }

        #[test]
        fn escapes_batch_arguments_with_the_rust_standard_library_rules() {
            let cases = [
                ("", "\"\""),
                ("test", "test"),
                ("한글", "한글"),
                ("%PATH%", r#""%%cd:~,%PATH%%cd:~,%""#),
                ("hello world", r#""hello world""#),
                (r#"quote"inside"#, r#""quote""inside""#),
                (r#"hello world\"#, r#""hello world\\""#),
                ("&echo", r#""&echo""#),
            ];
            for (input, expected) in cases {
                let mut actual = Vec::new();
                append_batch_argument(&mut actual, OsStr::new(input)).unwrap();
                assert_eq!(String::from_utf16(&actual).unwrap(), expected);
            }
        }

        #[test]
        fn rejects_unrepresentable_or_oversized_batch_arguments() {
            for input in ["nul\0value", "line\nvalue", "line\rvalue"] {
                assert!(append_batch_argument(&mut Vec::new(), OsStr::new(input)).is_err());
            }
            let invalid_unicode = OsString::from_wide(&[0xd800]);
            assert!(append_batch_argument(&mut Vec::new(), &invalid_unicode).is_err());
            let arguments = vec![OsString::from("x".repeat(8_192))];
            assert!(batch_command_line(Path::new(r"C:\fixture.cmd"), &arguments).is_err());
        }

        #[test]
        fn uses_the_cmd_exe_argv_zero_expected_by_the_batch_parser() {
            let command_line = batch_command_line(Path::new(r"C:\fixture.cmd"), &[]).unwrap();
            assert_eq!(
                String::from_utf16(&command_line[..command_line.len() - 1]).unwrap(),
                r#"cmd.exe /e:ON /v:OFF /d /c ""C:\fixture.cmd"""#
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use std::ffi::OsStr;

    #[cfg(windows)]
    #[test]
    fn opened_parent_handle_confines_windows_path_replacements() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let original = root.path().join("original");
        let marker_directory = live.join("markers");
        fs::create_dir_all(&marker_directory).unwrap();
        let marker_path = marker_directory.join("session.json");
        let moved_marker = marker_directory.join("moved.json");
        let (parent, _) = open_private_parent(&marker_path, "session file").unwrap();
        let temporary_name = OsStr::new("session.tmp");
        let temporary_path = marker_directory.join(temporary_name);
        let mut marker = open_windows_path(&temporary_path, true).unwrap();
        assert!(windows_parent_matches(&parent, &marker_directory).unwrap());
        marker.write_all(b"owned").unwrap();
        rename_windows_at(&marker, &parent, &marker_path).unwrap();
        let published = open_windows_probe(&marker_path).unwrap();
        assert!(windows_parent_matches(&parent, &marker_directory).unwrap());
        assert!(same_file(&marker, &published).unwrap());
        drop(published);

        assert!(fs::rename(&marker_path, &moved_marker).is_err());
        assert!(fs::remove_file(&marker_path).is_err());
        assert!(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&marker_path)
                .is_err()
        );
        let published = open_windows_probe(&marker_path).unwrap();
        assert!(same_file(&marker, &published).unwrap());
        assert_eq!(fs::read(&marker_path).unwrap(), b"owned");
        drop(published);

        assert!(fs::rename(&marker_directory, live.join("replacement")).is_err());
        remove_windows_file(&marker).unwrap();
        drop(marker);
        assert!(!marker_path.exists());

        let final_directory = if fs::rename(&live, &original).is_ok() {
            let replacement = live.join("markers");
            fs::create_dir_all(&replacement).unwrap();
            assert!(!windows_parent_matches(&parent, &replacement).unwrap());
            let misplaced = open_windows_path(&replacement.join("misplaced.tmp"), true).unwrap();
            remove_windows_file(&misplaced).unwrap();
            drop(misplaced);
            assert!(!replacement.join("misplaced.tmp").exists());
            original.join("markers")
        } else {
            assert!(windows_parent_matches(&parent, &marker_directory).unwrap());
            marker_directory
        };
        assert!(!final_directory.join("session.json").exists());
    }

    #[cfg(windows)]
    #[test]
    fn control_identity_handoff_rejects_replacement_and_blocks_delete_sharing() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let directory = tempfile::tempdir().unwrap();
        let control_path = directory.path().join("session.control");
        let moved_path = directory.path().join("moved.control");
        let mut host_options = OpenOptions::new();
        host_options
            .create_new(true)
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        let mut host = host_options.open(&control_path).unwrap();
        host.write_all(&[0]).unwrap();
        host.sync_all().unwrap();
        host.lock_shared().unwrap();
        let expected = file_identity(&host).unwrap();

        fs::rename(&control_path, &moved_path).unwrap();
        fs::write(&control_path, [0]).unwrap();
        let replacement = open_windows_probe(&control_path).unwrap();
        assert_ne!(file_identity(&replacement).unwrap(), expected);
        assert!(ControlFileGuard::open(&control_path, &expected).is_err());
        assert_eq!(fs::read(&control_path).unwrap(), [0]);
        drop(replacement);
        fs::remove_file(&control_path).unwrap();
        fs::rename(&moved_path, &control_path).unwrap();

        let guard = ControlFileGuard::open(&control_path, &expected).unwrap();
        host.unlock().unwrap();
        drop(host);
        assert!(fs::rename(&control_path, &moved_path).is_err());
        assert!(fs::remove_file(&control_path).is_err());
        let probe = open_windows_probe(&control_path).unwrap();
        assert_eq!(file_identity(&probe).unwrap(), expected);
        assert_eq!(fs::read(&control_path).unwrap(), [0]);
        drop(probe);
        drop(guard);
        assert!(!control_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn opened_parent_and_inode_checks_confine_path_replacements() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let original = root.path().join("original");
        fs::create_dir(&live).unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o700)).unwrap();
        let marker_path = live.join("session.json");
        let (parent, marker_name) = open_private_parent(&marker_path, "session file").unwrap();
        let temporary_name = OsStr::new("session.tmp");
        let mut marker = open_at(
            &parent,
            temporary_name,
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
        .unwrap();
        marker.write_all(b"owned").unwrap();

        fs::rename(&live, &original).unwrap();
        fs::create_dir(&live).unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o700)).unwrap();
        link_at(&parent, temporary_name, &marker_name).unwrap();
        assert_eq!(fs::read(original.join(&marker_name)).unwrap(), b"owned");
        assert!(!live.join(&marker_name).exists());
        assert!(same_file_at(&marker, &parent, &marker_name).unwrap());

        fs::remove_file(original.join(&marker_name)).unwrap();
        fs::write(original.join(&marker_name), b"replacement").unwrap();
        assert!(!same_file_at(&marker, &parent, &marker_name).unwrap());
        assert!(!remove_at_if_same(&parent, &marker_name, &marker).unwrap());
        assert_eq!(
            fs::read(original.join(&marker_name)).unwrap(),
            b"replacement"
        );
        assert!(remove_at_if_same(&parent, temporary_name, &marker).unwrap());
    }

    #[test]
    fn accepts_only_the_documented_shape() {
        let directory = std::env::temp_dir().canonicalize().unwrap();
        let unmanaged = vec![
            OsString::from("--cwd"),
            directory.as_os_str().to_owned(),
            OsString::from("--"),
            OsString::from("tool"),
        ];
        assert!(parse(unmanaged.clone()).is_some());

        let managed = |action_id: &str, worktree: &Path| {
            vec![
                OsString::from("--session-id"),
                OsString::from("018f47a2-bbab-7de0-8000-0123456789ab"),
                OsString::from("--session-file"),
                directory.join("session.json").into_os_string(),
                OsString::from("--control-file"),
                directory.join("session.control").into_os_string(),
                OsString::from("--control-identity"),
                OsString::from("fixture-identity"),
                OsString::from("--action-id"),
                OsString::from(action_id),
                OsString::from("--worktree"),
                worktree.as_os_str().to_owned(),
                OsString::from("--cwd"),
                directory.as_os_str().to_owned(),
                OsString::from("--"),
                OsString::from("tool"),
            ]
        };
        assert!(parse(managed("018f47a2-bbab-7de0-8000-0123456789ac", &directory)).is_some());
        assert!(parse(managed("00000000-0000-0000-0000-000000000000", &directory)).is_none());
        assert!(
            parse(managed(
                "018f47a2-bbab-7de0-8000-0123456789ac",
                Path::new("relative")
            ))
            .is_none()
        );

        let mut invalid_cwd = unmanaged.clone();
        invalid_cwd[1] = OsString::from("relative");
        assert!(parse(invalid_cwd).is_none());
        assert!(parse(unmanaged[..3].to_vec()).is_none());
    }
}
