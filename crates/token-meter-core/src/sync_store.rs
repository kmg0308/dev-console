use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::models::{SyncFolderStatus, TokenDeviceMetadata, TokenEvent};
use crate::sync::{SyncLedgerRecord, read_ledger, safe_device_file_name, write_local_ledger_v2};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
    pub events: Vec<TokenEvent>,
    pub status: SyncFolderStatus,
}

pub struct TokenSyncStore {
    folder: PathBuf,
    local_device: TokenDeviceMetadata,
}

impl TokenSyncStore {
    pub fn new(folder: impl Into<PathBuf>, local_device: TokenDeviceMetadata) -> Self {
        Self {
            folder: folder.into(),
            local_device,
        }
    }

    pub fn local_ledger_path(&self) -> PathBuf {
        self.folder.join("devices").join(format!(
            "{}.jsonl",
            safe_device_file_name(&self.local_device.id)
        ))
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

        match self.write_local_ledger(local_events, replace_local_ledger, &mut is_cancelled) {
            Ok(count) => status.exported_event_count = count,
            Err(error) => status.export_error = Some(error.to_string()),
        }
        if is_cancelled() {
            return SyncOutcome {
                events: Vec::new(),
                status,
            };
        }

        let read = read_device_ledgers(&self.folder, imported_after, &mut is_cancelled);
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
        if records.is_empty() || is_cancelled() {
            return Ok(0);
        }

        write_local_ledger_v2(
            &self.local_ledger_path(),
            records,
            replace_local_ledger,
            is_cancelled,
        )
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
    imported_after: Option<DateTime<Utc>>,
    is_cancelled: &mut impl FnMut() -> bool,
) -> DeviceLedgerRead {
    let paths = match direct_ledger_paths(&folder.join("devices")) {
        Ok(paths) => paths,
        Err(_) => {
            return DeviceLedgerRead {
                parse_error_count: 1,
                ..DeviceLedgerRead::default()
            };
        }
    };
    let mut read = DeviceLedgerRead::default();
    let mut records = Vec::new();
    for path in paths {
        if is_cancelled() {
            break;
        }
        read.device_file_count += 1;
        match read_ledger(&path, None) {
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

fn direct_ledger_paths(devices: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(devices) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            !entry.file_name().to_string_lossy().starts_with('.')
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
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

    use chrono::TimeZone;

    use super::*;
    use crate::models::{TokenSource, TokenUsage};
    use crate::sync::rewrite_local_ledger_v2;

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

    fn store(folder: &Path, device: &str) -> TokenSyncStore {
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
}
