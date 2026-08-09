use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    SYNC_SCHEMA_VERSION,
    models::{TokenSource, TokenUsage},
};

pub type SyncUsage = TokenUsage;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SyncLedgerRecord {
    pub schema_version: u32,
    pub device_id: String,
    pub device_name: String,
    pub event_id: String,
    #[serde(with = "canonical_timestamp")]
    pub timestamp: DateTime<Utc>,
    pub source: TokenSource,
    pub model: String,
    pub project_hash: String,
    pub session_hash: String,
    pub usage: SyncUsage,
}

impl SyncLedgerRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn v2(
        device_id: String,
        device_name: String,
        event_id: String,
        timestamp: DateTime<Utc>,
        source: TokenSource,
        model: String,
        project_path: &str,
        session_id: &str,
        usage: SyncUsage,
    ) -> Self {
        Self {
            schema_version: SYNC_SCHEMA_VERSION,
            device_id,
            device_name,
            event_id,
            timestamp,
            source,
            model: if model.is_empty() {
                "Unknown".into()
            } else {
                model
            },
            project_hash: privacy_hash(project_path),
            session_hash: privacy_hash(session_id),
            usage,
        }
    }

    pub fn identity_key(&self) -> String {
        format!("{}|{}", self.device_id, self.event_id)
    }

    pub fn project_display_name(&self) -> String {
        display_hash("Project", &self.project_hash)
    }

    pub fn session_display_name(&self) -> String {
        display_hash("Session", &self.session_hash)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LedgerRead {
    pub records: Vec<SyncLedgerRecord>,
    pub parse_error_count: usize,
}

#[derive(Deserialize)]
struct LedgerSchema {
    schema_version: u32,
}

pub fn read_ledger(
    path: &Path,
    imported_after: Option<DateTime<Utc>>,
) -> Result<LedgerRead, SyncError> {
    if !path.exists() {
        return Ok(LedgerRead::default());
    }
    with_ledger_lock(path, |_| read_ledger_unlocked(path, imported_after))
}

fn read_ledger_unlocked(
    path: &Path,
    imported_after: Option<DateTime<Utc>>,
) -> Result<LedgerRead, SyncError> {
    let mut options = OpenOptions::new();
    options.read(true);
    let (mut file, _) = open_regular_nofollow(path, &mut options)?;
    read_ledger_file(&mut file, imported_after)
}

fn read_ledger_file(
    file: &mut fs::File,
    imported_after: Option<DateTime<Utc>>,
) -> Result<LedgerRead, SyncError> {
    file.seek(SeekFrom::Start(0))?;
    let mut records = Vec::new();
    let mut parse_error_count = 0;
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                parse_error_count += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LedgerSchema>(&line) {
            Ok(schema) if matches!(schema.schema_version, 1 | SYNC_SCHEMA_VERSION) => {
                match serde_json::from_str::<SyncLedgerRecord>(&line) {
                    Ok(record) => records.push(record),
                    Err(_) => parse_error_count += 1,
                }
            }
            Ok(_) | Err(_) => parse_error_count += 1,
        }
    }
    let mut records = deduplicated(records);
    if let Some(cutoff) = imported_after {
        records.retain(|record| record.timestamp >= cutoff);
    }
    Ok(LedgerRead {
        records,
        parse_error_count,
    })
}

pub fn requires_local_ledger_replacement(path: &Path) -> Result<bool, SyncError> {
    if !path.exists() {
        return Ok(true);
    }
    with_ledger_lock(path, |_| requires_local_ledger_replacement_unlocked(path))
}

fn requires_local_ledger_replacement_unlocked(path: &Path) -> Result<bool, SyncError> {
    let mut options = OpenOptions::new();
    options.read(true);
    let (mut file, _) = open_regular_nofollow(path, &mut options)?;
    requires_local_ledger_replacement_file(&mut file)
}

fn requires_local_ledger_replacement_file(file: &mut fs::File) -> Result<bool, SyncError> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut replace = false;
    while reader.read_until(b'\n', &mut line)? != 0 {
        if let Ok(schema) = serde_json::from_slice::<LedgerSchema>(&line) {
            if schema.schema_version > SYNC_SCHEMA_VERSION {
                return Err(SyncError::UnsupportedSchemaVersion {
                    found: schema.schema_version,
                    supported: SYNC_SCHEMA_VERSION,
                });
            }
            if schema.schema_version == 1 {
                replace = true;
            }
        }
        line.clear();
    }
    Ok(replace)
}

/// Updates one device ledger while holding the same cross-process lock for the
/// legacy decision, existing-record snapshot, and final rewrite or append.
pub fn write_local_ledger_v2(
    path: &Path,
    records: impl IntoIterator<Item = SyncLedgerRecord>,
    replace_existing: bool,
    is_cancelled: impl FnOnce() -> bool,
) -> Result<usize, SyncError> {
    with_ledger_lock(path, |boundary| {
        let records = sorted_v2_records(records)?;
        if records.is_empty() {
            return Ok(0);
        }
        let (existing, replace_legacy) = local_ledger_state(path)?;
        if replace_existing || replace_legacy {
            if is_cancelled() {
                return Ok(0);
            }
            rewrite_local_ledger_v2_unlocked(path, &records, existing, boundary)?;
            Ok(records.len())
        } else {
            drop(existing);
            append_new_v2_records_unlocked(path, records, is_cancelled, boundary)
        }
    })
}

pub fn rewrite_local_ledger_v2(
    path: &Path,
    records: impl IntoIterator<Item = SyncLedgerRecord>,
) -> Result<usize, SyncError> {
    let records = sorted_v2_records(records)?;
    if records.is_empty() {
        return Ok(0);
    }
    with_ledger_lock(path, |boundary| {
        let (existing, _) = local_ledger_state(path)?;
        rewrite_local_ledger_v2_unlocked(path, &records, existing, boundary)
    })?;
    Ok(records.len())
}

fn rewrite_local_ledger_v2_unlocked(
    path: &Path,
    records: &[SyncLedgerRecord],
    existing: Option<OpenedLedger>,
    boundary: &LedgerBoundary,
) -> Result<(), SyncError> {
    rewrite_local_ledger_v2_unlocked_with(path, records, existing, boundary, || Ok(()))
}

fn rewrite_local_ledger_v2_unlocked_with(
    path: &Path,
    records: &[SyncLedgerRecord],
    existing: Option<OpenedLedger>,
    boundary: &LedgerBoundary,
    before_replace: impl FnOnce() -> Result<(), SyncError>,
) -> Result<(), SyncError> {
    let expected_identity = existing.as_ref().map(|opened| opened.identity);
    let temp = temporary_sibling(path);
    #[cfg(target_os = "windows")]
    let mut owned_temp_identity = None;
    #[cfg(target_os = "macos")]
    let mut owned_temp_identity = None;
    #[cfg(target_os = "macos")]
    let mut namespace_mutated = false;
    let result = (|| -> Result<(), SyncError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        boundary.validate_target(path, expected_identity)?;
        let (mut file, temp_identity) = open_regular_nofollow(&temp, &mut options)?;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            owned_temp_identity = Some(temp_identity);
        }
        boundary.validate_target(&temp, Some(temp_identity))?;
        write_lines(&mut file, records)?;
        file.sync_all()?;
        #[cfg(target_os = "macos")]
        let temp_probe = OpenedLedger {
            file,
            identity: temp_identity,
        };
        #[cfg(not(target_os = "macos"))]
        let temp_probe = {
            drop(file);
            open_regular_replace_probe(&temp)?
        };
        if temp_probe.identity != temp_identity {
            return Err(identity_changed("temporary sync ledger changed after writing").into());
        }
        boundary.validate_target(path, expected_identity)?;
        boundary.validate_target(&temp, Some(temp_identity))?;
        before_replace()?;
        #[cfg(target_os = "macos")]
        replace_ledger_identity_bound(
            &temp,
            path,
            &temp_probe,
            existing.as_ref(),
            boundary,
            &mut namespace_mutated,
        )?;
        #[cfg(not(target_os = "macos"))]
        replace_ledger_identity_bound(&temp, path, &temp_probe, existing.as_ref(), boundary)?;
        Ok(())
    })();
    #[cfg(target_os = "macos")]
    if result.is_err()
        && !namespace_mutated
        && let Some(identity) = owned_temp_identity
    {
        let _ = remove_checked_temp(boundary, &temp, identity);
    }
    #[cfg(target_os = "windows")]
    if result.is_err()
        && let Some(identity) = owned_temp_identity
    {
        let _ = remove_exact_leaf(boundary, &temp, identity);
    }
    result
}

#[cfg(target_os = "macos")]
fn replace_ledger_identity_bound(
    temp: &Path,
    destination: &Path,
    temp_probe: &OpenedLedger,
    existing: Option<&OpenedLedger>,
    boundary: &LedgerBoundary,
    namespace_mutated: &mut bool,
) -> Result<(), SyncError> {
    if let Some(existing) = existing {
        renameatx(boundary, temp, destination, libc::RENAME_SWAP)?;
        *namespace_mutated = true;
        let destination_after = open_regular_replace_probe(destination)?;
        let displaced_after = open_regular_replace_probe(temp)?;
        if destination_after.identity == temp_probe.identity
            && displaced_after.identity == existing.identity
        {
            remove_checked_temp(boundary, temp, existing.identity)?;
            return Ok(());
        }

        boundary.validate_target(destination, Some(destination_after.identity))?;
        boundary.validate_target(temp, Some(displaced_after.identity))?;
        renameatx(boundary, temp, destination, libc::RENAME_SWAP)?;
        boundary.validate_target(destination, Some(displaced_after.identity))?;
        boundary.validate_target(temp, Some(destination_after.identity))?;
        if destination_after.identity == temp_probe.identity {
            remove_checked_temp(boundary, temp, temp_probe.identity)?;
        }
        return Err(identity_changed("sync ledger destination changed during replacement").into());
    }

    renameatx(boundary, temp, destination, libc::RENAME_EXCL)?;
    *namespace_mutated = true;
    boundary.validate_target(destination, Some(temp_probe.identity))?;
    boundary._devices_file.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_checked_temp(
    boundary: &LedgerBoundary,
    path: &Path,
    expected: FileIdentity,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    boundary.validate(path)?;
    let opened = open_regular_replace_probe(path)?;
    if opened.identity != expected {
        return Err(identity_changed(
            "temporary sync ledger identity changed before cleanup",
        ));
    }
    boundary.validate_target(path, Some(expected))?;
    let leaf = leaf_cstring(path)?;
    // ponytail: this relies on the agreed non-malicious cloud-writer boundary for our unique leaf.
    // SAFETY: the checked leaf is relative to the verified, held devices directory handle.
    if unsafe { libc::unlinkat(boundary._devices_file.as_raw_fd(), leaf.as_ptr(), 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    boundary._devices_file.sync_all()
}

#[cfg(target_os = "macos")]
fn renameatx(
    boundary: &LedgerBoundary,
    source: &Path,
    destination: &Path,
    flags: libc::c_uint,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let source = leaf_cstring(source)?;
    let destination = leaf_cstring(destination)?;
    // SAFETY: both names are direct NUL-terminated leaves relative to the held devices dirfd.
    if unsafe {
        libc::renameatx_np(
            boundary._devices_file.as_raw_fd(),
            source.as_ptr(),
            boundary._devices_file.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    } == -1
    {
        Err(map_race_safe_rename_error(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn map_race_safe_rename_error(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(libc::ENOTSUP) || error.raw_os_error() == Some(libc::EOPNOTSUPP)
    {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "race-safe sync ledger replacement is unsupported by this filesystem",
        )
    } else {
        error
    }
}

#[cfg(target_os = "windows")]
fn replace_ledger_identity_bound(
    temp: &Path,
    destination: &Path,
    temp_probe: &OpenedLedger,
    existing: Option<&OpenedLedger>,
    boundary: &LedgerBoundary,
) -> Result<(), SyncError> {
    let Some(existing) = existing else {
        fs::rename(temp, destination)?;
        let destination_after = open_regular_replace_probe(destination)?;
        if destination_after.identity != temp_probe.identity {
            return Err(
                identity_changed("sync ledger destination changed during replacement").into(),
            );
        }
        boundary.validate_target(destination, Some(temp_probe.identity))?;
        return Ok(());
    };

    let backup = recovery_sibling(destination, "backup");
    boundary.validate_target(&backup, None)?;
    replace_file_windows(temp, destination, &backup)?;
    let destination_after = open_regular_replace_probe(destination)?;
    let displaced_after = open_regular_replace_probe(&backup)?;
    if destination_after.identity == temp_probe.identity
        && displaced_after.identity == existing.identity
    {
        remove_exact_leaf(boundary, &backup, existing.identity)?;
        return Ok(());
    }

    let rollback = recovery_sibling(destination, "rollback");
    boundary.validate_target(&rollback, None)?;
    let destination_identity = destination_after.identity;
    let displaced_identity = displaced_after.identity;
    boundary.validate_target(destination, Some(destination_identity))?;
    boundary.validate_target(&backup, Some(displaced_identity))?;
    replace_file_windows(&backup, destination, &rollback)?;
    boundary.validate_target(destination, Some(displaced_identity))?;
    boundary.validate_target(&rollback, Some(destination_identity))?;
    if destination_identity == temp_probe.identity {
        remove_exact_leaf(boundary, &rollback, temp_probe.identity)?;
    }
    Err(identity_changed("sync ledger destination changed during replacement").into())
}

#[cfg(target_os = "windows")]
fn replace_file_windows(replacement: &Path, destination: &Path, backup: &Path) -> io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let replacement = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let backup = backup
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: all three paths are valid NUL-terminated UTF-16 buffers for this call.
    if unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            0,
            ptr::null(),
            ptr::null(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn replace_ledger_identity_bound(
    temp: &Path,
    destination: &Path,
    temp_probe: &OpenedLedger,
    _: Option<&OpenedLedger>,
    boundary: &LedgerBoundary,
) -> Result<(), SyncError> {
    fs::rename(temp, destination)?;
    boundary.validate_target(destination, Some(temp_probe.identity))?;
    boundary._devices_file.sync_all()?;
    Ok(())
}

fn identity_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub fn append_new_v2_records(
    path: &Path,
    candidates: impl IntoIterator<Item = SyncLedgerRecord>,
) -> Result<usize, SyncError> {
    let candidates = sorted_v2_records(candidates)?;
    with_ledger_lock(path, |boundary| {
        append_new_v2_records_unlocked(path, candidates, || false, boundary)
    })
}

fn append_new_v2_records_unlocked<F>(
    path: &Path,
    candidates: Vec<SyncLedgerRecord>,
    is_cancelled: F,
    boundary: &LedgerBoundary,
) -> Result<usize, SyncError>
where
    F: FnOnce() -> bool,
{
    let mut options = OpenOptions::new();
    options.read(true).append(true);
    let mut file = open_optional_regular_nofollow(path, &mut options)?;
    let existing_keys = if let Some((file, identity)) = file.as_mut() {
        boundary.validate_target(path, Some(*identity))?;
        if requires_local_ledger_replacement_file(file)? {
            return Err(SyncError::LegacyLedgerRequiresReplacement);
        }
        read_ledger_file(file, None)?
            .records
            .into_iter()
            .map(|record| record.identity_key())
            .collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };
    let candidates = candidates
        .into_iter()
        .filter(|record| !existing_keys.contains(&record.identity_key()))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(0);
    }
    if is_cancelled() {
        return Ok(0);
    }
    let (mut file, identity) = match file {
        Some((file, identity)) => (file, identity),
        None => {
            boundary.validate_target(path, None)?;
            let mut options = OpenOptions::new();
            options.read(true).append(true).create_new(true);
            open_regular_nofollow(path, &mut options)?
        }
    };
    boundary.validate_target(path, Some(identity))?;
    let length = file.metadata()?.len();
    if length > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut final_byte = [0];
        file.read_exact(&mut final_byte)?;
        if final_byte[0] != b'\n' {
            file.write_all(b"\n")?;
        }
        file.seek(SeekFrom::End(0))?;
    }
    write_lines(&mut file, &candidates)?;
    file.sync_data()?;
    boundary.validate_target(path, Some(identity))?;
    Ok(candidates.len())
}

pub fn privacy_hash(value: &str) -> String {
    if value.is_empty() || value == "Unknown" {
        return "unknown".into();
    }
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn safe_device_file_name(device_id: &str) -> String {
    let name = device_id
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if name.is_empty() {
        "device".into()
    } else {
        name
    }
}

fn sorted_v2_records(
    records: impl IntoIterator<Item = SyncLedgerRecord>,
) -> Result<Vec<SyncLedgerRecord>, SyncError> {
    let mut by_key = BTreeMap::new();
    for mut record in records {
        if record.device_id.is_empty() || record.event_id.is_empty() {
            return Err(SyncError::MissingIdentity);
        }
        record.schema_version = SYNC_SCHEMA_VERSION;
        by_key.insert(record.identity_key(), record);
    }
    let mut records = by_key.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.event_id.cmp(&right.event_id))
    });
    Ok(records)
}

fn deduplicated(records: Vec<SyncLedgerRecord>) -> Vec<SyncLedgerRecord> {
    let mut seen = HashSet::new();
    records
        .into_iter()
        .filter(|record| seen.insert(record.identity_key()))
        .collect()
}

fn write_lines(writer: &mut impl Write, records: &[SyncLedgerRecord]) -> Result<(), SyncError> {
    for record in records {
        // serde_json::Map is key-sorted by default, matching Swift's .sortedKeys output.
        let mut line = serde_json::to_vec(&serde_json::to_value(record)?)?;
        line.push(b'\n');
        writer.write_all(&line)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity(u64, u64);

struct OpenedLedger {
    file: fs::File,
    identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeKind {
    File,
    Directory,
}

struct LedgerBoundary {
    root_path: PathBuf,
    devices_path: PathBuf,
    canonical_root: PathBuf,
    canonical_devices: PathBuf,
    root_identity: FileIdentity,
    devices_identity: FileIdentity,
    _root_file: fs::File,
    _devices_file: fs::File,
}

impl LedgerBoundary {
    fn open(ledger_path: &Path) -> io::Result<Self> {
        let devices_path = ledger_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync ledger must be a direct child of a devices directory",
            )
        })?;
        if devices_path.file_name() != Some(std::ffi::OsStr::new("devices")) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync ledger must be a direct child of a devices directory",
            ));
        }
        let root_path = devices_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        let (root_file, root_identity) = open_directory_nofollow(root_path)?;
        ensure_node_identity(root_path, NodeKind::Directory, root_identity)?;
        match fs::create_dir(devices_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        ensure_node_identity(root_path, NodeKind::Directory, root_identity)?;
        let (devices_file, devices_identity) = open_directory_nofollow(devices_path)?;

        let boundary = Self {
            root_path: root_path.to_path_buf(),
            devices_path: devices_path.to_path_buf(),
            canonical_root: fs::canonicalize(root_path)?,
            canonical_devices: fs::canonicalize(devices_path)?,
            root_identity,
            devices_identity,
            _root_file: root_file,
            _devices_file: devices_file,
        };
        boundary.validate(ledger_path)?;
        Ok(boundary)
    }

    fn validate(&self, ledger_path: &Path) -> io::Result<()> {
        if ledger_path.parent() != Some(self.devices_path.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync ledger escaped its devices directory",
            ));
        }
        ensure_node_identity(&self.root_path, NodeKind::Directory, self.root_identity)?;
        ensure_node_identity(
            &self.devices_path,
            NodeKind::Directory,
            self.devices_identity,
        )?;

        let canonical_root = fs::canonicalize(&self.root_path)?;
        let canonical_devices = fs::canonicalize(&self.devices_path)?;
        if canonical_root != self.canonical_root
            || canonical_devices != self.canonical_devices
            || canonical_devices.parent() != Some(canonical_root.as_path())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sync devices directory escaped its root",
            ));
        }

        ensure_node_identity(&self.root_path, NodeKind::Directory, self.root_identity)?;
        ensure_node_identity(
            &self.devices_path,
            NodeKind::Directory,
            self.devices_identity,
        )
    }

    fn validate_target(
        &self,
        target: &Path,
        expected_identity: Option<FileIdentity>,
    ) -> io::Result<()> {
        self.validate(target)?;
        ensure_path_identity(target, expected_identity)?;
        self.validate(target)
    }
}

fn local_ledger_state(path: &Path) -> Result<(Option<OpenedLedger>, bool), SyncError> {
    let Some(mut opened) = open_optional_regular_replace_probe(path)? else {
        return Ok((None, false));
    };
    let replace = requires_local_ledger_replacement_file(&mut opened.file)?;
    Ok((Some(opened), replace))
}

fn open_optional_regular_replace_probe(path: &Path) -> io::Result<Option<OpenedLedger>> {
    match open_regular_replace_probe(path) {
        Ok(opened) => Ok(Some(opened)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_optional_regular_nofollow(
    path: &Path,
    options: &mut OpenOptions,
) -> io::Result<Option<(fs::File, FileIdentity)>> {
    match open_regular_nofollow(path, options) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_regular_replace_probe(path: &Path) -> io::Result<OpenedLedger> {
    let mut options = OpenOptions::new();
    options.read(true);
    let (file, identity) = open_node_replace_compatible(path, &mut options)?;
    Ok(OpenedLedger { file, identity })
}

fn open_node_replace_compatible(
    path: &Path,
    options: &mut OpenOptions,
) -> io::Result<(fs::File, FileIdentity)> {
    configure_replace_compatible(options);
    let file = options.open(path)?;
    let identity = node_identity(&file, NodeKind::File)?;
    if current_replace_compatible_identity(path)? != identity {
        return Err(identity_changed(
            "sync ledger path changed while it was opened",
        ));
    }
    Ok((file, identity))
}

fn current_replace_compatible_identity(path: &Path) -> io::Result<FileIdentity> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_replace_compatible(&mut options);
    let file = options.open(path)?;
    node_identity(&file, NodeKind::File)
}

fn open_regular_nofollow(
    path: &Path,
    options: &mut OpenOptions,
) -> io::Result<(fs::File, FileIdentity)> {
    open_node_nofollow(path, options, NodeKind::File)
}

fn open_directory_nofollow(path: &Path) -> io::Result<(fs::File, FileIdentity)> {
    let mut options = OpenOptions::new();
    options.read(true);
    open_node_nofollow(path, &mut options, NodeKind::Directory)
}

fn open_node_nofollow(
    path: &Path,
    options: &mut OpenOptions,
    kind: NodeKind,
) -> io::Result<(fs::File, FileIdentity)> {
    configure_nofollow(options, kind);
    let file = options.open(path)?;
    let identity = node_identity(&file, kind)?;
    if current_path_identity(path, kind)? != identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync ledger path changed while it was opened",
        ));
    }
    Ok((file, identity))
}

fn ensure_path_identity(path: &Path, expected: Option<FileIdentity>) -> io::Result<()> {
    match expected {
        Some(expected) if current_path_identity(path, NodeKind::File)? == expected => Ok(()),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync ledger path identity changed before replacement",
        )),
        None => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "sync ledger path appeared before replacement",
            )),
            Err(error) => Err(error),
        },
    }
}

fn ensure_node_identity(path: &Path, kind: NodeKind, expected: FileIdentity) -> io::Result<()> {
    if current_path_identity(path, kind)? == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync path identity changed while it was in use",
        ))
    }
}

fn current_path_identity(path: &Path, kind: NodeKind) -> io::Result<FileIdentity> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_nofollow(&mut options, kind);
    let file = options.open(path)?;
    node_identity(&file, kind)
}

#[cfg(unix)]
fn configure_nofollow(options: &mut OpenOptions, _: NodeKind) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(unix)]
fn configure_replace_compatible(options: &mut OpenOptions) {
    configure_nofollow(options, NodeKind::File);
}

#[cfg(target_os = "windows")]
fn configure_nofollow(options: &mut OpenOptions, kind: NodeKind) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let directory_flag = if kind == NodeKind::Directory {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        0
    };
    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | directory_flag)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
}

#[cfg(target_os = "windows")]
fn configure_replace_compatible(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
}

#[cfg(unix)]
fn node_identity(file: &fs::File, kind: NodeKind) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if (kind == NodeKind::File && !metadata.is_file())
        || (kind == NodeKind::Directory && !metadata.is_dir())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync ledger path is not a regular file",
        ));
    }
    Ok(FileIdentity(metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "windows")]
fn node_identity(file: &fs::File, kind: NodeKind) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the File owns a live HANDLE and information points to writable storage.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if !windows_attributes_match_kind(information.dwFileAttributes, kind) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync ledger path is not a direct regular file",
        ));
    }
    Ok(FileIdentity(
        u64::from(information.dwVolumeSerialNumber),
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_attributes_match_kind(attributes: u32, kind: NodeKind) -> bool {
    const DIRECTORY: u32 = 0x10;
    const REPARSE_POINT: u32 = 0x400;
    attributes & REPARSE_POINT == 0
        && ((attributes & DIRECTORY != 0) == (kind == NodeKind::Directory))
}

fn ledger_lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    name.into()
}

fn with_ledger_lock<T>(
    path: &Path,
    operation: impl FnOnce(&LedgerBoundary) -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    let boundary = LedgerBoundary::open(path)?;
    boundary.validate(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock_path = ledger_lock_path(path);
    let (lock, identity) = open_regular_nofollow(&lock_path, &mut options)?;
    boundary.validate_target(&lock_path, Some(identity))?;
    lock.lock()?;
    boundary.validate_target(&lock_path, Some(identity))?;
    let value = operation(&boundary)?;
    boundary.validate_target(&lock_path, Some(identity))?;
    boundary.validate(path)?;
    Ok(value)
}

fn display_hash(prefix: &str, hash: &str) -> String {
    if hash == "unknown" {
        "Unknown".into()
    } else {
        format!("{prefix} {}", hash.chars().take(8).collect::<String>())
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    #[cfg(target_os = "macos")]
    {
        let mut name = std::ffi::OsString::from(".");
        name.push(path.file_name().unwrap_or(std::ffi::OsStr::new("ledger")));
        name.push(format!(".tmp-{}-{nonce}", std::process::id()));
        path.with_file_name(name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        path.with_extension(format!("tmp-{}-{nonce}", std::process::id()))
    }
}

#[cfg(target_os = "windows")]
fn recovery_sibling(path: &Path, role: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("{role}-{}-{nonce}", std::process::id()))
}

#[cfg(target_os = "macos")]
fn leaf_cstring(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    let leaf = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "sync ledger leaf is missing")
    })?;
    std::ffi::CString::new(leaf.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync ledger leaf contains a NUL byte",
        )
    })
}

#[cfg(target_os = "windows")]
fn remove_exact_leaf(
    boundary: &LedgerBoundary,
    path: &Path,
    expected: FileIdentity,
) -> io::Result<()> {
    use std::{mem::size_of, os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FILE_READ_ATTRIBUTES, FileDispositionInfoEx,
        SetFileInformationByHandle,
    };

    boundary.validate_target(path, Some(expected))?;
    let mut options = OpenOptions::new();
    options.access_mode(DELETE | FILE_READ_ATTRIBUTES);
    let (file, identity) = open_node_replace_compatible(path, &mut options)?;
    if identity != expected {
        return Err(identity_changed(
            "temporary sync ledger identity changed before cleanup",
        ));
    }
    boundary.validate(path)?;
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: the verified file handle has DELETE access and the typed buffer has exact size.
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

mod canonical_timestamp {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(date: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&date.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&value)
            .map(|date| date.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("sync ledger I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("sync ledger JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sync record device_id and event_id must not be empty")]
    MissingIdentity,
    #[error("schema v1 local ledger must be replaced before appending")]
    LegacyLedgerRequiresReplacement,
    #[error(
        "sync ledger schema version {found} is unsupported; this app supports up to version {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    fn record(device: &str, event: &str, input: i64) -> SyncLedgerRecord {
        SyncLedgerRecord::v2(
            device.into(),
            "Mac A".into(),
            event.into(),
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            TokenSource::Codex,
            "gpt-5.5".into(),
            "/private/project",
            "private-session",
            SyncUsage {
                input,
                total: input,
                ..SyncUsage::default()
            },
        )
    }

    fn recovery_files(path: &Path) -> Vec<PathBuf> {
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.contains(".tmp-") || name.contains(".backup-") || name.contains(".rollback-")
            })
            .collect()
    }

    #[test]
    fn writes_exact_v2_keys_hashes_private_values_and_deduplicates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        let count = rewrite_local_ledger_v2(
            &path,
            [record("mac-a", "event-a", 1), record("mac-a", "event-a", 2)],
        )
        .unwrap();
        assert_eq!(count, 1);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""schema_version":2"#));
        assert!(text.contains(r#""device_id":"mac-a""#));
        assert!(text.contains(r#""cachedInput":0"#));
        assert!(!text.contains("project_path"));
        assert!(!text.contains("/private/project"));
        assert!(!text.contains("private-session"));
        assert!(text.contains("\"timestamp\":\"2026-01-02T03:04:05.000Z\""));
        assert_eq!(
            privacy_hash("secret"),
            "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
        );
    }

    #[test]
    fn reads_v1_and_safely_rewrites_local_ledger_as_v2() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            concat!(
                r#"{"device_id":"mac-a","device_name":"Mac A","event_id":"stale","model":"gpt-5.5","project_hash":"abcdef","schema_version":1,"session_hash":"123456","source":"codex","timestamp":"2026-01-01T00:00:00.000Z","usage":{"input":1000,"cachedInput":900,"cacheCreation":0,"cacheRead":0,"output":0,"reasoning":0,"total":1000}}"#,
                "\n"
            ),
        )
        .unwrap();
        let legacy = read_ledger(&path, None).unwrap();
        assert_eq!(legacy.records[0].schema_version, 1);
        assert!(requires_local_ledger_replacement(&path).unwrap());

        rewrite_local_ledger_v2(&path, [record("mac-a", "fresh", 10)]).unwrap();
        let current = read_ledger(&path, None).unwrap();
        assert_eq!(current.records.len(), 1);
        assert_eq!(current.records[0].schema_version, SYNC_SCHEMA_VERSION);
        assert_eq!(current.records[0].event_id, "fresh");
        assert!(!fs::read_to_string(path).unwrap().contains("stale"));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn replacement_race_preserves_both_the_original_and_competing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        let moved = path.with_extension("moved");
        rewrite_local_ledger_v2(&path, [record("mac-a", "original", 1)]).unwrap();
        let original = fs::read(&path).unwrap();
        let replacement = sorted_v2_records([record("mac-a", "new", 2)]).unwrap();

        let result = with_ledger_lock(&path, |boundary| {
            let (existing, _) = local_ledger_state(&path)?;
            rewrite_local_ledger_v2_unlocked_with(&path, &replacement, existing, boundary, || {
                fs::rename(&path, &moved)?;
                fs::write(&path, b"competing destination\n")?;
                Ok(())
            })
        });

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"competing destination\n");
        assert_eq!(fs::read(&moved).unwrap(), original);
        assert!(recovery_files(&path).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn successful_existing_rewrite_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        rewrite_local_ledger_v2(&path, [record("mac-a", "original", 1)]).unwrap();

        rewrite_local_ledger_v2(&path, [record("mac-a", "replacement", 2)]).unwrap();

        assert_eq!(
            read_ledger(&path, None).unwrap().records[0].event_id,
            "replacement"
        );
        assert!(recovery_files(&path).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unsupported_pre_swap_failure_keeps_destination_and_removes_temp() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        rewrite_local_ledger_v2(&path, [record("mac-a", "original", 1)]).unwrap();
        let original = fs::read(&path).unwrap();
        let replacement = sorted_v2_records([record("mac-a", "new", 2)]).unwrap();

        let error = with_ledger_lock(&path, |boundary| {
            let (existing, _) = local_ledger_state(&path)?;
            rewrite_local_ledger_v2_unlocked_with(&path, &replacement, existing, boundary, || {
                Err(map_race_safe_rename_error(io::Error::from_raw_os_error(libc::ENOTSUP)).into())
            })
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("race-safe sync ledger replacement")
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(recovery_files(&path).is_empty());
    }

    #[test]
    fn finds_the_first_valid_schema_after_a_malformed_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = b"not json\n".repeat(16);
        bytes.extend_from_slice(&serde_json::to_vec(&record("mac-a", "current", 1)).unwrap());
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        assert!(!requires_local_ledger_replacement(&path).unwrap());

        fs::write(&path, b"not json\n".repeat(20)).unwrap();
        assert!(!requires_local_ledger_replacement(&path).unwrap());
    }

    #[test]
    fn append_deduplicates_by_device_and_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        rewrite_local_ledger_v2(&path, [record("mac-a", "same", 1)]).unwrap();
        assert_eq!(
            append_new_v2_records(
                &path,
                [record("mac-a", "same", 2), record("mac-b", "same", 3)]
            )
            .unwrap(),
            1
        );
        let records = read_ledger(&path, None).unwrap().records;
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .map(SyncLedgerRecord::identity_key)
                .collect::<HashSet<_>>(),
            HashSet::from(["mac-a|same".into(), "mac-b|same".into()])
        );
    }

    #[test]
    fn deduplicates_before_windowing_and_isolates_bad_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = record("mac-a", "duplicate", 10);
        let mut late = old.clone();
        late.timestamp = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();
        let mut bytes = serde_json::to_vec(&old).unwrap();
        bytes.extend_from_slice(b"\n{\xff}\n");
        bytes.extend_from_slice(&serde_json::to_vec(&late).unwrap());
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();

        let loaded = read_ledger(
            &path,
            Some(Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap()),
        )
        .unwrap();
        assert!(loaded.records.is_empty());
        assert_eq!(loaded.parse_error_count, 1);
    }

    #[test]
    fn normalizes_legacy_usage_and_preserves_unicode_device_file_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/legacy.jsonl");
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            concat!(
                r#"{"device_id":"기기-1","device_name":"Mac","event_id":"event","model":"claude","project_hash":"unknown","schema_version":1,"session_hash":"unknown","source":"claude","timestamp":"2026-01-01T00:00:00.000Z","usage":{"input":-100,"cachedInput":-1,"cacheCreation":2,"cacheRead":3,"output":5,"reasoning":-2,"total":-1}}"#,
                "\n"
            ),
        )
        .unwrap();

        let loaded = read_ledger(&path, None).unwrap();
        assert_eq!(loaded.records[0].usage.input, 0);
        assert_eq!(loaded.records[0].usage.total, 10);
        assert_eq!(safe_device_file_name("기기-1"), "기기-1");
    }

    #[test]
    fn serializes_concurrent_host_appends() {
        let directory = tempfile::tempdir().unwrap();
        let path = Arc::new(directory.path().join("devices/mac-a.jsonl"));
        rewrite_local_ledger_v2(&path, [record("mac-a", "base", 1)]).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let threads: Vec<_> = ["token-meter", "dev-console"]
            .into_iter()
            .map(|event| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    append_new_v2_records(&path, [record("mac-a", event, 1)]).unwrap();
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        let loaded = read_ledger(&path, None).unwrap();
        assert_eq!(loaded.records.len(), 3);
        assert_eq!(loaded.parse_error_count, 0);
    }

    #[test]
    fn serializes_legacy_decision_and_write_across_two_hosts() {
        let directory = tempfile::tempdir().unwrap();
        let path = Arc::new(directory.path().join("devices/mac-a.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut legacy = record("mac-a", "old", 1);
        legacy.schema_version = 1;
        let mut line = serde_json::to_vec(&legacy).unwrap();
        line.push(b'\n');
        fs::write(&*path, line).unwrap();

        let a_inside = Arc::new(Barrier::new(2));
        let release_a = Arc::new(Barrier::new(2));
        let path_a = Arc::clone(&path);
        let a_inside_thread = Arc::clone(&a_inside);
        let release_a_thread = Arc::clone(&release_a);
        let host_a = std::thread::spawn(move || {
            write_local_ledger_v2(&path_a, [record("mac-a", "old", 1)], false, || {
                a_inside_thread.wait();
                release_a_thread.wait();
                false
            })
            .unwrap();
        });
        a_inside.wait();

        let b_started = Arc::new(Barrier::new(2));
        let b_started_thread = Arc::clone(&b_started);
        let path_b = Arc::clone(&path);
        let (b_inside_tx, b_inside_rx) = mpsc::channel();
        let host_b = std::thread::spawn(move || {
            b_started_thread.wait();
            write_local_ledger_v2(
                &path_b,
                [record("mac-a", "old", 1), record("mac-a", "new", 2)],
                false,
                || {
                    b_inside_tx.send(()).unwrap();
                    false
                },
            )
            .unwrap();
        });
        b_started.wait();
        let b_entered_while_a_was_paused =
            b_inside_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        release_a.wait();
        host_a.join().unwrap();
        host_b.join().unwrap();

        assert!(!b_entered_while_a_was_paused);
        let ids = read_ledger(&path, None)
            .unwrap()
            .records
            .into_iter()
            .map(|record| record.event_id)
            .collect::<HashSet<_>>();
        assert_eq!(ids, HashSet::from(["old".into(), "new".into()]));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_precreated_ledger_symlink_without_touching_its_victim() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        let victim = directory.path().join("victim.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&victim, b"keep me").unwrap();
        symlink(&victim, &path).unwrap();

        assert!(
            write_local_ledger_v2(&path, [record("mac-a", "event", 1)], false, || false).is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_precreated_lock_symlink_without_touching_its_victim() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        let lock_path = ledger_lock_path(&path);
        let victim = directory.path().join("victim.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&victim, b"keep me").unwrap();
        symlink(&victim, &lock_path).unwrap();

        assert!(
            write_local_ledger_v2(&path, [record("mac-a", "event", 1)], false, || false).is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(!path.exists());
        assert!(
            fs::symlink_metadata(lock_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_devices_symlink_without_touching_an_outside_ledger() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let outside = directory.path().join("outside");
        let path = root.join("devices/mac-a.jsonl");
        let victim = outside.join("mac-a.jsonl");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(&victim, b"keep me").unwrap();
        symlink(&outside, root.join("devices")).unwrap();

        assert!(
            write_local_ledger_v2(&path, [record("mac-a", "event", 1)], false, || false).is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(!outside.join("mac-a.jsonl.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_sync_root_symlink_without_touching_an_outside_ledger() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        let victim = outside.join("devices/mac-a.jsonl");
        fs::create_dir_all(victim.parent().unwrap()).unwrap();
        fs::write(&victim, b"keep me").unwrap();
        let root = directory.path().join("sync");
        symlink(&outside, &root).unwrap();
        let path = root.join("devices/mac-a.jsonl");

        assert!(
            write_local_ledger_v2(&path, [record("mac-a", "event", 1)], false, || false).is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert!(!outside.join("devices/mac-a.jsonl.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn detects_a_replaced_ledger_identity_before_reporting_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices/mac-a.jsonl");
        let boundary = LedgerBoundary::open(&path).unwrap();
        fs::write(&path, b"original").unwrap();
        let mut options = OpenOptions::new();
        options.read(true);
        let (file, identity) = open_regular_nofollow(&path, &mut options).unwrap();
        drop(file);

        fs::rename(&path, path.with_extension("moved")).unwrap();
        fs::write(&path, b"replacement").unwrap();

        assert!(boundary.validate_target(&path, Some(identity)).is_err());
    }

    #[test]
    fn windows_reparse_points_never_match_direct_files_or_directories() {
        const DIRECTORY: u32 = 0x10;
        const REPARSE_POINT: u32 = 0x400;

        assert!(windows_attributes_match_kind(0, NodeKind::File));
        assert!(windows_attributes_match_kind(
            DIRECTORY,
            NodeKind::Directory
        ));
        assert!(!windows_attributes_match_kind(
            REPARSE_POINT,
            NodeKind::File
        ));
        assert!(!windows_attributes_match_kind(
            DIRECTORY | REPARSE_POINT,
            NodeKind::Directory
        ));
    }
}
