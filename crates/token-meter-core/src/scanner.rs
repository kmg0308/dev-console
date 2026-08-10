use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Utc};

use crate::{
    cache::{CachedFile, FileSnapshot, TokenEventCache},
    hermes::{HermesScanOutcome, HermesScanner},
    models::{ScanResult, ScanSourceStatus, TokenDeviceMetadata, TokenEvent, TokenSource},
    parser::{ParseError, parse_claude_file, parse_codex_file},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScannerRoots {
    pub codex_sessions: Option<PathBuf>,
    pub codex_archive: Option<PathBuf>,
    pub claude_projects: Option<PathBuf>,
    pub hermes_database: Option<PathBuf>,
}

impl ScannerRoots {
    pub fn new(
        codex_sessions: impl Into<PathBuf>,
        codex_archive: impl Into<PathBuf>,
        claude_projects: impl Into<PathBuf>,
        hermes_database: impl Into<PathBuf>,
    ) -> Self {
        Self {
            codex_sessions: Some(codex_sessions.into()),
            codex_archive: Some(codex_archive.into()),
            claude_projects: Some(claude_projects.into()),
            hermes_database: Some(hermes_database.into()),
        }
    }
}

pub struct TokenLogScanner<'a> {
    roots: ScannerRoots,
    local_device: TokenDeviceMetadata,
    cache: Option<&'a TokenEventCache>,
}

impl<'a> TokenLogScanner<'a> {
    pub fn new(
        roots: ScannerRoots,
        local_device: TokenDeviceMetadata,
        cache: Option<&'a TokenEventCache>,
    ) -> Self {
        Self {
            roots,
            local_device,
            cache,
        }
    }

    pub fn scan(
        &self,
        modified_after: Option<DateTime<Utc>>,
        event_after: Option<DateTime<Utc>>,
        is_cancelled: impl Fn() -> bool,
    ) -> ScanResult {
        if is_cancelled() {
            return ScanResult::default();
        }
        let required_paths = self
            .cache
            .and_then(|cache| cache.local_log_paths_requiring_rebuild().ok())
            .unwrap_or_default()
            .into_iter()
            .collect::<HashSet<_>>();
        let mut roots = Vec::new();
        let mut enumeration_completed = true;
        for root in self.local_roots() {
            if is_cancelled() {
                return ScanResult::default();
            }
            let mut enumeration = root
                .path
                .as_deref()
                .map_or_else(Enumeration::empty, |path| {
                    enumerate_jsonl(path, root.source, &is_cancelled)
                });
            for file in &mut enumeration.files {
                file.snapshot.device_id = Some(self.local_device.id.clone());
            }
            enumeration_completed &= enumeration.completed;
            let selected_files = enumeration
                .files
                .iter()
                .filter(|file| {
                    required_paths.contains(&file.snapshot.path)
                        || modified_after.is_none_or(|cutoff| file.modified_at >= cutoff)
                })
                .cloned()
                .collect();
            roots.push(RootScan {
                root,
                files: enumeration.files,
                selected_files,
                parse_error_count: enumeration.error_count,
            });
        }
        if is_cancelled() {
            return ScanResult::default();
        }
        if enumeration_completed && let Some(cache) = self.cache {
            let keeping = roots
                .iter()
                .flat_map(|root| root.files.iter().map(|file| file.snapshot.path.clone()))
                .collect();
            let pruning_sources = if self.roots.claude_projects.is_some() {
                HashSet::from([TokenSource::Claude])
            } else {
                HashSet::new()
            };
            let _ = cache.remove_missing_local_origins(&keeping, &pruning_sources);
        }

        let mut fresh_events = Vec::new();
        for root in &mut roots {
            for file in &root.selected_files {
                if is_cancelled() {
                    return ScanResult::default();
                }
                match self.cached_or_parsed_events(file, &is_cancelled) {
                    Ok(events) => fresh_events.extend(events),
                    Err(()) => root.parse_error_count += 1,
                }
                if is_cancelled() {
                    return ScanResult::default();
                }
            }
        }

        let hermes =
            self.roots
                .hermes_database
                .as_ref()
                .map_or_else(HermesScanOutcome::default, |path| {
                    HermesScanner::new(path, self.local_device.clone(), self.cache)
                        .scan(&is_cancelled)
                });
        if is_cancelled() {
            return ScanResult::default();
        }
        fresh_events.extend(hermes.events.iter().cloned());
        let cached_events = self
            .cache
            .and_then(|cache| cache.events(event_after.or(modified_after)).ok())
            .unwrap_or_default();
        let events = deduplicated(cached_events.into_iter().chain(fresh_events));
        self.make_result(roots, hermes, events, event_after.or(modified_after))
    }

    pub fn cached_result(&self, event_after: Option<DateTime<Utc>>) -> Option<ScanResult> {
        let cache = self.cache?;
        let events = cache.events(event_after).ok()?;
        if events.is_empty() {
            return None;
        }
        let origins = cache.local_log_origins(event_after).unwrap_or_default();
        let mut source_statuses = self
            .local_roots()
            .into_iter()
            .map(|root| {
                let count = origins
                    .iter()
                    .filter(|(path, source)| {
                        *source == root.source
                            && root
                                .path
                                .as_deref()
                                .is_some_and(|root| path_is_under(path, root))
                    })
                    .count();
                ScanSourceStatus {
                    source: root.source,
                    label: root.label.into(),
                    path: root
                        .path
                        .as_ref()
                        .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                    exists: root.path.as_deref().is_some_and(Path::exists),
                    total_file_count: count,
                    scanned_file_count: count,
                    parse_error_count: 0,
                }
            })
            .collect::<Vec<_>>();
        let has_hermes = events
            .iter()
            .any(|event| event.raw_file_path.starts_with("hermes://"));
        let hermes_exists = self
            .roots
            .hermes_database
            .as_deref()
            .is_some_and(Path::is_file);
        source_statuses.push(ScanSourceStatus {
            source: TokenSource::Codex,
            label: "Hermes Agent".into(),
            path: self
                .roots
                .hermes_database
                .as_ref()
                .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
            exists: hermes_exists,
            total_file_count: usize::from(hermes_exists),
            scanned_file_count: usize::from(self.roots.hermes_database.is_some() && has_hermes),
            parse_error_count: 0,
        });
        Some(result_from(events, source_statuses))
    }

    pub fn find_codex_files(&self) -> Vec<PathBuf> {
        [
            self.roots.codex_sessions.as_deref(),
            self.roots.codex_archive.as_deref(),
        ]
        .into_iter()
        .flatten()
        .flat_map(|root| enumerate_jsonl(root, TokenSource::Codex, &|| false).files)
        .map(|file| file.path)
        .collect()
    }

    pub fn find_claude_files(&self) -> Vec<PathBuf> {
        self.roots
            .claude_projects
            .as_deref()
            .map(|root| enumerate_jsonl(root, TokenSource::Claude, &|| false).files)
            .unwrap_or_default()
            .into_iter()
            .map(|file| file.path)
            .collect()
    }

    pub fn clear_cache(&self) -> Result<(), crate::cache::CacheError> {
        if let Some(cache) = self.cache {
            cache.clear()?;
        }
        Ok(())
    }

    fn cached_or_parsed_events(
        &self,
        file: &LogFile,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<TokenEvent>, ()> {
        if let Some(cache) = self.cache
            && let Ok(Some(cached)) = cache.cached_file(&file.snapshot)
        {
            return match cached {
                CachedFile::Events(events) => Ok(events),
                CachedFile::ParseError => Err(()),
            };
        }
        if let Some(events) = self.append_cached_events(file, is_cancelled) {
            return events;
        }
        let parsed = self.parse(file, 0, is_cancelled);
        if is_cancelled() {
            return Ok(Vec::new());
        }
        match parsed {
            Ok(events) => {
                let events = events
                    .into_iter()
                    .map(|event| event.with_device(&self.local_device))
                    .collect::<Vec<_>>();
                if let Some(cache) = self.cache {
                    let _ = cache.replace_local_events(&file.snapshot, &events, false);
                }
                Ok(events)
            }
            Err(_) => {
                if let Some(cache) = self.cache {
                    let _ = cache.replace_local_events(&file.snapshot, &[], true);
                }
                Err(())
            }
        }
    }

    fn append_cached_events(
        &self,
        file: &LogFile,
        is_cancelled: &impl Fn() -> bool,
    ) -> Option<Result<Vec<TokenEvent>, ()>> {
        let cache = self.cache?;
        let base = cache.incremental_append_base(&file.snapshot).ok()??;
        let base_snapshot = FileSnapshot {
            size: base.size,
            modified_at: base.modified_at,
            ..file.snapshot.clone()
        };
        let CachedFile::Events(cached_events) = cache.cached_file(&base_snapshot).ok()?? else {
            return None;
        };
        let new_events = match self.parse(file, base.size as u64, is_cancelled) {
            Ok(events) => events,
            Err(ParseError::RequiresFullFile) | Err(ParseError::Read { .. }) => return None,
        };
        if is_cancelled() {
            return Some(Ok(cached_events));
        }
        let existing = cached_events
            .iter()
            .map(|event| (event.device_id.clone(), event.id.clone()))
            .collect::<HashSet<_>>();
        let new_events = new_events
            .into_iter()
            .map(|event| event.with_device(&self.local_device))
            .filter(|event| !existing.contains(&(event.device_id.clone(), event.id.clone())))
            .collect::<Vec<_>>();
        if cache
            .append_local_events(&file.snapshot, &new_events)
            .is_err()
        {
            return None;
        }
        Some(Ok(cached_events.into_iter().chain(new_events).collect()))
    }

    fn parse(
        &self,
        file: &LogFile,
        offset: u64,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<TokenEvent>, ParseError> {
        match file.source {
            TokenSource::Codex => parse_codex_file(&file.path, offset, is_cancelled),
            TokenSource::Claude => parse_claude_file(&file.path, offset, is_cancelled),
            TokenSource::All => Ok(Vec::new()),
        }
    }

    fn make_result(
        &self,
        roots: Vec<RootScan>,
        hermes: HermesScanOutcome,
        events: Vec<TokenEvent>,
        event_after: Option<DateTime<Utc>>,
    ) -> ScanResult {
        let cached_origins = self
            .cache
            .and_then(|cache| cache.local_log_origins(event_after).ok())
            .unwrap_or_default();
        let mut source_statuses = roots
            .into_iter()
            .map(|root| {
                let cached_count = cached_origins
                    .iter()
                    .filter(|(path, source)| {
                        *source == root.root.source
                            && root
                                .root
                                .path
                                .as_deref()
                                .is_some_and(|root| path_is_under(path, root))
                    })
                    .count();
                ScanSourceStatus {
                    source: root.root.source,
                    label: root.root.label.into(),
                    path: root
                        .root
                        .path
                        .as_ref()
                        .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                    exists: root.root.path.as_deref().is_some_and(Path::exists),
                    total_file_count: root.files.len().max(cached_count),
                    scanned_file_count: root.selected_files.len().max(cached_count),
                    parse_error_count: root.parse_error_count,
                }
            })
            .collect::<Vec<_>>();
        source_statuses.push(ScanSourceStatus {
            source: TokenSource::Codex,
            label: "Hermes Agent".into(),
            path: self
                .roots
                .hermes_database
                .as_ref()
                .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
            exists: hermes.database_exists,
            total_file_count: usize::from(hermes.database_exists),
            scanned_file_count: usize::from(
                hermes.database_exists && hermes.parse_error_count == 0,
            ),
            parse_error_count: hermes.parse_error_count,
        });
        result_from(events, source_statuses)
    }

    fn local_roots(&self) -> [LogRoot; 3] {
        [
            LogRoot {
                source: TokenSource::Codex,
                label: "Codex sessions",
                path: self.roots.codex_sessions.clone(),
            },
            LogRoot {
                source: TokenSource::Codex,
                label: "Codex archive",
                path: self.roots.codex_archive.clone(),
            },
            LogRoot {
                source: TokenSource::Claude,
                label: "Claude projects",
                path: self.roots.claude_projects.clone(),
            },
        ]
    }
}

fn result_from(events: Vec<TokenEvent>, source_statuses: Vec<ScanSourceStatus>) -> ScanResult {
    ScanResult {
        codex_file_count: source_statuses
            .iter()
            .filter(|status| status.source == TokenSource::Codex)
            .map(|status| status.scanned_file_count)
            .sum(),
        claude_file_count: source_statuses
            .iter()
            .filter(|status| status.source == TokenSource::Claude)
            .map(|status| status.scanned_file_count)
            .sum(),
        parse_error_count: source_statuses
            .iter()
            .map(|status| status.parse_error_count)
            .sum(),
        events,
        source_statuses,
        ..ScanResult::default()
    }
}

fn deduplicated(events: impl IntoIterator<Item = TokenEvent>) -> Vec<TokenEvent> {
    let mut by_key = HashMap::new();
    for event in events {
        let key = (event.device_id.clone(), event.id.clone());
        match by_key.get(&key) {
            Some(existing) if has_local_details(existing) && !has_local_details(&event) => {}
            _ => {
                by_key.insert(key, event);
            }
        }
    }
    let mut events = by_key.into_values().collect::<Vec<_>>();
    events.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then(left.id.cmp(&right.id))
    });
    events
}

fn has_local_details(event: &TokenEvent) -> bool {
    !event.raw_file_path.starts_with("sync://")
}

fn path_is_under(path: &str, root: &Path) -> bool {
    Path::new(path).starts_with(canonical_or_owned(root))
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

#[derive(Clone)]
struct LogRoot {
    source: TokenSource,
    label: &'static str,
    path: Option<PathBuf>,
}

struct RootScan {
    root: LogRoot,
    files: Vec<LogFile>,
    selected_files: Vec<LogFile>,
    parse_error_count: usize,
}

#[derive(Clone)]
struct LogFile {
    path: PathBuf,
    source: TokenSource,
    modified_at: DateTime<Utc>,
    snapshot: FileSnapshot,
}

struct Enumeration {
    files: Vec<LogFile>,
    completed: bool,
    error_count: usize,
}

impl Enumeration {
    fn empty() -> Self {
        Self {
            files: Vec::new(),
            completed: true,
            error_count: 0,
        }
    }
}

fn enumerate_jsonl(
    root: &Path,
    source: TokenSource,
    is_cancelled: &impl Fn() -> bool,
) -> Enumeration {
    match fs::metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Enumeration {
                files: Vec::new(),
                completed: true,
                error_count: 0,
            };
        }
        Err(_) => {
            return Enumeration {
                files: Vec::new(),
                completed: false,
                error_count: 1,
            };
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Enumeration {
                files: Vec::new(),
                completed: false,
                error_count: 1,
            };
        }
        Ok(_) => {}
    }
    let mut paths = vec![root.to_owned()];
    let mut files = Vec::new();
    let mut completed = true;
    let mut error_count = 0;
    while let Some(directory) = paths.pop() {
        if is_cancelled() {
            return Enumeration {
                files,
                completed: false,
                error_count: 0,
            };
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(_) => {
                return Enumeration {
                    files,
                    completed: false,
                    error_count: 1,
                };
            }
        };
        for entry in entries {
            if is_cancelled() {
                return Enumeration {
                    files,
                    completed: false,
                    error_count: 0,
                };
            }
            let Ok(entry) = entry else {
                return Enumeration {
                    files,
                    completed: false,
                    error_count: 1,
                };
            };
            if is_hidden(&entry) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                return Enumeration {
                    files,
                    completed: false,
                    error_count: 1,
                };
            };
            if file_type.is_dir() {
                paths.push(entry.path());
                continue;
            }
            if !file_type.is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = fs::metadata(&path) else {
                completed = false;
                error_count += 1;
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                completed = false;
                error_count += 1;
                continue;
            };
            let modified_at: DateTime<Utc> = modified.into();
            files.push(LogFile {
                snapshot: FileSnapshot {
                    path: canonical_or_owned(&path).to_string_lossy().into_owned(),
                    source: Some(source),
                    size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
                    modified_at: modified
                        .duration_since(UNIX_EPOCH)
                        .map_or(0.0, |duration| duration.as_secs_f64()),
                    device_id: None,
                },
                path,
                source,
                modified_at,
            });
        }
    }
    files.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then(left.path.cmp(&right.path))
    });
    Enumeration {
        files,
        completed,
        error_count,
    }
}

fn is_hidden(entry: &fs::DirEntry) -> bool {
    if entry.file_name().to_string_lossy().starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        entry
            .metadata()
            .is_ok_and(|metadata| metadata.file_attributes() & 0x2 != 0)
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Write};

    use tempfile::tempdir;

    use super::*;

    fn roots(base: &Path) -> ScannerRoots {
        ScannerRoots::new(
            base.join("codex/sessions"),
            base.join("codex/archive"),
            base.join("claude/projects"),
            base.join("hermes/state.db"),
        )
    }

    fn configured(root: &Option<PathBuf>) -> &Path {
        root.as_deref().unwrap()
    }

    fn write_claude(path: &Path, request: &str, input: i64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "sessionId": "s",
            "requestId": request,
            "cwd": "/tmp/p",
            "message": {"model": "claude", "usage": {"input_tokens": input}},
        });
        fs::write(path, format!("{value}\n")).unwrap();
    }

    fn write_codex(path: &Path, input: i64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({
            "timestamp": "2026-01-01T00:00:00.000Z",
            "payload": {
                "cwd": "/tmp/p",
                "model": "gpt",
                "info": {
                    "last_token_usage": {"input_tokens": input, "total_tokens": input},
                    "total_token_usage": {"input_tokens": input, "total_tokens": input},
                },
            },
        });
        fs::write(path, format!("{value}\n")).unwrap();
    }

    #[test]
    fn recursively_scans_files_but_not_jsonl_directories() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        for index in 0..45 {
            write_claude(
                &configured(&roots.claude_projects).join(format!("nested/{index}.jsonl")),
                &index.to_string(),
                1,
            );
        }
        fs::create_dir_all(configured(&roots.claude_projects).join("ignored.jsonl")).unwrap();
        let scanner = TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), None);
        let result = scanner.scan(None, None, || false);
        assert_eq!(result.claude_file_count, 45);
        assert_eq!(result.events.len(), 45);
        assert_eq!(result.parse_error_count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn root_metadata_failure_is_an_incomplete_scan() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let root = directory.path().join("loop");
        symlink(&root, &root).unwrap();

        let enumeration = enumerate_jsonl(&root, TokenSource::Codex, &|| false);

        assert!(!enumeration.completed);
        assert_eq!(enumeration.error_count, 1);
        assert!(enumeration.files.is_empty());
    }

    #[test]
    fn cache_observes_growth_with_an_open_writer_and_deduplicates_requests() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        let path = configured(&roots.claude_projects).join("one.jsonl");
        write_claude(&path, "a", 10);
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner =
            TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), Some(&cache));
        assert_eq!(scanner.scan(None, None, || false).events[0].usage.total, 10);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{\"timestamp\":\"2026-01-01T00:01:00.000Z\",\"sessionId\":\"s\",\"requestId\":\"a\",\"message\":{{\"usage\":{{\"input_tokens\":90}}}}}}").unwrap();
        writeln!(file, "{{\"timestamp\":\"2026-01-01T00:02:00.000Z\",\"sessionId\":\"s\",\"requestId\":\"b\",\"message\":{{\"usage\":{{\"input_tokens\":25}}}}}}").unwrap();
        let result = scanner.scan(None, None, || false);
        assert_eq!(result.events.len(), 2);
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.usage.total)
                .sum::<i64>(),
            35
        );
    }

    #[test]
    fn full_parse_replaces_growth_when_mtime_regresses() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        let path = configured(&roots.claude_projects).join("one.jsonl");
        write_claude(&path, "old", 10);
        let future = std::fs::FileTimes::new()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000));
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(future)
            .unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner =
            TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), Some(&cache));
        scanner.scan(None, None, || false);
        write_claude(&path, "new-a", 20);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{\"timestamp\":\"2026-01-01T00:01:00.000Z\",\"requestId\":\"new-b\",\"message\":{{\"usage\":{{\"input_tokens\":30}}}}}}").unwrap();
        let result = scanner.scan(None, None, || false);
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.usage.total)
                .sum::<i64>(),
            50
        );
    }

    #[test]
    fn preserves_missing_codex_and_prunes_missing_claude() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        let codex = configured(&roots.codex_sessions).join("one.jsonl");
        let claude = configured(&roots.claude_projects).join("one.jsonl");
        write_codex(&codex, 10);
        write_claude(&claude, "one", 20);
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner =
            TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), Some(&cache));
        assert_eq!(scanner.scan(None, None, || false).events.len(), 2);
        fs::remove_file(codex).unwrap();
        fs::remove_file(claude).unwrap();
        let result = scanner.scan(None, None, || false);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].source, TokenSource::Codex);
    }

    #[test]
    fn cancellation_during_parse_writes_no_partial_cache() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        let path = configured(&roots.claude_projects).join("large.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents = (0..2_000)
            .map(|index| format!("{{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"requestId\":\"{index}\",\"message\":{{\"usage\":{{\"input_tokens\":1}}}}}}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, contents).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner =
            TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), Some(&cache));
        let checks = Cell::new(0);
        let result = scanner.scan(None, None, || {
            checks.set(checks.get() + 1);
            checks.get() >= 12
        });
        assert!(result.events.is_empty());
        assert!(cache.events(None).unwrap().is_empty());
    }

    #[test]
    fn cancelled_enumeration_does_not_prune_existing_cache() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        let removed = configured(&roots.claude_projects).join("removed.jsonl");
        let kept = configured(&roots.claude_projects).join("kept.jsonl");
        write_claude(&removed, "removed", 10);
        write_claude(&kept, "kept", 20);
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner =
            TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), Some(&cache));
        assert_eq!(scanner.scan(None, None, || false).events.len(), 2);
        fs::remove_file(removed).unwrap();

        let checks = Cell::new(0);
        scanner.scan(None, None, || {
            checks.set(checks.get() + 1);
            checks.get() >= 6
        });
        assert_eq!(scanner.cached_result(None).unwrap().events.len(), 2);
    }

    #[test]
    fn parser_version_rebuilds_existing_file_outside_refresh_window_once() {
        let directory = tempdir().unwrap();
        let roots = roots(directory.path());
        let path = configured(&roots.codex_sessions).join("one.jsonl");
        write_codex(&path, 10);
        let cache_path = directory.path().join("cache.sqlite");
        let cache = TokenEventCache::open_or_create(&cache_path).unwrap();
        let scanner =
            TokenLogScanner::new(roots, TokenDeviceMetadata::local_fallback(), Some(&cache));
        scanner.scan(None, None, || false);
        rusqlite::Connection::open(&cache_path)
            .unwrap()
            .execute("UPDATE origin_files SET parser_version = 3", [])
            .unwrap();
        write_codex(&path, 20);
        let future = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        let rebuilt = scanner.scan(Some(future), Some(future), || false);
        assert_eq!(rebuilt.events[0].usage.total, 20);
        fs::remove_file(path).unwrap();
        assert!(
            scanner
                .scan(Some(future), Some(future), || false)
                .events
                .is_empty()
        );
    }

    #[test]
    fn unconfigured_roots_do_not_probe_or_prune_cached_history() {
        let directory = tempdir().unwrap();
        let configured_roots = roots(directory.path());
        let claude = configured(&configured_roots.claude_projects).join("one.jsonl");
        write_claude(&claude, "one", 10);
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        TokenLogScanner::new(
            configured_roots,
            TokenDeviceMetadata::local_fallback(),
            Some(&cache),
        )
        .scan(None, None, || false);

        let scanner = TokenLogScanner::new(
            ScannerRoots::default(),
            TokenDeviceMetadata::local_fallback(),
            Some(&cache),
        );
        let result = scanner.scan(None, None, || false);
        assert_eq!(result.events.len(), 1);
        assert!(result.source_statuses.iter().all(|status| {
            status.path.is_empty()
                && !status.exists
                && status.total_file_count == 0
                && status.scanned_file_count == 0
        }));
        assert!(scanner.find_codex_files().is_empty());
        assert!(scanner.find_claude_files().is_empty());
        assert_eq!(cache.events(None).unwrap().len(), 1);
    }
}
