use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionSnapshot {
    /// Canonical path relative to the canonical Codex home, after resolving symlinks.
    pub relative_path: PathBuf,
    pub size: u64,
    pub modified_at_unix_nanos: i64,
    pub content_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionEvidence {
    pub snapshot: CodexSessionSnapshot,
    /// `None` means the cache has no verified record for this file.
    pub cached_event_keys: Option<BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionCleanupFile {
    pub snapshot: CodexSessionSnapshot,
    pub event_count: usize,
    #[serde(skip)]
    cached_event_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSessionCleanupPlan {
    pub retention_days: u32,
    pub scanned_file_count: usize,
    eligible_files: Vec<CodexSessionCleanupFile>,
    pub unsafe_file_count: usize,
    pub uncached_file_count: usize,
    pub sync_ledger_event_count: usize,
    pub created_at: DateTime<Utc>,
}

impl CodexSessionCleanupPlan {
    pub fn eligible_files(&self) -> &[CodexSessionCleanupFile] {
        &self.eligible_files
    }

    pub fn eligible_file_count(&self) -> usize {
        self.eligible_files.len()
    }

    pub fn can_apply(&self) -> bool {
        !self.eligible_files.is_empty()
    }

    pub fn eligible_byte_count(&self) -> u64 {
        self.eligible_files
            .iter()
            .fold(0, |total, file| total.saturating_add(file.snapshot.size))
    }

    pub fn eligible_event_count(&self) -> usize {
        self.eligible_files
            .iter()
            .map(|file| file.event_count)
            .sum()
    }

    /// Revalidates the plan immediately before an archive is created.
    pub fn archive_request(
        &self,
        current: &[CodexSessionEvidence],
        sync_ledger_keys: &BTreeSet<String>,
    ) -> Result<ArchiveRequest, CleanupError> {
        self.validate_current(current, sync_ledger_keys)?;
        Ok(ArchiveRequest {
            entries: self
                .eligible_files
                .iter()
                .map(|file| CleanupArchiveEntry::from_snapshot(&file.snapshot))
                .collect(),
        })
    }

    /// Returns removal permission only after the caller has created and inspected the archive.
    pub fn authorize_removal(
        &self,
        current: &[CodexSessionEvidence],
        sync_ledger_keys: &BTreeSet<String>,
        archive: &ArchiveInspection,
    ) -> Result<RemovalAuthorization, CleanupError> {
        self.validate_current(current, sync_ledger_keys)?;

        let mut expected = self
            .eligible_files
            .iter()
            .map(|file| CleanupArchiveEntry::from_snapshot(&file.snapshot))
            .collect::<Vec<_>>();
        let mut archived = archive.entries.clone();
        expected.sort_unstable();
        archived.sort_unstable();
        if archive.entries.len() != expected.len() {
            return Err(CleanupError::ArchiveCountMismatch);
        }
        if archived != expected {
            return Err(CleanupError::ArchiveEntriesMismatch);
        }

        Ok(RemovalAuthorization {
            entries: expected,
            byte_count: self.eligible_byte_count(),
            archive_sha256: archive.archive_sha256.clone(),
        })
    }

    fn validate_current(
        &self,
        current: &[CodexSessionEvidence],
        sync_ledger_keys: &BTreeSet<String>,
    ) -> Result<(), CleanupError> {
        if self.eligible_files.is_empty() {
            return Err(CleanupError::NoEligibleFiles);
        }

        let mut by_path = BTreeMap::new();
        for evidence in current {
            if by_path
                .insert(&evidence.snapshot.relative_path, evidence)
                .is_some()
            {
                return Err(CleanupError::DuplicateEvidence(
                    evidence.snapshot.relative_path.clone(),
                ));
            }
        }

        for planned in &self.eligible_files {
            let path = &planned.snapshot.relative_path;
            let evidence = by_path
                .get(path)
                .ok_or_else(|| CleanupError::FileChanged(path.clone()))?;
            if evidence.snapshot != planned.snapshot {
                return Err(CleanupError::FileChanged(path.clone()));
            }
            let keys = evidence
                .cached_event_keys
                .as_ref()
                .ok_or_else(|| CleanupError::CacheValidationFailed(path.clone()))?;
            if keys != &planned.cached_event_keys {
                return Err(CleanupError::CacheValidationFailed(path.clone()));
            }
            if !keys.is_subset(sync_ledger_keys) {
                return Err(CleanupError::SyncValidationFailed(path.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRequest {
    entries: Vec<CleanupArchiveEntry>,
}

impl ArchiveRequest {
    pub fn entries(&self) -> &[CleanupArchiveEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CleanupArchiveEntry {
    relative_path: PathBuf,
    size: u64,
    content_sha256: String,
}

impl CleanupArchiveEntry {
    fn from_snapshot(snapshot: &CodexSessionSnapshot) -> Self {
        Self {
            relative_path: snapshot.relative_path.clone(),
            size: snapshot.size,
            content_sha256: snapshot.content_sha256.clone(),
        }
    }

    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub const fn size(&self) -> u64 {
        self.size
    }

    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    pub(crate) fn verified(relative_path: PathBuf, size: u64, content_sha256: String) -> Self {
        Self {
            relative_path,
            size,
            content_sha256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInspection {
    entries: Vec<CleanupArchiveEntry>,
    archive_sha256: String,
}

impl ArchiveInspection {
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn entries(&self) -> &[CleanupArchiveEntry] {
        &self.entries
    }

    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }

    pub(crate) fn verified(entries: Vec<CleanupArchiveEntry>, archive_sha256: String) -> Self {
        Self {
            entries,
            archive_sha256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalAuthorization {
    entries: Vec<CleanupArchiveEntry>,
    byte_count: u64,
    archive_sha256: String,
}

impl RemovalAuthorization {
    pub fn entries(&self) -> &[CleanupArchiveEntry] {
        &self.entries
    }

    pub const fn byte_count(&self) -> u64 {
        self.byte_count
    }

    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CleanupError {
    #[error("No verified Codex session files are ready to archive.")]
    NoEligibleFiles,
    #[error("Duplicate current evidence for {0:?}.")]
    DuplicateEvidence(PathBuf),
    #[error("Codex session changed before cleanup could finish: {0:?}.")]
    FileChanged(PathBuf),
    #[error("Codex session cache validation failed: {0:?}.")]
    CacheValidationFailed(PathBuf),
    #[error("Codex session sync validation failed: {0:?}.")]
    SyncValidationFailed(PathBuf),
    #[error("Archive entry count did not match the cleanup plan.")]
    ArchiveCountMismatch,
    #[error("Archive entries did not match the cleanup plan.")]
    ArchiveEntriesMismatch,
}

pub fn plan_codex_session_cleanup(
    candidates: &[CodexSessionEvidence],
    sync_ledger_keys: &BTreeSet<String>,
    retention_days: u32,
    created_at: DateTime<Utc>,
) -> CodexSessionCleanupPlan {
    let retention_days = retention_days.max(1);
    let cutoff = created_at
        .checked_sub_signed(Duration::days(i64::from(retention_days)))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut eligible_files = Vec::new();
    let mut unsafe_file_count = 0;
    let mut uncached_file_count = 0;
    let mut scanned_file_count = 0;
    let path_counts = candidates
        .iter()
        .fold(BTreeMap::new(), |mut counts, candidate| {
            *counts
                .entry(candidate.snapshot.relative_path.clone())
                .or_insert(0_usize) += 1;
            counts
        });

    for candidate in candidates {
        let modified_at =
            DateTime::<Utc>::from_timestamp_nanos(candidate.snapshot.modified_at_unix_nanos);
        if modified_at >= cutoff {
            continue;
        }
        scanned_file_count += 1;

        let path = &candidate.snapshot.relative_path;
        if !is_safe_codex_session_path(path) || path_counts.get(path) != Some(&1) {
            unsafe_file_count += 1;
            continue;
        }
        let Some(keys) = &candidate.cached_event_keys else {
            uncached_file_count += 1;
            continue;
        };
        if !keys.is_subset(sync_ledger_keys) {
            unsafe_file_count += 1;
            continue;
        }
        eligible_files.push(CodexSessionCleanupFile {
            snapshot: candidate.snapshot.clone(),
            event_count: keys.len(),
            cached_event_keys: keys.clone(),
        });
    }
    eligible_files.sort_unstable_by(|left, right| {
        left.snapshot
            .relative_path
            .cmp(&right.snapshot.relative_path)
    });

    CodexSessionCleanupPlan {
        retention_days,
        scanned_file_count,
        eligible_files,
        unsafe_file_count,
        uncached_file_count,
        sync_ledger_event_count: sync_ledger_keys.len(),
        created_at,
    }
}

fn is_safe_codex_session_path(path: &Path) -> bool {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return false;
    }
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(value)) if value == OsStr::new("sessions") || value == OsStr::new("archived_sessions"))
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    fn evidence(path: &str, modified_at: &str, keys: Option<&[&str]>) -> CodexSessionEvidence {
        CodexSessionEvidence {
            snapshot: CodexSessionSnapshot {
                relative_path: path.into(),
                size: 12,
                modified_at_unix_nanos: timestamp(modified_at).timestamp_nanos_opt().unwrap(),
                content_sha256: "00".repeat(32),
            },
            cached_event_keys: keys.map(|keys| keys.iter().map(|key| (*key).into()).collect()),
        }
    }

    #[test]
    fn plan_includes_only_old_cached_and_synced_sessions() {
        let synced = BTreeSet::from(["safe".into()]);
        let old = evidence(
            "sessions/2026/old.jsonl",
            "2026-01-01T00:00:00Z",
            Some(&["safe"]),
        );
        let candidates = [
            old.clone(),
            evidence(
                "sessions/2026/recent.jsonl",
                "2026-07-01T00:00:00Z",
                Some(&["safe"]),
            ),
            evidence(
                "sessions/2026/unsynced.jsonl",
                "2026-01-01T00:00:00Z",
                Some(&["missing"]),
            ),
            evidence("sessions/2026/uncached.jsonl", "2026-01-01T00:00:00Z", None),
            evidence("../outside.jsonl", "2026-01-01T00:00:00Z", Some(&["safe"])),
        ];
        let plan =
            plan_codex_session_cleanup(&candidates, &synced, 90, timestamp("2026-07-04T00:00:00Z"));

        assert_eq!(plan.scanned_file_count, 4);
        assert_eq!(plan.eligible_files().len(), 1);
        assert_eq!(plan.eligible_file_count(), 1);
        assert_eq!(plan.eligible_event_count(), 1);
        assert!(plan.can_apply());
        assert_eq!(plan.unsafe_file_count, 2);
        assert_eq!(plan.uncached_file_count, 1);
        assert_eq!(
            plan.archive_request(&[old], &synced)
                .unwrap()
                .entries()
                .len(),
            1
        );
    }

    #[test]
    fn removal_requires_current_proofs_and_exact_archive_contents() {
        let synced = BTreeSet::from(["safe".into()]);
        let original = evidence(
            "archived_sessions/old.jsonl",
            "2026-01-01T00:00:00Z",
            Some(&["safe"]),
        );
        let plan = plan_codex_session_cleanup(
            std::slice::from_ref(&original),
            &synced,
            90,
            timestamp("2026-07-04T00:00:00Z"),
        );

        let mut changed = original.clone();
        changed.snapshot.size += 1;
        assert!(matches!(
            plan.archive_request(&[changed], &synced),
            Err(CleanupError::FileChanged(_))
        ));
        assert!(matches!(
            plan.archive_request(std::slice::from_ref(&original), &BTreeSet::new()),
            Err(CleanupError::SyncValidationFailed(_))
        ));
        let mut cache_changed = original.clone();
        cache_changed.cached_event_keys = Some(BTreeSet::from(["other".into()]));
        assert!(matches!(
            plan.archive_request(&[cache_changed], &synced),
            Err(CleanupError::CacheValidationFailed(_))
        ));
        assert_eq!(
            plan.authorize_removal(
                std::slice::from_ref(&original),
                &synced,
                &ArchiveInspection::verified(vec![], "00".repeat(32))
            ),
            Err(CleanupError::ArchiveCountMismatch)
        );
        assert_eq!(
            plan.authorize_removal(
                std::slice::from_ref(&original),
                &synced,
                &ArchiveInspection::verified(
                    vec![CleanupArchiveEntry {
                        relative_path: "sessions/wrong.jsonl".into(),
                        size: original.snapshot.size,
                        content_sha256: original.snapshot.content_sha256.clone(),
                    }],
                    "00".repeat(32),
                )
            ),
            Err(CleanupError::ArchiveEntriesMismatch)
        );

        let authorized = plan
            .authorize_removal(
                std::slice::from_ref(&original),
                &synced,
                &ArchiveInspection::verified(
                    vec![CleanupArchiveEntry::from_snapshot(&original.snapshot)],
                    "00".repeat(32),
                ),
            )
            .unwrap();
        assert_eq!(authorized.entries().len(), 1);
        assert_eq!(authorized.byte_count(), 12);

        let wrong_content = ArchiveInspection::verified(
            vec![CleanupArchiveEntry {
                relative_path: original.snapshot.relative_path.clone(),
                size: original.snapshot.size,
                content_sha256: "11".repeat(32),
            }],
            "11".repeat(32),
        );
        assert_eq!(
            plan.authorize_removal(std::slice::from_ref(&original), &synced, &wrong_content),
            Err(CleanupError::ArchiveEntriesMismatch)
        );
    }

    #[test]
    fn cleanup_paths_are_relative_to_codex_home_only() {
        assert!(is_safe_codex_session_path(Path::new(
            "sessions/2026/session.jsonl"
        )));
        assert!(is_safe_codex_session_path(Path::new(
            "archived_sessions/session.jsonl"
        )));
        assert!(!is_safe_codex_session_path(Path::new(
            ".codex/sessions/session.jsonl"
        )));
        assert!(!is_safe_codex_session_path(Path::new(
            "session_archives/session.jsonl"
        )));
    }
}
