use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::CACHE_PARSER_VERSION;
use crate::cache::{FileSnapshot, OriginKind, TokenEventCache};
use crate::models::{SyncFolderStatus, TokenDeviceMetadata, TokenEvent};
use crate::sync::{
    LedgerRead, SyncLedgerRecord, read_ledger_cancelable,
    requires_local_ledger_replacement_cancelable, safe_device_file_name,
    verified_direct_ledger_paths, verified_ledger_snapshot, write_local_ledger_v2_cached,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
    pub events: Vec<TokenEvent>,
    pub status: SyncFolderStatus,
}

pub struct TokenSyncStore<'a> {
    folder: PathBuf,
    local_device: TokenDeviceMetadata,
    cache: Option<&'a TokenEventCache>,
}

impl<'a> TokenSyncStore<'a> {
    pub fn new(folder: impl Into<PathBuf>, local_device: TokenDeviceMetadata) -> Self {
        Self {
            folder: folder.into(),
            local_device,
            cache: None,
        }
    }

    pub fn with_cache(mut self, cache: &'a TokenEventCache) -> Self {
        self.cache = Some(cache);
        self
    }

    pub fn local_ledger_path(&self) -> PathBuf {
        self.folder.join("devices").join(format!(
            "{}.jsonl",
            safe_device_file_name(&self.local_device.id)
        ))
    }

    pub fn cached_outcome(&self, imported_after: Option<DateTime<Utc>>) -> Option<SyncOutcome> {
        let path = self.folder.to_string_lossy().into_owned();
        if !self.folder.exists() {
            return Some(SyncOutcome {
                events: Vec::new(),
                status: SyncFolderStatus {
                    path: Some(path),
                    ..SyncFolderStatus::default()
                },
            });
        }
        let cache = self.cache?;
        let paths = verified_direct_ledger_paths(&self.folder.join("devices")).ok()?;
        let mut cache_paths = Vec::with_capacity(paths.len());
        for ledger_path in &paths {
            let Some(snapshot) = file_snapshot(ledger_path).ok() else {
                continue;
            };
            let cache_path = snapshot.path.clone();
            let Some(origin) = cache
                .origin_file(OriginKind::SyncLedger, &cache_path)
                .ok()
                .flatten()
            else {
                continue;
            };
            if origin.parser_version != CACHE_PARSER_VERSION
                || origin.parse_error
                || !origin_identity_matches(origin.device_id.as_deref(), &snapshot)
                || origin.file_size > snapshot.size
                || snapshot.modified_at + 0.000_001 < origin.modified_at
                || (ledger_path == &self.local_ledger_path()
                    && (origin.file_size != snapshot.size
                        || (origin.modified_at - snapshot.modified_at).abs() >= 0.000_001))
            {
                continue;
            }
            cache_paths.push(cache_path);
        }
        if !paths.is_empty() && cache_paths.is_empty() {
            return None;
        }
        let events = cache.sync_events(&cache_paths, imported_after).ok()?;
        Some(SyncOutcome {
            status: SyncFolderStatus {
                path: Some(path),
                exists: true,
                imported_event_count: events.len(),
                device_file_count: paths.len(),
                ..SyncFolderStatus::default()
            },
            events,
        })
    }

    pub fn synchronize(
        &self,
        local_events: &[TokenEvent],
        replace_local_ledger: bool,
        imported_after: Option<DateTime<Utc>>,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> SyncOutcome {
        let path = self.folder.to_string_lossy().into_owned();
        if !self.folder.exists() {
            return SyncOutcome {
                events: Vec::new(),
                status: SyncFolderStatus {
                    path: Some(path),
                    ..SyncFolderStatus::default()
                },
            };
        }

        let mut status = SyncFolderStatus {
            path: Some(path),
            exists: true,
            last_synced_at: Some(Utc::now()),
            ..SyncFolderStatus::default()
        };
        if is_cancelled() {
            return SyncOutcome {
                events: Vec::new(),
                status,
            };
        }

        let mut cache_usable = true;
        match self.write_local_ledger(
            local_events,
            replace_local_ledger,
            &mut cache_usable,
            &mut is_cancelled,
        ) {
            Ok(count) => status.exported_event_count = count,
            Err(error) => status.export_error = Some(error.to_string()),
        }
        if is_cancelled() {
            return SyncOutcome {
                events: Vec::new(),
                status,
            };
        }

        let read = read_device_ledgers(
            &self.folder,
            cache_usable.then_some(self.cache).flatten(),
            imported_after,
            &mut is_cancelled,
        );
        status.device_file_count = read.device_file_count;
        status.imported_event_count = read.events.len();
        status.parse_error_count = read.parse_error_count;
        SyncOutcome {
            events: read.events,
            status,
        }
    }

    fn write_local_ledger(
        &self,
        events: &[TokenEvent],
        replace_local_ledger: bool,
        cache_usable: &mut bool,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<usize, crate::sync::SyncError> {
        let mut records = Vec::with_capacity(events.len());
        for event in events {
            if is_cancelled() {
                return Ok(0);
            }
            records.push(SyncLedgerRecord::v2(
                self.local_device.id.clone(),
                self.local_device.name.clone(),
                event.id.clone(),
                event.timestamp,
                event.source,
                event.model.clone(),
                &event.project_path,
                &event.session_id,
                event.usage,
            ));
        }
        if is_cancelled() {
            return Ok(0);
        }

        let path = self.local_ledger_path();
        if records.is_empty() {
            if let Some(cache) = self.cache
                && path.try_exists()?
                && cached_v2_snapshot(cache, &path).is_none()
            {
                self.invalidate_local_cache(&path, cache_usable)?;
            }
            return Ok(0);
        }
        let cache_hint = self
            .cache
            .and_then(|cache| cached_writer_hint(cache, &path, &records));
        let invalidate_cache = replace_local_ledger || cache_hint.is_none();
        let count = write_local_ledger_v2_cached(
            &path,
            records,
            replace_local_ledger,
            cache_hint,
            &mut *is_cancelled,
        )?;
        if count == 0 && is_cancelled() {
            return Ok(0);
        }
        if invalidate_cache {
            self.invalidate_local_cache(&path, cache_usable)?;
        }
        Ok(count)
    }

    fn invalidate_local_cache(
        &self,
        path: &Path,
        cache_usable: &mut bool,
    ) -> Result<(), crate::sync::SyncError> {
        if let Some(cache) = self.cache {
            let result = file_snapshot(path)
                .map(|snapshot| snapshot.path)
                .map_err(|error| error.to_string())
                .and_then(|origin_path| {
                    cache
                        .remove_sync_origin(&origin_path)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                *cache_usable = false;
                return Err(crate::sync::SyncError::Io(std::io::Error::other(error)));
            }
        }
        Ok(())
    }

    pub fn local_ledger_needs_full_export(
        &self,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<bool, crate::sync::SyncError> {
        let path = self.local_ledger_path();
        if !path.try_exists()? {
            return Ok(true);
        }
        if self
            .cache
            .and_then(|cache| cached_v2_snapshot(cache, &path))
            .is_some()
        {
            return Ok(false);
        }
        requires_local_ledger_replacement_cancelable(&path, &mut is_cancelled)
    }
}

pub fn merge_local_and_sync(
    local_events: impl IntoIterator<Item = TokenEvent>,
    sync_events: impl IntoIterator<Item = TokenEvent>,
) -> Vec<TokenEvent> {
    let mut events = std::collections::BTreeMap::new();
    for event in local_events.into_iter().chain(sync_events) {
        let key = (event.device_id.clone(), event.id.clone());
        let replace = events.get(&key).is_none_or(|existing: &TokenEvent| {
            has_local_details(&event) || !has_local_details(existing)
        });
        if replace {
            events.insert(key, event);
        }
    }
    let mut events = events.into_values().collect::<Vec<_>>();
    events.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.id.cmp(&right.id))
    });
    events
}

fn has_local_details(event: &TokenEvent) -> bool {
    !event.raw_file_path.starts_with("sync://")
}

#[derive(Default)]
struct DeviceLedgerRead {
    events: Vec<TokenEvent>,
    device_file_count: usize,
    parse_error_count: usize,
}

fn read_device_ledgers(
    folder: &Path,
    cache: Option<&TokenEventCache>,
    imported_after: Option<DateTime<Utc>>,
    is_cancelled: &mut impl FnMut() -> bool,
) -> DeviceLedgerRead {
    let paths = match verified_direct_ledger_paths(&folder.join("devices")) {
        Ok(paths) => paths,
        Err(_) => {
            return DeviceLedgerRead {
                parse_error_count: 1,
                ..DeviceLedgerRead::default()
            };
        }
    };
    let mut keeping = HashSet::new();
    let mut read = DeviceLedgerRead::default();
    let mut completed = true;
    let mut cache_complete = cache.is_some();
    let mut prefetched = Vec::new();
    for path in &paths {
        if is_cancelled() {
            completed = false;
            break;
        }
        read.device_file_count += 1;
        match cache.map(|cache| reconcile_cached_ledger(path, cache, is_cancelled)) {
            Some(Ok((usable, parse_error_count, cache_path, ledger))) => {
                cache_complete &= usable;
                read.parse_error_count += parse_error_count;
                keeping.insert(cache_path);
                if let Some(ledger) = ledger {
                    prefetched.push((path.clone(), ledger));
                }
            }
            Some(Err(crate::sync::SyncError::Cancelled)) => completed = false,
            Some(Err(_)) => {
                completed = false;
                cache_complete = false;
                read.parse_error_count += 1;
            }
            None => cache_complete = false,
        }
    }

    if completed
        && !is_cancelled()
        && read.device_file_count == paths.len()
        && let Some(cache) = cache
        && cache.remove_missing_sync_origins(&keeping).is_err()
    {
        cache_complete = false;
    }
    if completed && cache_complete {
        match cache
            .unwrap()
            .sync_events(&keeping.iter().cloned().collect::<Vec<_>>(), imported_after)
        {
            Ok(events) => read.events = events,
            Err(_) => {
                return read_ledgers_uncached(paths, prefetched, imported_after, is_cancelled);
            }
        }
    } else if !is_cancelled() {
        return read_ledgers_uncached(paths, prefetched, imported_after, is_cancelled);
    }
    read
}

fn reconcile_cached_ledger(
    path: &Path,
    cache: &TokenEventCache,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<(bool, usize, String, Option<LedgerRead>), crate::sync::SyncError> {
    let current = file_snapshot(path)?;
    let cache_path = current.path.clone();
    if let Ok(Some(origin)) = cache.origin_file(OriginKind::SyncLedger, &cache_path)
        && origin.parser_version == CACHE_PARSER_VERSION
    {
        let identity_matches = origin_identity_matches(origin.device_id.as_deref(), &current);
        if identity_matches
            && origin.file_size == current.size
            && (origin.modified_at - current.modified_at).abs() < 0.000_001
        {
            return Ok((
                !origin.parse_error,
                usize::from(origin.parse_error),
                cache_path,
                None,
            ));
        }
        if identity_matches
            && origin
                .device_id
                .as_deref()
                .is_some_and(|marker| marker.starts_with("v2:"))
            && current.size > origin.file_size
            && current.modified_at + 0.000_001 >= origin.modified_at
            && let Ok(tail) =
                read_ledger_cancelable(path, origin.file_size as u64, &mut *is_cancelled)
            && tail.file_identity.as_deref() == current.device_id.as_deref()
        {
            if is_cancelled() {
                return Err(crate::sync::SyncError::Cancelled);
            }
            let tail_is_v2 = tail
                .records
                .iter()
                .all(|record| record.schema_version == crate::SYNC_SCHEMA_VERSION);
            let parse_error_count = tail.parse_error_count;
            let snapshot = read_snapshot(&cache_path, &tail, tail_is_v2);
            let tail_events = tail
                .records
                .into_iter()
                .map(record_event)
                .collect::<Vec<_>>();
            let candidates = tail_events
                .iter()
                .map(|event| (event.device_id.clone(), event.id.clone()))
                .collect::<Vec<_>>();
            let Ok(existing) = cache.existing_sync_event_keys(&cache_path, &candidates) else {
                return Ok((false, 0, cache_path, None));
            };
            let new_events = tail_events
                .into_iter()
                .filter(|event| !existing.contains(&(event.device_id.clone(), event.id.clone())))
                .collect::<Vec<_>>();
            if origin.parse_error || parse_error_count != 0 {
                return Ok((
                    false,
                    usize::from(origin.parse_error) + parse_error_count,
                    cache_path,
                    None,
                ));
            }
            let usable = snapshot
                .is_some_and(|snapshot| cache.append_sync_events(&snapshot, &new_events).is_ok());
            return Ok((usable, 0, cache_path, None));
        }
    }

    let ledger = read_ledger_cancelable(path, 0, &mut *is_cancelled)?;
    if is_cancelled() {
        return Err(crate::sync::SyncError::Cancelled);
    }
    let is_v2 = ledger
        .records
        .iter()
        .all(|record| record.schema_version == crate::SYNC_SCHEMA_VERSION);
    if ledger.parse_error_count != 0 {
        return Ok((false, ledger.parse_error_count, cache_path, Some(ledger)));
    }
    let events = ledger
        .records
        .iter()
        .cloned()
        .map(record_event)
        .collect::<Vec<_>>();
    let usable = read_snapshot(&cache_path, &ledger, is_v2)
        .is_some_and(|snapshot| cache.replace_sync_events(&snapshot, &events).is_ok());
    Ok((usable, 0, cache_path, (!usable).then_some(ledger)))
}

fn read_ledgers_uncached(
    paths: Vec<PathBuf>,
    mut prefetched: Vec<(PathBuf, LedgerRead)>,
    imported_after: Option<DateTime<Utc>>,
    is_cancelled: &mut impl FnMut() -> bool,
) -> DeviceLedgerRead {
    let mut read = DeviceLedgerRead::default();
    let mut records = Vec::new();
    for path in paths {
        if is_cancelled() {
            break;
        }
        read.device_file_count += 1;
        let ledger = prefetched
            .iter()
            .position(|(prefetched_path, _)| prefetched_path == &path)
            .map(|index| prefetched.swap_remove(index).1)
            .map(Ok)
            .unwrap_or_else(|| read_ledger_cancelable(&path, 0, &mut *is_cancelled));
        match ledger {
            Ok(ledger) => {
                read.parse_error_count += ledger.parse_error_count;
                records.extend(ledger.records);
            }
            Err(_) => read.parse_error_count += 1,
        }
    }
    let mut seen = HashSet::new();
    read.events = records
        .into_iter()
        .filter(|record| seen.insert((record.device_id.clone(), record.event_id.clone())))
        .filter(|record| imported_after.is_none_or(|cutoff| record.timestamp >= cutoff))
        .map(record_event)
        .collect();
    read.events.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.id.cmp(&right.id))
    });
    read
}

fn cached_v2_snapshot(cache: &TokenEventCache, path: &Path) -> Option<(String, u64, f64, String)> {
    let snapshot = file_snapshot(path).ok()?;
    let cache_path = snapshot.path.clone();
    let origin = cache
        .origin_file(OriginKind::SyncLedger, &cache_path)
        .ok()??;
    if origin.parser_version != CACHE_PARSER_VERSION
        || origin.parse_error
        || !v2_origin_identity_matches(origin.device_id.as_deref(), &snapshot)
        || origin.file_size != snapshot.size
        || (origin.modified_at - snapshot.modified_at).abs() >= 0.000_001
    {
        return None;
    }
    Some((
        cache_path,
        snapshot.size as u64,
        snapshot.modified_at,
        snapshot.device_id?,
    ))
}

fn cached_writer_hint(
    cache: &TokenEventCache,
    path: &Path,
    records: &[SyncLedgerRecord],
) -> Option<(u64, f64, String, HashSet<String>)> {
    let (cache_path, size, modified_at, identity) = cached_v2_snapshot(cache, path)?;
    let candidates = records
        .iter()
        .map(|record| (record.device_id.clone(), record.event_id.clone()))
        .collect::<Vec<_>>();
    let existing = cache
        .existing_sync_event_keys(&cache_path, &candidates)
        .ok()?;
    let keys = existing
        .into_iter()
        .map(|(device, event)| format!("{device}|{event}"))
        .collect();
    Some((size, modified_at, identity, keys))
}

fn file_snapshot(path: &Path) -> Result<FileSnapshot, crate::sync::SyncError> {
    let (canonical, size, modified, identity) = verified_ledger_snapshot(path)?;
    let modified_at = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64());
    Ok(FileSnapshot {
        path: canonical.to_string_lossy().into_owned(),
        source: None,
        size: i64::try_from(size).unwrap_or(i64::MAX),
        modified_at,
        device_id: Some(identity),
    })
}

fn read_snapshot(
    cache_path: &str,
    read: &crate::sync::LedgerRead,
    is_v2: bool,
) -> Option<FileSnapshot> {
    Some(FileSnapshot {
        path: cache_path.to_owned(),
        source: None,
        size: i64::try_from(read.file_size).ok()?,
        modified_at: read
            .modified_at?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64(),
        device_id: read
            .file_identity
            .as_ref()
            .map(|identity| format!("{}:{identity}", if is_v2 { "v2" } else { "legacy" })),
    })
}

fn origin_identity_matches(marker: Option<&str>, snapshot: &FileSnapshot) -> bool {
    marker
        .and_then(|marker| marker.split_once(':'))
        .zip(snapshot.device_id.as_deref())
        .is_some_and(|((_, cached), current)| cached == current)
}

fn v2_origin_identity_matches(marker: Option<&str>, snapshot: &FileSnapshot) -> bool {
    marker
        .and_then(|marker| marker.strip_prefix("v2:"))
        .zip(snapshot.device_id.as_deref())
        .is_some_and(|(cached, current)| cached == current)
}

fn record_event(record: SyncLedgerRecord) -> TokenEvent {
    let project = record.project_display_name();
    let session = record.session_display_name();
    let raw_path = format!("sync://{}/{}", record.device_id, record.event_id);
    TokenEvent::new(
        record.event_id,
        record.source,
        record.timestamp,
        record.device_id,
        record.device_name,
        project,
        session,
        record.model,
        record.usage,
        raw_path,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io::Write;

    use chrono::TimeZone;

    use super::*;
    use crate::models::{TokenSource, TokenUsage};
    use crate::sync::{read_ledger, rewrite_local_ledger_v2};

    fn event(device: &str, id: &str, day: u32, raw_path: &str) -> TokenEvent {
        TokenEvent::new(
            id,
            TokenSource::Codex,
            Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap(),
            device,
            format!("{device} name"),
            "/private/project",
            "private-session",
            "gpt-5.5",
            TokenUsage {
                input: 10,
                total: 10,
                ..TokenUsage::default()
            },
            raw_path,
        )
    }

    fn ledger_record(device: &str, id: &str, day: u32) -> SyncLedgerRecord {
        SyncLedgerRecord::v2(
            device.into(),
            format!("{device} name"),
            id.into(),
            Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap(),
            TokenSource::Codex,
            "gpt".into(),
            "/project",
            "session",
            TokenUsage::default(),
        )
    }

    fn store(folder: &Path, device: &str) -> TokenSyncStore<'static> {
        TokenSyncStore::new(
            folder,
            TokenDeviceMetadata::new(device, format!("{device} name")),
        )
    }

    #[test]
    fn merges_all_ledgers_and_prefers_local_details() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        fs::create_dir(&sync).unwrap();
        store(&sync, "mac-a").synchronize(
            &[event("mac-a", "same", 1, "/local/a.jsonl")],
            false,
            None,
            || false,
        );
        store(&sync, "mac-b").synchronize(
            &[event("mac-b", "remote", 2, "/local/b.jsonl")],
            false,
            None,
            || false,
        );

        let outcome = store(&sync, "reader").synchronize(&[], false, None, || false);
        assert_eq!(outcome.status.device_file_count, 2);
        assert_eq!(outcome.events.len(), 2);
        assert!(
            outcome
                .events
                .iter()
                .all(|event| event.raw_file_path.starts_with("sync://"))
        );
        assert!(
            outcome
                .events
                .iter()
                .all(|event| event.project_path.starts_with("Project "))
        );

        let local = event("mac-a", "same", 1, "/local/a.jsonl");
        let merged = merge_local_and_sync([local.clone()], outcome.events);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], local);
    }

    #[test]
    fn deduplicates_before_applying_import_window() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let devices = sync.join("devices");
        fs::create_dir_all(&devices).unwrap();
        let old = SyncLedgerRecord::v2(
            "same-device".into(),
            "Mac".into(),
            "duplicate".into(),
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            TokenSource::Codex,
            "gpt-5.5".into(),
            "/project",
            "session",
            TokenUsage::default(),
        );
        let mut late = old.clone();
        late.timestamp = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();
        rewrite_local_ledger_v2(&devices.join("a.jsonl"), [old]).unwrap();
        rewrite_local_ledger_v2(&devices.join("b.jsonl"), [late]).unwrap();

        let outcome = store(&sync, "reader").synchronize(
            &[],
            false,
            Some(Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap()),
            || false,
        );
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.status.imported_event_count, 0);

        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let cached = store(&sync, "reader").with_cache(&cache);
        let cutoff = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        assert!(
            cached
                .synchronize(&[], false, Some(cutoff), || false)
                .events
                .is_empty()
        );
        assert!(
            cached
                .cached_outcome(Some(cutoff))
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn rewrites_local_v1_to_sorted_private_v2() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let ledger = sync.join("devices/mac-a.jsonl");
        fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        fs::write(
            &ledger,
            concat!(
                r#"{"device_id":"mac-a","device_name":"Mac A","event_id":"stale","model":"gpt-5.5","project_hash":"abcdef","schema_version":1,"session_hash":"123456","source":"codex","timestamp":"2026-01-01T00:00:00.000Z","usage":{"input":1,"cachedInput":0,"cacheCreation":0,"cacheRead":0,"output":0,"reasoning":0,"total":1}}"#,
                "\n"
            ),
        )
        .unwrap();

        let outcome = store(&sync, "mac-a").synchronize(
            &[
                event("mac-a", "later", 2, "/local/later.jsonl"),
                event("mac-a", "earlier", 1, "/local/earlier.jsonl"),
            ],
            false,
            None,
            || false,
        );
        assert_eq!(outcome.status.exported_event_count, 2);
        let text = fs::read_to_string(ledger).unwrap();
        assert!(!text.contains("stale"));
        assert!(!text.contains("/private/project"));
        assert!(!text.contains("private-session"));
        assert!(
            text.lines()
                .all(|line| line.contains(r#""schema_version":2"#))
        );
        assert!(text.find("earlier").unwrap() < text.find("later").unwrap());
    }

    #[test]
    fn rejects_a_mixed_future_local_ledger_without_losing_remote_results_or_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let devices = sync.join("devices");
        fs::create_dir_all(&devices).unwrap();
        rewrite_local_ledger_v2(
            &devices.join("mac-b.jsonl"),
            [SyncLedgerRecord::v2(
                "mac-b".into(),
                "Mac B".into(),
                "remote".into(),
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                TokenSource::Claude,
                "claude".into(),
                "/project",
                "session",
                TokenUsage::default(),
            )],
        )
        .unwrap();

        let ledger = devices.join("mac-a.jsonl");
        let current = SyncLedgerRecord::v2(
            "mac-a".into(),
            "Mac A".into(),
            "current".into(),
            Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
            TokenSource::Codex,
            "current-model".into(),
            "/current-project",
            "current-session",
            TokenUsage::default(),
        );
        let mut future = serde_json::to_value(&current).unwrap();
        future["schema_version"] = serde_json::json!(3);
        future["event_id"] = serde_json::json!("future");
        future["future_only"] = serde_json::json!("must survive");
        let mut original = serde_json::to_vec(&current).unwrap();
        original.push(b'\n');
        original.extend(serde_json::to_vec(&future).unwrap());
        original.push(b'\n');
        fs::write(&ledger, &original).unwrap();

        for replace in [false, true] {
            let outcome = store(&sync, "mac-a").synchronize(
                &[event("mac-a", "local", 3, "/local/event.jsonl")],
                replace,
                None,
                || false,
            );

            assert_eq!(fs::read(&ledger).unwrap(), original);
            assert_eq!(outcome.status.exported_event_count, 0);
            assert!(
                outcome
                    .status
                    .export_error
                    .as_deref()
                    .is_some_and(|error| error.contains("schema version 3"))
            );
            assert_eq!(outcome.status.device_file_count, 2);
            assert_eq!(outcome.status.imported_event_count, 2);
            assert_eq!(outcome.status.parse_error_count, 1);
            assert!(outcome.events.iter().any(|event| event.id == "remote"));
        }
    }

    #[test]
    fn reads_only_direct_regular_jsonl_files_and_counts_bad_lines() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let devices = sync.join("devices");
        fs::create_dir_all(devices.join("ignored.jsonl")).unwrap();
        fs::create_dir_all(devices.join("nested")).unwrap();
        let valid = SyncLedgerRecord::v2(
            "mac-b".into(),
            "Mac B".into(),
            "direct".into(),
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            TokenSource::Claude,
            "claude".into(),
            "/project",
            "session",
            TokenUsage::default(),
        );
        rewrite_local_ledger_v2(&devices.join("direct.jsonl"), [valid.clone()]).unwrap();
        fs::write(
            devices.join("nested/ignored.jsonl"),
            serde_json::to_vec(&valid).unwrap(),
        )
        .unwrap();
        fs::write(devices.join("bad.jsonl"), "not json\n").unwrap();

        let outcome = store(&sync, "reader").synchronize(&[], false, None, || false);
        assert_eq!(outcome.status.device_file_count, 2);
        assert_eq!(outcome.status.parse_error_count, 1);
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].id, "direct");
    }

    #[test]
    fn cancellation_before_and_after_write_never_leaves_a_partial_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        fs::create_dir(&sync).unwrap();
        let store = store(&sync, "mac-a");
        let events = [
            event("mac-a", "first", 1, "/first.jsonl"),
            event("mac-a", "second", 2, "/second.jsonl"),
        ];
        let checks = Cell::new(0);
        store.synchronize(&events, true, None, || {
            checks.set(checks.get() + 1);
            checks.get() >= 4
        });
        assert!(!store.local_ledger_path().exists());

        let checks = Cell::new(0);
        store.synchronize(&events, true, None, || {
            checks.set(checks.get() + 1);
            checks.get() >= 6
        });
        let ledger = fs::read_to_string(store.local_ledger_path()).unwrap();
        assert_eq!(ledger.lines().count(), 2);
        assert!(
            ledger
                .lines()
                .all(|line| serde_json::from_str::<SyncLedgerRecord>(line).is_ok())
        );
    }

    #[test]
    fn cached_ledger_reads_append_only_tail_and_deduplicates_before_cutoff() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let devices = sync.join("devices");
        fs::create_dir_all(&devices).unwrap();
        let path = devices.join("a.jsonl");
        let old = SyncLedgerRecord::v2(
            "remote".into(),
            "Remote".into(),
            "duplicate".into(),
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            TokenSource::Codex,
            "gpt".into(),
            "/project",
            "session",
            TokenUsage::default(),
        );
        rewrite_local_ledger_v2(&path, [old.clone()]).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let store = store(&sync, "reader").with_cache(&cache);
        assert_eq!(
            store.synchronize(&[], false, None, || false).events.len(),
            1
        );
        let cached = store.cached_outcome(None).unwrap();
        assert_eq!(cached.events.len(), 1);
        rewrite_local_ledger_v2(
            &devices.join("b.jsonl"),
            [ledger_record("other", "uncached", 1)],
        )
        .unwrap();
        assert_eq!(store.cached_outcome(None).unwrap().events.len(), 1);

        let mut late = old;
        late.timestamp = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        serde_json::to_writer(&mut file, &late).unwrap();
        writeln!(file).unwrap();
        file.sync_all().unwrap();

        let outcome = store.synchronize(
            &[],
            false,
            Some(Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap()),
            || false,
        );
        assert!(outcome.events.is_empty());
        assert!(store.cached_outcome(None).is_some());
    }

    #[test]
    fn atomic_growth_replacement_never_reuses_the_old_cached_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let path = sync.join("devices/remote.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = ledger_record("remote", "old", 1);
        let new = ledger_record("remote", "new", 1);
        assert_eq!(
            serde_json::to_vec(&old).unwrap().len(),
            serde_json::to_vec(&new).unwrap().len()
        );
        rewrite_local_ledger_v2(&path, [old]).unwrap();
        let cached_offset = path.metadata().unwrap().len();
        let cache_db = directory.path().join("cache.sqlite");
        let cache = TokenEventCache::open_or_create(&cache_db).unwrap();
        let store = store(&sync, "reader").with_cache(&cache);
        assert_eq!(
            store.synchronize(&[], false, None, || false).events[0].id,
            "old"
        );
        rusqlite::Connection::open(&cache_db)
            .unwrap()
            .execute(
                "UPDATE origin_files SET device_id = 'v2' WHERE origin_kind = 'sync_ledger'",
                [],
            )
            .unwrap();
        assert!(store.cached_outcome(None).is_none());
        assert_eq!(
            store.synchronize(&[], false, None, || false).events[0].id,
            "old"
        );
        let origin_path = path.canonicalize().unwrap().to_string_lossy().into_owned();
        assert!(
            cache
                .origin_file(OriginKind::SyncLedger, &origin_path)
                .unwrap()
                .unwrap()
                .device_id
                .unwrap()
                .starts_with("v2:")
        );

        rewrite_local_ledger_v2(&path, [new, ledger_record("remote", "second", 2)]).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() as u64 > cached_offset);
        assert_eq!(bytes[(cached_offset - 1) as usize], b'\n');
        assert!(store.cached_outcome(None).is_none());

        let outcome = store.synchronize(&[], false, None, || false);
        assert_eq!(
            outcome
                .events
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            ["new", "second"]
        );
    }

    #[test]
    fn replacement_replaces_cached_events_and_warmed_v1_is_not_appended() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let devices = sync.join("devices");
        fs::create_dir_all(&devices).unwrap();
        let remote = devices.join("remote.jsonl");
        rewrite_local_ledger_v2(&remote, [ledger_record("remote", "old", 1)]).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let reader = store(&sync, "reader").with_cache(&cache);
        assert_eq!(
            reader.synchronize(&[], false, None, || false).events[0].id,
            "old"
        );
        rewrite_local_ledger_v2(&remote, [ledger_record("remote", "new", 2)]).unwrap();
        let replaced = reader.synchronize(&[], false, None, || false);
        assert_eq!(replaced.events.len(), 1);
        assert_eq!(replaced.events[0].id, "new");

        let local = devices.join("mac-a.jsonl");
        let mut legacy = ledger_record("mac-a", "legacy", 1);
        legacy.schema_version = 1;
        fs::write(
            &local,
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .unwrap();
        let writer = store(&sync, "mac-a").with_cache(&cache);
        writer.synchronize(&[], false, None, || false);
        writer.synchronize(
            &[event("mac-a", "current", 2, "/local/current.jsonl")],
            false,
            None,
            || false,
        );
        let records = read_ledger(&local, None).unwrap().records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].schema_version, crate::SYNC_SCHEMA_VERSION);
        assert_eq!(records[0].event_id, "current");
        let cached = writer.cached_outcome(None).unwrap();
        assert!(cached.events.iter().any(|event| event.id == "current"));
        assert!(!cached.events.iter().any(|event| event.id == "legacy"));
    }

    #[test]
    fn invalid_or_cancelled_tail_keeps_last_good_cache_and_completed_removal_prunes() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let devices = sync.join("devices");
        fs::create_dir_all(&devices).unwrap();
        let first = devices.join("a.jsonl");
        let removed = devices.join("b.jsonl");
        rewrite_local_ledger_v2(&first, [ledger_record("a", "old", 1)]).unwrap();
        rewrite_local_ledger_v2(&removed, [ledger_record("b", "removed", 1)]).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let store = store(&sync, "reader").with_cache(&cache);
        store.synchronize(&[], false, None, || false);
        let first_key = first.canonicalize().unwrap().to_string_lossy().into_owned();
        let removed_key = removed
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let last_good = cache
            .origin_file(OriginKind::SyncLedger, &first_key)
            .unwrap()
            .unwrap();

        writeln!(
            fs::OpenOptions::new().append(true).open(&first).unwrap(),
            "bad"
        )
        .unwrap();
        let invalid = store.synchronize(&[], false, None, || false);
        assert_eq!(invalid.status.parse_error_count, 1);
        assert_eq!(
            cache
                .origin_file(OriginKind::SyncLedger, &first_key)
                .unwrap()
                .unwrap(),
            last_good
        );

        let mut file = fs::OpenOptions::new().append(true).open(&first).unwrap();
        serde_json::to_writer(&mut file, &ledger_record("a", "tail", 2)).unwrap();
        writeln!(file).unwrap();
        let checks = Cell::new(0);
        assert!(matches!(
            reconcile_cached_ledger(&first, &cache, &mut || {
                checks.set(checks.get() + 1);
                checks.get() >= 2
            }),
            Err(crate::sync::SyncError::Cancelled)
        ));
        assert_eq!(
            cache
                .origin_file(OriginKind::SyncLedger, &first_key)
                .unwrap()
                .unwrap(),
            last_good
        );

        fs::remove_file(removed).unwrap();
        store.synchronize(&[], false, None, || false);
        assert!(
            cache
                .origin_file(OriginKind::SyncLedger, &removed_key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn full_parse_error_reuses_valid_records_without_reading_the_ledger_twice() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        fs::create_dir(&sync).unwrap();
        let path = sync.join("devices/remote.jsonl");
        rewrite_local_ledger_v2(&path, [ledger_record("remote", &"old".repeat(100), 1)]).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        store(&sync, "reader")
            .with_cache(&cache)
            .synchronize(&[], false, None, || false);
        fs::write(
            &path,
            format!(
                "{}\nbad\n",
                serde_json::to_string(&ledger_record("remote", "visible", 2)).unwrap()
            ),
        )
        .unwrap();
        let checks = Cell::new(0);

        let read = read_device_ledgers(&sync, Some(&cache), None, &mut || {
            checks.set(checks.get() + 1);
            checks.get() >= 8
        });

        assert_eq!(read.parse_error_count, 1);
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].id, "visible");
        assert!(checks.get() < 8);
    }

    #[cfg(unix)]
    #[test]
    fn cached_outcome_rejects_a_symlinked_sync_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("actual");
        fs::create_dir(&actual).unwrap();
        rewrite_local_ledger_v2(
            &actual.join("devices/remote.jsonl"),
            [ledger_record("remote", "event", 1)],
        )
        .unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        store(&actual, "reader")
            .with_cache(&cache)
            .synchronize(&[], false, None, || false);
        let linked = directory.path().join("linked");
        symlink(&actual, &linked).unwrap();

        assert!(
            store(&linked, "reader")
                .with_cache(&cache)
                .cached_outcome(None)
                .is_none()
        );
    }

    #[test]
    fn failed_writer_or_cache_invalidation_never_serves_a_stale_local_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        fs::create_dir(&sync).unwrap();
        let cache_db_path = directory.path().join("cache.sqlite");
        let cache = TokenEventCache::open_or_create(&cache_db_path).unwrap();
        let writer = store(&sync, "mac-a").with_cache(&cache);
        rewrite_local_ledger_v2(
            &writer.local_ledger_path(),
            [ledger_record("mac-a", "old", 1)],
        )
        .unwrap();
        writer.synchronize(&[], false, None, || false);
        let origin_path = writer
            .local_ledger_path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let last_good = cache
            .origin_file(OriginKind::SyncLedger, &origin_path)
            .unwrap()
            .unwrap();
        let checks = Cell::new(0);
        let mut cache_usable = true;
        let cancelled = writer.write_local_ledger(
            &[event("mac-a", "cancelled", 2, "/cancelled.jsonl")],
            true,
            &mut cache_usable,
            &mut || {
                checks.set(checks.get() + 1);
                checks.get() >= 3
            },
        );
        assert!(matches!(cancelled, Err(crate::sync::SyncError::Cancelled)));
        assert!(cache_usable);
        assert_eq!(
            cache
                .origin_file(OriginKind::SyncLedger, &origin_path)
                .unwrap()
                .unwrap(),
            last_good
        );

        let blocker = rusqlite::Connection::open(&cache_db_path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let failed = writer.write_local_ledger(
            &[event("mac-a", "replacement", 2, "/replacement.jsonl")],
            true,
            &mut cache_usable,
            &mut || false,
        );
        assert!(failed.is_err());
        assert!(!cache_usable);
        assert!(writer.cached_outcome(None).is_none());
        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            read_ledger(&writer.local_ledger_path(), None)
                .unwrap()
                .records[0]
                .event_id,
            "replacement"
        );

        let recovered = writer.synchronize(&[], false, None, || false);
        assert_eq!(recovered.events.len(), 1);
        assert_eq!(recovered.events[0].id, "replacement");
        assert_ne!(
            cache
                .origin_file(OriginKind::SyncLedger, &origin_path)
                .unwrap()
                .unwrap(),
            last_good
        );
    }
}
