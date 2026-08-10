use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::cleanup::{
    ArchiveInspection, ArchiveRequest, CleanupArchiveEntry, CodexSessionEvidence,
    CodexSessionSnapshot, RemovalAuthorization,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupArchiveResult {
    pub archived_file_count: usize,
    pub archived_byte_count: u64,
    pub archive_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedCleanupFile {
    pub relative_path: PathBuf,
    pub quarantine_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum CleanupArchiveError {
    #[error("The canonical Codex home directory is invalid: {0:?}.")]
    InvalidHome(PathBuf),
    #[error("The archive destination is invalid: {0:?}.")]
    InvalidDestination(PathBuf),
    #[error("Refusing to replace an existing cleanup archive: {0:?}.")]
    DestinationExists(PathBuf),
    #[error("Refusing an unsafe cleanup path: {0:?}.")]
    UnsafePath(PathBuf),
    #[error("Cleanup source is missing, changed, or not a regular file: {0:?}.")]
    SourceChanged(PathBuf),
    #[error("Duplicate cleanup source: {0:?}.")]
    DuplicateSource(PathBuf),
    #[error("The cleanup archive bytes do not exactly match the authorized files.")]
    ArchiveMismatch,
    #[error("A cleanup path cannot be represented in a ZIP archive: {0:?}.")]
    UnsupportedPath(PathBuf),
    #[error("Cleanup sources could not be restored after an interrupted removal.")]
    RollbackFailed,
    #[error(
        "Cleanup was only partially removed; the verified archive is {archive_path:?}, removed files are {removed_relative_paths:?}, and retained quarantine files are {retained_files:?}."
    )]
    PartialRemoval {
        archive_path: PathBuf,
        removed_relative_paths: Vec<PathBuf>,
        retained_files: Vec<RetainedCleanupFile>,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

struct ValidatedSource {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
    entry: CleanupArchiveEntry,
}

struct QuarantinedSource {
    relative_path: PathBuf,
    original_path: PathBuf,
    quarantine_path: PathBuf,
    quarantine_directory: PathBuf,
    file: File,
    identity: FileIdentity,
    entry: CleanupArchiveEntry,
}

struct PrunableDirectory {
    relative_path: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovalStep {
    BeforeQuarantine,
    BeforeDelete,
}

/// Reads a cleanup source through a non-symlink handle and returns the snapshot needed by the
/// cleanup plan. The caller still supplies cache and sync evidence separately.
pub fn cleanup_source_snapshot(
    canonical_codex_home: &Path,
    relative_path: &Path,
) -> Result<CodexSessionSnapshot, CleanupArchiveError> {
    validate_home(canonical_codex_home)?;
    Ok(open_source(canonical_codex_home, relative_path)?.0)
}

/// Creates and verifies a new archive without replacing any existing destination.
/// The caller must obtain `ArchiveRequest` from `CodexSessionCleanupPlan::archive_request`.
pub fn create_cleanup_archive(
    canonical_codex_home: &Path,
    destination: &Path,
    request: &ArchiveRequest,
    current: &[CodexSessionEvidence],
) -> Result<ArchiveInspection, CleanupArchiveError> {
    validate_home(canonical_codex_home)?;
    let sources = validate_sources(canonical_codex_home, request.entries(), current)?;
    let (destination, parent) = prepare_destination(destination, &sources)?;
    let temp = create_temp_path(&parent)?;

    let result = (|| {
        write_archive(&temp, request.entries(), &sources)?;
        let temp_inspection = inspect_exact(&temp, request.entries())?;
        publish_new(&temp, &destination)?;
        sync_directory(&parent)?;
        let destination_inspection = inspect_exact(&destination, request.entries())?;
        if destination_inspection != temp_inspection {
            return Err(CleanupArchiveError::ArchiveMismatch);
        }
        Ok(destination_inspection)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Removes only files bound to the authorization's entry and whole-archive digests.
pub fn remove_cleanup_sources(
    canonical_codex_home: &Path,
    archive_path: &Path,
    current: &[CodexSessionEvidence],
    authorization: &RemovalAuthorization,
) -> Result<CleanupArchiveResult, CleanupArchiveError> {
    remove_cleanup_sources_with_hook(
        canonical_codex_home,
        archive_path,
        current,
        authorization,
        |_, _, _| Ok(()),
    )
}

fn remove_cleanup_sources_with_hook(
    canonical_codex_home: &Path,
    archive_path: &Path,
    current: &[CodexSessionEvidence],
    authorization: &RemovalAuthorization,
    mut hook: impl FnMut(RemovalStep, usize, &Path) -> io::Result<()>,
) -> Result<CleanupArchiveResult, CleanupArchiveError> {
    validate_home(canonical_codex_home)?;
    let sources = validate_sources(canonical_codex_home, authorization.entries(), current)?;
    inspect_authorized(archive_path, authorization)?;
    let prunable_directories = collect_prunable_directories(canonical_codex_home, &sources);

    let mut quarantined = VecDeque::new();
    for (index, source) in sources.into_iter().enumerate() {
        if let Err(error) = hook(RemovalStep::BeforeQuarantine, index, &source.path) {
            rollback_all(&mut quarantined)?;
            return Err(error.into());
        }
        match quarantine(source) {
            Ok(source) => quarantined.push_back(source),
            Err(error) => {
                rollback_all(&mut quarantined)?;
                return Err(error);
            }
        }
    }

    if let Err(error) = inspect_authorized(archive_path, authorization) {
        rollback_all(&mut quarantined)?;
        return Err(error);
    }

    let mut removed = Vec::new();
    while let Some(source) = quarantined.front() {
        let index = removed.len();
        let deletion = hook(RemovalStep::BeforeDelete, index, &source.quarantine_path)
            .map_err(CleanupArchiveError::from)
            .and_then(|_| inspect_authorized(archive_path, authorization))
            .and_then(|_| delete_quarantined(source));
        if let Err(error) = deletion {
            if removed.is_empty() {
                rollback_all(&mut quarantined)?;
                return Err(error);
            }
            return Err(CleanupArchiveError::PartialRemoval {
                archive_path: archive_path.to_path_buf(),
                removed_relative_paths: removed,
                retained_files: quarantined
                    .iter()
                    .map(|source| RetainedCleanupFile {
                        relative_path: source.relative_path.clone(),
                        quarantine_path: source.quarantine_path.clone(),
                    })
                    .collect(),
            });
        }

        let source = quarantined.pop_front().expect("front entry exists");
        removed.push(source.relative_path.clone());
        drop(source.file);
        let _ = fs::remove_dir(source.quarantine_directory);
    }

    prune_empty_source_directories(canonical_codex_home, &prunable_directories);

    Ok(CleanupArchiveResult {
        archived_file_count: authorization.entries().len(),
        archived_byte_count: authorization.byte_count(),
        archive_path: archive_path.to_path_buf(),
    })
}

fn collect_prunable_directories(
    home: &Path,
    sources: &[ValidatedSource],
) -> Vec<PrunableDirectory> {
    let mut relative_paths = BTreeSet::new();
    for source in sources {
        let relative = source.entry.relative_path();
        let Some(root) = relative
            .components()
            .next()
            .map(|component| component.as_os_str())
        else {
            continue;
        };
        let root = Path::new(root);
        let mut parent = relative.parent();
        while let Some(path) = parent.filter(|path| *path != root) {
            relative_paths.insert(path.to_path_buf());
            parent = path.parent();
        }
    }

    let mut directories: Vec<_> = relative_paths
        .into_iter()
        .filter_map(|relative_path| {
            direct_source_directory_identity(home, &relative_path).map(|identity| {
                PrunableDirectory {
                    relative_path,
                    identity,
                }
            })
        })
        .collect();
    directories.sort_unstable_by(|left, right| {
        right
            .relative_path
            .components()
            .count()
            .cmp(&left.relative_path.components().count())
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    directories
}

fn prune_empty_source_directories(home: &Path, directories: &[PrunableDirectory]) {
    for directory in directories {
        if direct_source_directory_identity(home, &directory.relative_path)
            == Some(directory.identity)
        {
            let _ = fs::remove_dir(home.join(&directory.relative_path));
        }
    }
}

fn direct_source_directory_identity(home: &Path, relative: &Path) -> Option<FileIdentity> {
    let mut components = relative.components();
    let root = match components.next()? {
        Component::Normal(root)
            if root == std::ffi::OsStr::new("sessions")
                || root == std::ffi::OsStr::new("archived_sessions") =>
        {
            home.join(root)
        }
        _ => return None,
    };
    components.next()?;

    let path = home.join(relative);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return None;
    }
    if fs::canonicalize(&root).ok().as_deref() != Some(root.as_path())
        || fs::canonicalize(&path).ok().as_deref() != Some(path.as_path())
        || !path.starts_with(&root)
    {
        return None;
    }
    open_directory_identity(&path).ok()
}

fn validate_home(home: &Path) -> Result<(), CleanupArchiveError> {
    if !home.is_absolute() || fs::canonicalize(home).ok().as_deref() != Some(home) {
        return Err(CleanupArchiveError::InvalidHome(home.to_path_buf()));
    }
    Ok(())
}

fn prepare_destination(
    destination: &Path,
    sources: &[ValidatedSource],
) -> Result<(PathBuf, PathBuf), CleanupArchiveError> {
    if !destination.is_absolute() {
        return Err(CleanupArchiveError::InvalidDestination(
            destination.to_path_buf(),
        ));
    }
    let Some(parent) = destination.parent() else {
        return Err(CleanupArchiveError::InvalidDestination(
            destination.to_path_buf(),
        ));
    };
    fs::create_dir_all(parent)?;
    let canonical_parent = fs::canonicalize(parent)?;
    let Some(file_name) = destination.file_name() else {
        return Err(CleanupArchiveError::InvalidDestination(
            destination.to_path_buf(),
        ));
    };
    let canonical_destination = canonical_parent.join(file_name);
    if canonical_destination != destination
        || sources
            .iter()
            .any(|source| destination.starts_with(&source.path))
    {
        return Err(CleanupArchiveError::InvalidDestination(
            destination.to_path_buf(),
        ));
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => Err(CleanupArchiveError::DestinationExists(
            destination.to_path_buf(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok((canonical_destination, canonical_parent))
        }
        Err(error) => Err(error.into()),
    }
}

fn create_temp_path(parent: &Path) -> Result<PathBuf, CleanupArchiveError> {
    for attempt in 0..100 {
        let path = parent.join(format!(
            ".tokenmeter-cleanup.{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                set_private_permissions(&file)?;
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a cleanup archive temporary file",
    )
    .into())
}

fn publish_new(temp: &Path, destination: &Path) -> Result<(), CleanupArchiveError> {
    match fs::hard_link(temp, destination) {
        Ok(()) => {
            fs::remove_file(temp)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(
            CleanupArchiveError::DestinationExists(destination.to_path_buf()),
        ),
        Err(error) => Err(error.into()),
    }
}

fn validate_sources(
    home: &Path,
    expected: &[CleanupArchiveEntry],
    current: &[CodexSessionEvidence],
) -> Result<Vec<ValidatedSource>, CleanupArchiveError> {
    let mut evidence_by_path = BTreeMap::new();
    for evidence in current {
        if evidence_by_path
            .insert(evidence.snapshot.relative_path.clone(), &evidence.snapshot)
            .is_some()
        {
            return Err(CleanupArchiveError::DuplicateSource(
                evidence.snapshot.relative_path.clone(),
            ));
        }
    }

    let mut seen = BTreeSet::new();
    expected
        .iter()
        .map(|entry| {
            let relative = entry.relative_path();
            normalize_relative(relative)?;
            if !seen.insert(relative) {
                return Err(CleanupArchiveError::DuplicateSource(relative.to_path_buf()));
            }
            let expected_snapshot = evidence_by_path
                .get(relative)
                .ok_or_else(|| CleanupArchiveError::SourceChanged(relative.to_path_buf()))?;
            let (snapshot, file, identity, path) = open_source(home, relative)?;
            if &snapshot != *expected_snapshot
                || snapshot.size != entry.size()
                || snapshot.content_sha256 != entry.content_sha256()
            {
                return Err(CleanupArchiveError::SourceChanged(relative.to_path_buf()));
            }
            Ok(ValidatedSource {
                path,
                file,
                identity,
                entry: entry.clone(),
            })
        })
        .collect()
}

fn open_source(
    home: &Path,
    relative: &Path,
) -> Result<(CodexSessionSnapshot, File, FileIdentity, PathBuf), CleanupArchiveError> {
    normalize_relative(relative)?;
    let path = home.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| CleanupArchiveError::SourceChanged(relative.to_path_buf()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CleanupArchiveError::SourceChanged(relative.to_path_buf()));
    }
    let canonical = fs::canonicalize(&path)
        .map_err(|_| CleanupArchiveError::SourceChanged(relative.to_path_buf()))?;
    if canonical != path || !canonical.starts_with(home) {
        return Err(CleanupArchiveError::UnsafePath(relative.to_path_buf()));
    }

    let file = open_cleanup_file(&path)?;
    let handle_metadata = file.metadata()?;
    let path_handle = open_cleanup_file(&path)?;
    if !handle_metadata.is_file()
        || file_identity(&file)? != file_identity(&path_handle)?
        || metadata.len() != handle_metadata.len()
    {
        return Err(CleanupArchiveError::SourceChanged(relative.to_path_buf()));
    }
    let identity = file_identity(&file)?;
    let (size, content_sha256) = digest_file(&file)?;
    let snapshot = CodexSessionSnapshot {
        relative_path: relative.to_path_buf(),
        size,
        modified_at_unix_nanos: system_time_unix_nanos(handle_metadata.modified()?)?,
        content_sha256,
    };
    Ok((snapshot, file, identity, path))
}

fn system_time_unix_nanos(time: std::time::SystemTime) -> Result<i64, CleanupArchiveError> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "file modification time is out of range",
            )
            .into()
        }),
        Err(error) => i64::try_from(error.duration().as_nanos())
            .map(|value| -value)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file modification time is out of range",
                )
                .into()
            }),
    }
}

fn normalize_relative(path: &Path) -> Result<(), CleanupArchiveError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(CleanupArchiveError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn zip_name(path: &Path) -> Result<String, CleanupArchiveError> {
    normalize_relative(path)?;
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| CleanupArchiveError::UnsupportedPath(path.to_path_buf())),
            _ => unreachable!(),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

fn write_archive(
    path: &Path,
    entries: &[CleanupArchiveEntry],
    sources: &[ValidatedSource],
) -> Result<(), CleanupArchiveError> {
    let file = OpenOptions::new().write(true).truncate(true).open(path)?;
    set_private_permissions(&file)?;
    let mut archive = ZipWriter::new(file);
    for (entry, source) in entries.iter().zip(sources) {
        if entry != &source.entry {
            return Err(CleanupArchiveError::ArchiveMismatch);
        }
        archive.start_file(
            zip_name(entry.relative_path())?,
            SimpleFileOptions::default(),
        )?;
        let mut reader = source.file.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        io::copy(&mut reader, &mut archive)?;
    }
    let file = archive.finish()?;
    file.sync_all()?;
    Ok(())
}

fn inspect_authorized(
    archive_path: &Path,
    authorization: &RemovalAuthorization,
) -> Result<(), CleanupArchiveError> {
    let inspection = inspect_exact(archive_path, authorization.entries())?;
    if inspection.archive_sha256() != authorization.archive_sha256() {
        return Err(CleanupArchiveError::ArchiveMismatch);
    }
    Ok(())
}

fn inspect_exact(
    archive_path: &Path,
    expected: &[CleanupArchiveEntry],
) -> Result<ArchiveInspection, CleanupArchiveError> {
    if fs::canonicalize(archive_path).ok().as_deref() != Some(archive_path) {
        return Err(CleanupArchiveError::ArchiveMismatch);
    }
    let file = open_read_file(archive_path)?;
    if !file.metadata()?.is_file() {
        return Err(CleanupArchiveError::ArchiveMismatch);
    }
    let (_, archive_sha256) = digest_file(&file)?;
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut archive = ZipArchive::new(reader)?;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.is_file() || entry.name().contains('\\') {
            return Err(CleanupArchiveError::ArchiveMismatch);
        }
        let path = entry
            .enclosed_name()
            .ok_or(CleanupArchiveError::ArchiveMismatch)?;
        normalize_relative(&path).map_err(|_| CleanupArchiveError::ArchiveMismatch)?;
        let (size, content_sha256) = digest_reader(&mut entry)?;
        entries.push(CleanupArchiveEntry::verified(path, size, content_sha256));
    }

    let mut actual = entries.clone();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    if actual != expected {
        return Err(CleanupArchiveError::ArchiveMismatch);
    }
    Ok(ArchiveInspection::verified(entries, archive_sha256))
}

fn digest_file(file: &File) -> Result<(u64, String), CleanupArchiveError> {
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    digest_reader(&mut reader)
}

fn digest_reader(reader: &mut impl Read) -> Result<(u64, String), CleanupArchiveError> {
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file is too large"))?;
    }
    let digest = digest.finalize();
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hex.push(DIGITS[usize::from(byte >> 4)] as char);
        hex.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok((size, hex))
}

fn quarantine(source: ValidatedSource) -> Result<QuarantinedSource, CleanupArchiveError> {
    let parent = source
        .path
        .parent()
        .ok_or_else(|| CleanupArchiveError::UnsafePath(source.path.clone()))?;
    let directory = tempfile::Builder::new()
        .prefix(".tokenmeter-cleanup-")
        .tempdir_in(parent)?
        .keep();
    let quarantine_path = directory.join("source.jsonl");
    if let Err(error) = fs::rename(&source.path, &quarantine_path) {
        let _ = fs::remove_dir(&directory);
        return Err(error.into());
    }
    let quarantined = QuarantinedSource {
        relative_path: source.entry.relative_path().to_path_buf(),
        original_path: source.path,
        quarantine_path,
        quarantine_directory: directory,
        file: source.file,
        identity: source.identity,
        entry: source.entry,
    };
    if let Err(error) = verify_quarantined(&quarantined) {
        if !quarantined.original_path.exists() {
            let _ = fs::rename(&quarantined.quarantine_path, &quarantined.original_path);
        }
        let _ = fs::remove_dir(&quarantined.quarantine_directory);
        return Err(error);
    }
    Ok(quarantined)
}

fn verify_quarantined(source: &QuarantinedSource) -> Result<(), CleanupArchiveError> {
    let metadata = fs::symlink_metadata(&source.quarantine_path)
        .map_err(|_| CleanupArchiveError::SourceChanged(source.relative_path.clone()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CleanupArchiveError::SourceChanged(
            source.relative_path.clone(),
        ));
    }
    let file = open_cleanup_file(&source.quarantine_path)?;
    if file_identity(&file)? != source.identity {
        return Err(CleanupArchiveError::SourceChanged(
            source.relative_path.clone(),
        ));
    }
    let (size, digest) = digest_file(&file)?;
    if size != source.entry.size() || digest != source.entry.content_sha256() {
        return Err(CleanupArchiveError::SourceChanged(
            source.relative_path.clone(),
        ));
    }
    Ok(())
}

fn rollback_all(quarantined: &mut VecDeque<QuarantinedSource>) -> Result<(), CleanupArchiveError> {
    let mut restored = true;
    while let Some(source) = quarantined.pop_back() {
        if source.original_path.exists()
            || verify_quarantined(&source).is_err()
            || fs::rename(&source.quarantine_path, &source.original_path).is_err()
        {
            restored = false;
        }
        drop(source.file);
        let _ = fs::remove_dir(source.quarantine_directory);
    }
    if restored {
        Ok(())
    } else {
        Err(CleanupArchiveError::RollbackFailed)
    }
}

fn delete_quarantined(source: &QuarantinedSource) -> Result<(), CleanupArchiveError> {
    verify_quarantined(source)?;
    delete_exact_file(&source.file, &source.quarantine_path)?;
    Ok(())
}

#[cfg(unix)]
fn open_cleanup_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_cleanup_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        },
    };
    OpenOptions::new()
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(unix)]
fn open_read_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_read_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(unix)]
fn open_directory_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup ancestor is not a directory",
        ));
    }
    file_identity(&file)
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file owns a valid handle and `info` is writable for the duration of the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        first: u64::from(info.dwVolumeSerialNumber),
        second: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(windows)]
fn open_directory_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file owns a valid handle and `info` is writable for the duration of the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;
    if info.dwFileAttributes & DIRECTORY == 0 || info.dwFileAttributes & REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cleanup ancestor is not a direct directory",
        ));
    }
    Ok(FileIdentity {
        first: u64::from(info.dwVolumeSerialNumber),
        second: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(unix)]
fn delete_exact_file(_: &File, path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn delete_exact_file(file: &File, _: &Path) -> io::Result<()> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: the handle was opened with DELETE access and the typed buffer has the exact size.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Windows has no supported std API for flushing a containing directory;
    // the archive file itself is flushed before publication.
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn set_private_permissions(_: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeSet, io::Write};

    use chrono::{TimeDelta, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::cleanup::{CodexSessionCleanupPlan, plan_codex_session_cleanup};

    fn setup() -> (TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("custom-codex-home");
        let archive_directory = temp.path().join("archives");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&archive_directory).unwrap();
        let home = fs::canonicalize(home).unwrap();
        let archive = fs::canonicalize(archive_directory)
            .unwrap()
            .join("cleanup.zip");
        (temp, home, archive)
    }

    fn evidence(home: &Path, relative: &Path) -> CodexSessionEvidence {
        CodexSessionEvidence {
            snapshot: cleanup_source_snapshot(home, relative).unwrap(),
            cached_event_keys: Some(BTreeSet::from(["event".to_owned()])),
        }
    }

    fn plan(evidence: &[CodexSessionEvidence]) -> CodexSessionCleanupPlan {
        plan_codex_session_cleanup(
            evidence,
            &BTreeSet::from(["event".to_owned()]),
            1,
            Utc::now() + TimeDelta::days(2),
        )
    }

    fn write_source(home: &Path, relative: &Path, contents: &[u8]) {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn authorized(
        home: &Path,
        archive: &Path,
        current: &[CodexSessionEvidence],
    ) -> (CodexSessionCleanupPlan, RemovalAuthorization) {
        let plan = plan(current);
        let synced = BTreeSet::from(["event".to_owned()]);
        let request = plan.archive_request(current, &synced).unwrap();
        let inspection = create_cleanup_archive(home, archive, &request, current).unwrap();
        let authorization = plan
            .authorize_removal(current, &synced, &inspection)
            .unwrap();
        (plan, authorization)
    }

    fn write_unchecked_archive(path: &Path, relative: &Path, contents: &[u8]) {
        let file = File::create(path).unwrap();
        let mut archive = ZipWriter::new(file);
        archive
            .start_file(zip_name(relative).unwrap(), SimpleFileOptions::default())
            .unwrap();
        archive.write_all(contents).unwrap();
        archive.finish().unwrap().sync_all().unwrap();
    }

    #[test]
    fn archives_then_removes_only_the_authorized_file() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/old.jsonl");
        write_source(&home, relative, b"old session");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);

        let result = remove_cleanup_sources(&home, &archive, &current, &authorization).unwrap();

        assert_eq!(result.archived_file_count, 1);
        assert_eq!(result.archived_byte_count, b"old session".len() as u64);
        assert_eq!(result.archive_path, archive);
        assert!(!home.join(relative).exists());
        assert!(result.archive_path.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn windows_archive_publication_uses_the_supported_file_flush_boundary() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/old.jsonl");
        write_source(&home, relative, b"old session");
        let current = vec![evidence(&home, relative)];
        let plan = plan(&current);
        let request = plan
            .archive_request(&current, &BTreeSet::from(["event".to_owned()]))
            .unwrap();

        create_cleanup_archive(&home, &archive, &request, &current).unwrap();

        assert!(archive.is_file());
        assert!(home.join(relative).is_file());
    }

    #[test]
    fn successful_cleanup_prunes_only_empty_source_ancestors() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/08/old.jsonl");
        write_source(&home, relative, b"old session");
        fs::write(home.join("sessions/2026/keep.txt"), b"keep").unwrap();
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);

        remove_cleanup_sources(&home, &archive, &current, &authorization).unwrap();

        assert!(!home.join("sessions/2026/08").exists());
        assert_eq!(
            fs::read(home.join("sessions/2026/keep.txt")).unwrap(),
            b"keep"
        );
        assert!(home.join("sessions").is_dir());
        assert!(home.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn prune_keeps_replaced_or_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/08/old.jsonl");
        write_source(&home, relative, b"old session");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);
        let sources = validate_sources(&home, authorization.entries(), &current).unwrap();
        let directories = collect_prunable_directories(&home, &sources);
        let ancestor = home.join("sessions/2026/08");
        let original = home.join("sessions/2026/original");
        fs::rename(&ancestor, &original).unwrap();
        fs::create_dir(&ancestor).unwrap();

        prune_empty_source_directories(&home, &directories);
        assert!(ancestor.is_dir());

        fs::remove_dir(&ancestor).unwrap();
        symlink(&original, &ancestor).unwrap();
        prune_empty_source_directories(&home, &directories);
        assert!(
            fs::symlink_metadata(&ancestor)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(original.join("old.jsonl").is_file());
    }

    #[test]
    fn changed_source_is_never_removed() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/changed.jsonl");
        write_source(&home, relative, b"before");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);
        OpenOptions::new()
            .append(true)
            .open(home.join(relative))
            .unwrap()
            .write_all(b" changed")
            .unwrap();

        assert!(matches!(
            remove_cleanup_sources(&home, &archive, &current, &authorization),
            Err(CleanupArchiveError::SourceChanged(_))
        ));
        assert!(home.join(relative).is_file());
    }

    #[test]
    fn same_name_wrong_content_archive_never_removes_source() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/old.jsonl");
        write_source(&home, relative, b"good");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);
        fs::remove_file(&archive).unwrap();
        write_unchecked_archive(&archive, relative, b"evil");

        assert!(matches!(
            remove_cleanup_sources(&home, &archive, &current, &authorization),
            Err(CleanupArchiveError::ArchiveMismatch)
        ));
        assert_eq!(fs::read(home.join(relative)).unwrap(), b"good");
    }

    #[test]
    fn archive_replacement_race_restores_quarantined_source() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/old.jsonl");
        write_source(&home, relative, b"good");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);
        let raced = Cell::new(false);

        let result = remove_cleanup_sources_with_hook(
            &home,
            &archive,
            &current,
            &authorization,
            |step, _, _| {
                if step == RemovalStep::BeforeDelete && !raced.replace(true) {
                    fs::remove_file(&archive)?;
                    write_unchecked_archive(&archive, relative, b"evil");
                }
                Ok(())
            },
        );

        assert!(matches!(result, Err(CleanupArchiveError::ArchiveMismatch)));
        assert_eq!(fs::read(home.join(relative)).unwrap(), b"good");
    }

    #[test]
    fn destination_collision_never_overwrites_any_file() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/old.jsonl");
        write_source(&home, relative, b"source");
        fs::write(&archive, b"existing archive").unwrap();
        let current = vec![evidence(&home, relative)];
        let plan = plan(&current);
        let request = plan
            .archive_request(&current, &BTreeSet::from(["event".to_owned()]))
            .unwrap();

        assert!(matches!(
            create_cleanup_archive(&home, &archive, &request, &current),
            Err(CleanupArchiveError::DestinationExists(_))
        ));
        assert_eq!(fs::read(&archive).unwrap(), b"existing archive");
        assert_eq!(fs::read(home.join(relative)).unwrap(), b"source");
    }

    #[test]
    fn source_cannot_be_the_archive_destination() {
        let (_temp, home, _archive) = setup();
        let relative = Path::new("sessions/2026/old.jsonl");
        write_source(&home, relative, b"source");
        let current = vec![evidence(&home, relative)];
        let plan = plan(&current);
        let request = plan
            .archive_request(&current, &BTreeSet::from(["event".to_owned()]))
            .unwrap();

        assert!(matches!(
            create_cleanup_archive(&home, &home.join(relative), &request, &current),
            Err(CleanupArchiveError::InvalidDestination(_)
                | CleanupArchiveError::DestinationExists(_))
        ));
        assert_eq!(fs::read(home.join(relative)).unwrap(), b"source");
    }

    #[test]
    fn source_replacement_race_is_detected_without_deleting_replacement() {
        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/race.jsonl");
        write_source(&home, relative, b"original");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);
        let raced = Cell::new(false);
        let aside = home.join(relative).with_extension("aside");

        let result = remove_cleanup_sources_with_hook(
            &home,
            &archive,
            &current,
            &authorization,
            |step, _, path| {
                if step == RemovalStep::BeforeQuarantine && !raced.replace(true) {
                    fs::rename(path, &aside)?;
                    fs::write(path, b"replacement")?;
                }
                Ok(())
            },
        );

        assert!(matches!(result, Err(CleanupArchiveError::SourceChanged(_))));
        assert_eq!(fs::read(home.join(relative)).unwrap(), b"replacement");
        assert_eq!(fs::read(aside).unwrap(), b"original");
    }

    #[test]
    fn quarantine_replacement_race_never_deletes_replacement() {
        use std::cell::RefCell;

        let (_temp, home, archive) = setup();
        let relative = Path::new("sessions/2026/race.jsonl");
        write_source(&home, relative, b"original");
        let current = vec![evidence(&home, relative)];
        let (_plan, authorization) = authorized(&home, &archive, &current);
        let replacement_path = RefCell::new(None);

        let result = remove_cleanup_sources_with_hook(
            &home,
            &archive,
            &current,
            &authorization,
            |step, _, path| {
                if step == RemovalStep::BeforeDelete && replacement_path.borrow().is_none() {
                    let aside = path.with_extension("authorized");
                    fs::rename(path, aside)?;
                    fs::write(path, b"replacement")?;
                    *replacement_path.borrow_mut() = Some(path.to_path_buf());
                }
                Ok(())
            },
        );

        assert!(matches!(result, Err(CleanupArchiveError::RollbackFailed)));
        let replacement_path = replacement_path.into_inner().unwrap();
        assert_eq!(fs::read(replacement_path).unwrap(), b"replacement");
    }

    #[test]
    fn partial_quarantine_failure_restores_all_sources() {
        let (_temp, home, archive) = setup();
        let first = Path::new("sessions/2026/first.jsonl");
        let second = Path::new("sessions/2026/second.jsonl");
        write_source(&home, first, b"first");
        write_source(&home, second, b"second");
        let current = vec![evidence(&home, first), evidence(&home, second)];
        let (_plan, authorization) = authorized(&home, &archive, &current);

        let result = remove_cleanup_sources_with_hook(
            &home,
            &archive,
            &current,
            &authorization,
            |step, index, _| {
                if step == RemovalStep::BeforeQuarantine && index == 1 {
                    return Err(io::Error::other("injected failure"));
                }
                Ok(())
            },
        );

        assert!(matches!(result, Err(CleanupArchiveError::Io(_))));
        assert_eq!(fs::read(home.join(first)).unwrap(), b"first");
        assert_eq!(fs::read(home.join(second)).unwrap(), b"second");
    }

    #[test]
    fn partial_delete_reports_exact_recovery_state() {
        let (_temp, home, archive) = setup();
        let first = Path::new("sessions/2026/first.jsonl");
        let second = Path::new("sessions/2026/second.jsonl");
        write_source(&home, first, b"first");
        write_source(&home, second, b"second");
        let current = vec![evidence(&home, first), evidence(&home, second)];
        let (_plan, authorization) = authorized(&home, &archive, &current);

        let result = remove_cleanup_sources_with_hook(
            &home,
            &archive,
            &current,
            &authorization,
            |step, index, _| {
                if step == RemovalStep::BeforeDelete && index == 1 {
                    return Err(io::Error::other("injected delete failure"));
                }
                Ok(())
            },
        );

        let Err(CleanupArchiveError::PartialRemoval {
            removed_relative_paths,
            retained_files,
            archive_path,
        }) = result
        else {
            panic!("expected exact partial result");
        };
        assert_eq!(archive_path, archive);
        assert_eq!(removed_relative_paths, vec![first.to_path_buf()]);
        assert_eq!(retained_files.len(), 1);
        assert_eq!(retained_files[0].relative_path, second);
        assert_eq!(
            fs::read(&retained_files[0].quarantine_path).unwrap(),
            b"second"
        );
        assert!(archive.is_file());
    }

    #[test]
    fn traversal_is_rejected() {
        let (_temp, home, _archive) = setup();
        assert!(matches!(
            cleanup_source_snapshot(&home, Path::new("../outside.jsonl")),
            Err(CleanupArchiveError::UnsafePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let (_temp, home, _archive) = setup();
        let target = Path::new("sessions/2026/target.jsonl");
        let link = Path::new("sessions/2026/link.jsonl");
        write_source(&home, target, b"target");
        symlink(home.join(target), home.join(link)).unwrap();

        assert!(matches!(
            cleanup_source_snapshot(&home, link),
            Err(CleanupArchiveError::SourceChanged(_))
        ));
    }
}
