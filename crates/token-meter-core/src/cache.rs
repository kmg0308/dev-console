use std::path::Path;
use std::{collections::HashSet, fs};

use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::Value;
use thiserror::Error;

use crate::{
    CACHE_PARSER_VERSION,
    models::{TokenEvent, TokenSource, TokenUsage},
};

pub const MAX_FIXTURE_MIGRATION_BYTES: u64 = 16 * 1024 * 1024;

const REQUIRED_TABLES: &[(&str, &[ExpectedColumn])] = &[
    (
        "origin_files",
        &[
            ExpectedColumn::required_pk("origin_kind", "TEXT", 1),
            ExpectedColumn::required_pk("origin_path", "TEXT", 2),
            ExpectedColumn::optional("source", "TEXT"),
            ExpectedColumn::required("file_size", "INTEGER"),
            ExpectedColumn::required("modified_at", "REAL"),
            ExpectedColumn::required("parser_version", "INTEGER"),
            ExpectedColumn::optional("device_id", "TEXT"),
            ExpectedColumn::required("parse_error", "INTEGER"),
            ExpectedColumn::required("event_count", "INTEGER"),
            ExpectedColumn::optional("first_event_at", "REAL"),
            ExpectedColumn::optional("last_event_at", "REAL"),
            ExpectedColumn::required("scanned_at", "REAL"),
        ],
    ),
    (
        "event_records",
        &[
            ExpectedColumn::required_pk("origin_kind", "TEXT", 1),
            ExpectedColumn::required_pk("origin_path", "TEXT", 2),
            ExpectedColumn::required_pk("device_id", "TEXT", 3),
            ExpectedColumn::required_pk("event_id", "TEXT", 4),
            ExpectedColumn::required("timestamp", "REAL"),
            ExpectedColumn::required("source", "TEXT"),
            ExpectedColumn::required("priority", "INTEGER"),
            ExpectedColumn::required("event_json", "BLOB"),
        ],
    ),
    (
        "hermes_checkpoints",
        &[
            ExpectedColumn::required_pk("origin_path", "TEXT", 1),
            ExpectedColumn::required_pk("device_id", "TEXT", 2),
            ExpectedColumn::required_pk("session_id", "TEXT", 3),
            ExpectedColumn::required("input_tokens", "INTEGER"),
            ExpectedColumn::required("cache_write_tokens", "INTEGER"),
            ExpectedColumn::required("cache_read_tokens", "INTEGER"),
            ExpectedColumn::required("output_tokens", "INTEGER"),
            ExpectedColumn::required("reasoning_tokens", "INTEGER"),
            ExpectedColumn::required("event_sequence", "INTEGER"),
            ExpectedColumn::required("updated_at", "REAL"),
        ],
    ),
];

const REQUIRED_INDEXES: &[&str] = &[
    "idx_origin_files_rebuild",
    "idx_event_records_origin",
    "idx_event_records_origin_time",
    "idx_event_records_timestamp",
    "idx_event_records_source_status",
    "idx_event_records_identity",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginKind {
    LocalLog,
    HermesDatabase,
    SyncLedger,
}

impl OriginKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalLog => "local_log",
            Self::HermesDatabase => "hermes_database",
            Self::SyncLedger => "sync_ledger",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CachedEventRecord {
    pub origin_kind: String,
    pub origin_path: String,
    pub device_id: String,
    pub event_id: String,
    pub timestamp: f64,
    pub source: String,
    pub priority: i64,
    pub event_json: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OriginFile {
    pub origin_kind: String,
    pub origin_path: String,
    pub source: Option<String>,
    pub file_size: i64,
    pub modified_at: f64,
    pub parser_version: i64,
    pub device_id: Option<String>,
    pub parse_error: bool,
    pub event_count: i64,
    pub first_event_at: Option<f64>,
    pub last_event_at: Option<f64>,
    pub scanned_at: f64,
}

impl CachedEventRecord {
    pub fn event_value(&self) -> Result<Value, CacheError> {
        Ok(serde_json::from_slice(&self.event_json)?)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HermesCheckpoint {
    pub origin_path: String,
    pub device_id: String,
    pub session_id: String,
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub event_sequence: i64,
    pub updated_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileSnapshot {
    pub path: String,
    pub source: Option<TokenSource>,
    pub size: i64,
    pub modified_at: f64,
    pub device_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CachedFile {
    Events(Vec<TokenEvent>),
    ParseError,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IncrementalAppendBase {
    pub size: i64,
    pub modified_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HermesCheckpointUpdate {
    pub session_id: String,
    pub usage: TokenUsage,
    pub sequence: i64,
    pub event: Option<TokenEvent>,
}

pub struct TokenEventCache {
    connection: Connection,
}

impl TokenEventCache {
    pub fn open_or_create(path: &Path) -> Result<Self, CacheError> {
        if path.exists() {
            Self::validate_file_without_writing(path)?;
            let mut connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            if schema_state(&connection)? == SchemaState::PreHermes
                || !has_all_required_indexes(&connection)?
            {
                migrate_known_schema(&mut connection)?;
            }
            validate_schema(&connection)?;
            Ok(Self { connection })
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            create_schema(&mut connection)?;
            Ok(Self { connection })
        }
    }

    pub fn validate_file_without_writing(path: &Path) -> Result<(), CacheError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        schema_state(&connection).map(|_| ())
    }

    pub fn events_for_origin(
        &self,
        origin_kind: OriginKind,
        origin_path: &str,
    ) -> Result<Vec<CachedEventRecord>, CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT origin_kind, origin_path, device_id, event_id, timestamp, source, priority, event_json
             FROM event_records
             WHERE origin_kind = ?1 AND origin_path = ?2
             ORDER BY timestamp ASC, event_id ASC",
        )?;
        let rows = statement.query_map(params![origin_kind.as_str(), origin_path], |row| {
            Ok(CachedEventRecord {
                origin_kind: row.get(0)?,
                origin_path: row.get(1)?,
                device_id: row.get(2)?,
                event_id: row.get(3)?,
                timestamp: row.get(4)?,
                source: row.get(5)?,
                priority: row.get(6)?,
                event_json: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn origin_file(
        &self,
        origin_kind: OriginKind,
        origin_path: &str,
    ) -> Result<Option<OriginFile>, CacheError> {
        Ok(self
            .connection
            .query_row(
                "SELECT origin_kind, origin_path, source, file_size, modified_at, parser_version,
                        device_id, parse_error, event_count, first_event_at, last_event_at, scanned_at
                 FROM origin_files WHERE origin_kind = ?1 AND origin_path = ?2",
                params![origin_kind.as_str(), origin_path],
                |row| {
                    Ok(OriginFile {
                        origin_kind: row.get(0)?,
                        origin_path: row.get(1)?,
                        source: row.get(2)?,
                        file_size: row.get(3)?,
                        modified_at: row.get(4)?,
                        parser_version: row.get(5)?,
                        device_id: row.get(6)?,
                        parse_error: row.get::<_, i64>(7)? != 0,
                        event_count: row.get(8)?,
                        first_event_at: row.get(9)?,
                        last_event_at: row.get(10)?,
                        scanned_at: row.get(11)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn upsert_origin_file(&self, origin: &OriginFile) -> Result<(), CacheError> {
        if origin.parser_version != CACHE_PARSER_VERSION {
            return Err(CacheError::UnsupportedParserVersion(origin.parser_version));
        }
        self.connection.execute(
            "INSERT OR REPLACE INTO origin_files (
                origin_kind, origin_path, source, file_size, modified_at, parser_version,
                device_id, parse_error, event_count, first_event_at, last_event_at, scanned_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                origin.origin_kind,
                origin.origin_path,
                origin.source,
                origin.file_size,
                origin.modified_at,
                origin.parser_version,
                origin.device_id,
                origin.parse_error,
                origin.event_count,
                origin.first_event_at,
                origin.last_event_at,
                origin.scanned_at,
            ],
        )?;
        Ok(())
    }

    pub fn local_log_paths_requiring_rebuild(&self) -> Result<Vec<String>, CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT origin_path FROM origin_files
             WHERE origin_kind = 'local_log' AND parser_version != ?1
             ORDER BY origin_path",
        )?;
        let rows = statement.query_map([CACHE_PARSER_VERSION], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn cached_file(&self, snapshot: &FileSnapshot) -> Result<Option<CachedFile>, CacheError> {
        let Some(origin) = self.origin_file(OriginKind::LocalLog, &snapshot.path)? else {
            return Ok(None);
        };
        if origin.file_size != snapshot.size
            || (origin.modified_at - snapshot.modified_at).abs() >= 0.000_001
            || origin.parser_version != CACHE_PARSER_VERSION
            || origin.device_id != snapshot.device_id
            || origin.source.as_deref().and_then(parse_source) != snapshot.source
        {
            return Ok(None);
        }
        if origin.parse_error {
            return Ok(Some(CachedFile::ParseError));
        }
        Ok(Some(CachedFile::Events(self.events_for_origin_decoded(
            OriginKind::LocalLog,
            &snapshot.path,
        )?)))
    }

    pub fn incremental_append_base(
        &self,
        snapshot: &FileSnapshot,
    ) -> Result<Option<IncrementalAppendBase>, CacheError> {
        let Some(origin) = self.origin_file(OriginKind::LocalLog, &snapshot.path)? else {
            return Ok(None);
        };
        Ok((origin.parser_version == CACHE_PARSER_VERSION
            && origin.device_id == snapshot.device_id
            && origin.source.as_deref().and_then(parse_source) == snapshot.source
            && !origin.parse_error
            && origin.file_size > 0
            && snapshot.modified_at + 0.000_001 >= origin.modified_at
            && snapshot.size > origin.file_size)
            .then_some(IncrementalAppendBase {
                size: origin.file_size,
                modified_at: origin.modified_at,
            }))
    }

    pub fn replace_local_events(
        &self,
        snapshot: &FileSnapshot,
        events: &[TokenEvent],
        parse_error: bool,
    ) -> Result<(), CacheError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM event_records WHERE origin_kind = 'local_log' AND origin_path = ?1",
            [&snapshot.path],
        )?;
        transaction.execute(
            "DELETE FROM origin_files WHERE origin_kind = 'local_log' AND origin_path = ?1",
            [&snapshot.path],
        )?;
        insert_origin(&transaction, snapshot, parse_error, events.len() as i64)?;
        if !parse_error {
            insert_events(&transaction, OriginKind::LocalLog, &snapshot.path, events)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn append_local_events(
        &self,
        snapshot: &FileSnapshot,
        events: &[TokenEvent],
    ) -> Result<(), CacheError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let existing_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM event_records WHERE origin_kind = 'local_log' AND origin_path = ?1",
            [&snapshot.path],
            |row| row.get(0),
        )?;
        transaction.execute(
            "DELETE FROM origin_files WHERE origin_kind = 'local_log' AND origin_path = ?1",
            [&snapshot.path],
        )?;
        insert_origin(
            &transaction,
            snapshot,
            false,
            existing_count.saturating_add(events.len() as i64),
        )?;
        insert_events(&transaction, OriginKind::LocalLog, &snapshot.path, events)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_missing_local_origins(
        &self,
        keeping: &HashSet<String>,
        pruning_sources: &HashSet<TokenSource>,
    ) -> Result<(), CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT origin_path, source FROM origin_files WHERE origin_kind = 'local_log'",
        )?;
        let paths = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(path, source)| {
                source
                    .as_deref()
                    .and_then(parse_source)
                    .filter(|source| pruning_sources.contains(source) && !keeping.contains(&path))
                    .map(|_| path)
            })
            .collect::<Vec<_>>();
        drop(statement);
        if paths.is_empty() {
            return Ok(());
        }

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        for path in paths {
            transaction.execute(
                "DELETE FROM event_records WHERE origin_kind = 'local_log' AND origin_path = ?1",
                [&path],
            )?;
            transaction.execute(
                "DELETE FROM origin_files WHERE origin_kind = 'local_log' AND origin_path = ?1",
                [&path],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn events(
        &self,
        modified_after: Option<DateTime<Utc>>,
    ) -> Result<Vec<TokenEvent>, CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT event_json, device_id, event_id, priority
             FROM event_records
             WHERE origin_kind IN ('local_log', 'hermes_database')
               AND (?1 IS NULL OR timestamp >= ?1)
             ORDER BY timestamp, event_id, priority",
        )?;
        let timestamp = modified_after.map(unix_timestamp);
        let rows = statement.query_map([timestamp], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut by_key = std::collections::BTreeMap::new();
        for row in rows {
            let (json, device_id, event_id, priority) = row?;
            let Ok(event) = serde_json::from_slice::<TokenEvent>(&json) else {
                continue;
            };
            let key = (device_id, event_id);
            if by_key
                .get(&key)
                .is_none_or(|(_, existing_priority)| *existing_priority <= priority)
            {
                by_key.insert(key, (event, priority));
            }
        }
        let mut events = by_key
            .into_values()
            .map(|(event, _)| event)
            .collect::<Vec<_>>();
        events.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then(left.id.cmp(&right.id))
        });
        Ok(events)
    }

    pub fn local_log_origins(
        &self,
        modified_after: Option<DateTime<Utc>>,
    ) -> Result<Vec<(String, TokenSource)>, CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT origin_path, source
             FROM event_records
             WHERE origin_kind = 'local_log' AND (?1 IS NULL OR timestamp >= ?1)
             ORDER BY origin_path",
        )?;
        let rows = statement.query_map([modified_after.map(unix_timestamp)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(path, source)| parse_source(&source).map(|source| (path, source)))
            .collect())
    }

    pub fn clear(&self) -> Result<(), CacheError> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM event_records", [])?;
        transaction.execute("DELETE FROM origin_files", [])?;
        transaction.execute("DELETE FROM hermes_checkpoints", [])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn insert_event_raw(&self, record: &CachedEventRecord) -> Result<(), CacheError> {
        // Validate at the trust boundary while retaining the original Swift JSON bytes.
        let _: Value = serde_json::from_slice(&record.event_json)?;
        self.connection.execute(
            "INSERT OR REPLACE INTO event_records (
                origin_kind, origin_path, device_id, event_id, timestamp, source, priority, event_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.origin_kind,
                record.origin_path,
                record.device_id,
                record.event_id,
                record.timestamp,
                record.source,
                record.priority,
                record.event_json,
            ],
        )?;
        Ok(())
    }

    pub fn hermes_checkpoints(
        &self,
        origin_path: &str,
        device_id: &str,
    ) -> Result<Vec<HermesCheckpoint>, CacheError> {
        let mut statement = self.connection.prepare(
            "SELECT origin_path, device_id, session_id, input_tokens, cache_write_tokens,
                    cache_read_tokens, output_tokens, reasoning_tokens, event_sequence, updated_at
             FROM hermes_checkpoints
             WHERE origin_path = ?1 AND device_id = ?2
             ORDER BY session_id",
        )?;
        let rows = statement.query_map(params![origin_path, device_id], |row| {
            Ok(HermesCheckpoint {
                origin_path: row.get(0)?,
                device_id: row.get(1)?,
                session_id: row.get(2)?,
                input_tokens: row.get(3)?,
                cache_write_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                reasoning_tokens: row.get(7)?,
                event_sequence: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn insert_hermes_checkpoint(
        &self,
        checkpoint: &HermesCheckpoint,
    ) -> Result<(), CacheError> {
        self.connection.execute(
            "INSERT OR REPLACE INTO hermes_checkpoints (
                origin_path, device_id, session_id, input_tokens, cache_write_tokens,
                cache_read_tokens, output_tokens, reasoning_tokens, event_sequence, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                checkpoint.origin_path,
                checkpoint.device_id,
                checkpoint.session_id,
                checkpoint.input_tokens,
                checkpoint.cache_write_tokens,
                checkpoint.cache_read_tokens,
                checkpoint.output_tokens,
                checkpoint.reasoning_tokens,
                checkpoint.event_sequence,
                checkpoint.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn apply_hermes_updates(
        &self,
        origin_path: &str,
        device_id: &str,
        updates: &[HermesCheckpointUpdate],
        updated_at: DateTime<Utc>,
    ) -> Result<(), CacheError> {
        if updates.is_empty() {
            return Ok(());
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        for update in updates {
            if let Some(event) = &update.event {
                insert_events(
                    &transaction,
                    OriginKind::HermesDatabase,
                    origin_path,
                    std::slice::from_ref(event),
                )?;
            }
            transaction.execute(
                "INSERT INTO hermes_checkpoints (
                    origin_path, device_id, session_id, input_tokens, cache_write_tokens,
                    cache_read_tokens, output_tokens, reasoning_tokens, event_sequence, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(origin_path, device_id, session_id) DO UPDATE SET
                    input_tokens = excluded.input_tokens,
                    cache_write_tokens = excluded.cache_write_tokens,
                    cache_read_tokens = excluded.cache_read_tokens,
                    output_tokens = excluded.output_tokens,
                    reasoning_tokens = excluded.reasoning_tokens,
                    event_sequence = excluded.event_sequence,
                    updated_at = excluded.updated_at",
                params![
                    origin_path,
                    device_id,
                    update.session_id,
                    update.usage.input,
                    update.usage.cache_creation,
                    update.usage.cache_read,
                    update.usage.output,
                    update.usage.reasoning,
                    update.sequence,
                    unix_timestamp(updated_at),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn events_for_origin_decoded(
        &self,
        origin_kind: OriginKind,
        origin_path: &str,
    ) -> Result<Vec<TokenEvent>, CacheError> {
        Ok(self
            .events_for_origin(origin_kind, origin_path)?
            .into_iter()
            .filter_map(|record| serde_json::from_slice(&record.event_json).ok())
            .collect())
    }

    pub fn mark_fixture_migrated(path: &Path, migration: &str) -> Result<(), CacheError> {
        let size = fs::metadata(path)?.len();
        if size > MAX_FIXTURE_MIGRATION_BYTES {
            return Err(CacheError::FixtureTooLarge {
                actual: size,
                maximum: MAX_FIXTURE_MIGRATION_BYTES,
            });
        }
        Self::validate_file_without_writing(path)?;

        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )?;
        validate_schema(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS token_meter_migrations (
                migration TEXT PRIMARY KEY NOT NULL,
                applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             )",
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO token_meter_migrations (migration) VALUES (?1)",
            [migration],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn has_migration_marker(&self, migration: &str) -> Result<bool, CacheError> {
        let table_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'token_meter_migrations')",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(false);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM token_meter_migrations WHERE migration = ?1",
                [migration],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    #[cfg(test)]
    fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn insert_origin(
    connection: &Connection,
    snapshot: &FileSnapshot,
    parse_error: bool,
    event_count: i64,
) -> Result<(), CacheError> {
    connection.execute(
        "INSERT INTO origin_files (
            origin_kind, origin_path, source, file_size, modified_at, parser_version,
            device_id, parse_error, event_count, first_event_at, last_event_at, scanned_at
         ) VALUES ('local_log', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, ?9)",
        params![
            snapshot.path,
            snapshot.source.map(source_name),
            snapshot.size,
            snapshot.modified_at,
            CACHE_PARSER_VERSION,
            snapshot.device_id,
            i64::from(parse_error),
            event_count,
            unix_timestamp(Utc::now()),
        ],
    )?;
    Ok(())
}

fn insert_events(
    connection: &Connection,
    origin_kind: OriginKind,
    origin_path: &str,
    events: &[TokenEvent],
) -> Result<(), CacheError> {
    let priority = if origin_kind == OriginKind::SyncLedger {
        1
    } else {
        2
    };
    let mut statement = connection.prepare(
        "INSERT OR REPLACE INTO event_records (
            origin_kind, origin_path, device_id, event_id, timestamp, source, priority, event_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for event in events {
        statement.execute(params![
            origin_kind.as_str(),
            origin_path,
            event.device_id,
            event.id,
            unix_timestamp(event.timestamp),
            source_name(event.source),
            priority,
            serde_json::to_vec(event)?,
        ])?;
    }
    Ok(())
}

fn source_name(source: TokenSource) -> &'static str {
    match source {
        TokenSource::All => "all",
        TokenSource::Codex => "codex",
        TokenSource::Claude => "claude",
    }
}

fn parse_source(source: &str) -> Option<TokenSource> {
    match source {
        "all" => Some(TokenSource::All),
        "codex" => Some(TokenSource::Codex),
        "claude" => Some(TokenSource::Claude),
        _ => None,
    }
}

fn unix_timestamp(date: DateTime<Utc>) -> f64 {
    date.timestamp() as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9
}

fn create_schema(connection: &mut Connection) -> Result<(), CacheError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE origin_files (
            origin_kind TEXT NOT NULL,
            origin_path TEXT NOT NULL,
            source TEXT,
            file_size INTEGER NOT NULL,
            modified_at REAL NOT NULL,
            parser_version INTEGER NOT NULL,
            device_id TEXT,
            parse_error INTEGER NOT NULL,
            event_count INTEGER NOT NULL,
            first_event_at REAL,
            last_event_at REAL,
            scanned_at REAL NOT NULL,
            PRIMARY KEY (origin_kind, origin_path)
        );
        CREATE TABLE event_records (
            origin_kind TEXT NOT NULL,
            origin_path TEXT NOT NULL,
            device_id TEXT NOT NULL,
            event_id TEXT NOT NULL,
            timestamp REAL NOT NULL,
            source TEXT NOT NULL,
            priority INTEGER NOT NULL,
            event_json BLOB NOT NULL,
            PRIMARY KEY (origin_kind, origin_path, device_id, event_id)
        );
        CREATE INDEX idx_origin_files_rebuild
            ON origin_files(origin_kind, parser_version, origin_path);
        CREATE INDEX idx_event_records_origin
            ON event_records(origin_kind, origin_path);
        CREATE INDEX idx_event_records_origin_time
            ON event_records(origin_kind, origin_path, timestamp, event_id);
        CREATE INDEX idx_event_records_timestamp ON event_records(timestamp);
        CREATE INDEX idx_event_records_source_status
            ON event_records(origin_kind, timestamp, source, origin_path);
        CREATE INDEX idx_event_records_identity
            ON event_records(device_id, event_id, priority);
        CREATE TABLE hermes_checkpoints (
            origin_path TEXT NOT NULL,
            device_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            input_tokens INTEGER NOT NULL,
            cache_write_tokens INTEGER NOT NULL,
            cache_read_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            reasoning_tokens INTEGER NOT NULL,
            event_sequence INTEGER NOT NULL,
            updated_at REAL NOT NULL,
            PRIMARY KEY (origin_path, device_id, session_id)
        );",
    )?;
    transaction.commit()?;
    validate_schema(connection)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchemaState {
    Current,
    PreHermes,
}

fn schema_state(connection: &Connection) -> Result<SchemaState, CacheError> {
    for (table, expected) in &REQUIRED_TABLES[..2] {
        validate_table(connection, table, expected)?;
    }

    let (table, expected) = REQUIRED_TABLES[2];
    let actual = table_columns(connection, table)?;
    if actual.is_empty() {
        let object_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if object_exists {
            return Err(CacheError::SchemaMismatch { table, actual });
        }
        return Ok(SchemaState::PreHermes);
    }
    ensure_columns_match(table, actual, expected)?;
    Ok(SchemaState::Current)
}

fn migrate_known_schema(connection: &mut Connection) -> Result<(), CacheError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if schema_state(&transaction)? == SchemaState::PreHermes {
        transaction.execute_batch(
            "CREATE TABLE hermes_checkpoints (
            origin_path TEXT NOT NULL,
            device_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            input_tokens INTEGER NOT NULL,
            cache_write_tokens INTEGER NOT NULL,
            cache_read_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            reasoning_tokens INTEGER NOT NULL,
            event_sequence INTEGER NOT NULL,
            updated_at REAL NOT NULL,
            PRIMARY KEY (origin_path, device_id, session_id)
        );",
        )?;
    }
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_origin_files_rebuild
            ON origin_files(origin_kind, parser_version, origin_path);
        CREATE INDEX IF NOT EXISTS idx_event_records_origin
            ON event_records(origin_kind, origin_path);
        CREATE INDEX IF NOT EXISTS idx_event_records_origin_time
            ON event_records(origin_kind, origin_path, timestamp, event_id);
        CREATE INDEX IF NOT EXISTS idx_event_records_timestamp
            ON event_records(timestamp);
        CREATE INDEX IF NOT EXISTS idx_event_records_source_status
            ON event_records(origin_kind, timestamp, source, origin_path);
        CREATE INDEX IF NOT EXISTS idx_event_records_identity
            ON event_records(device_id, event_id, priority);",
    )?;
    transaction.commit()?;
    validate_schema(connection)
}

fn has_all_required_indexes(connection: &Connection) -> Result<bool, CacheError> {
    let mut statement =
        connection.prepare("SELECT name FROM sqlite_master WHERE type = 'index'")?;
    let indexes = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(REQUIRED_INDEXES
        .iter()
        .all(|index| indexes.contains(*index)))
}

fn validate_schema(connection: &Connection) -> Result<(), CacheError> {
    for (table, expected) in REQUIRED_TABLES {
        validate_table(connection, table, expected)?;
    }
    Ok(())
}

fn validate_table(
    connection: &Connection,
    table: &'static str,
    expected: &[ExpectedColumn],
) -> Result<(), CacheError> {
    ensure_columns_match(table, table_columns(connection, table)?, expected)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<Column>, CacheError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| {
            Ok(Column {
                name: row.get(1)?,
                data_type: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                primary_key: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn ensure_columns_match(
    table: &'static str,
    actual: Vec<Column>,
    expected: &[ExpectedColumn],
) -> Result<(), CacheError> {
    let matches = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            actual.name == expected.name
                && actual.data_type.eq_ignore_ascii_case(expected.data_type)
                && actual.not_null == expected.not_null
                && actual.primary_key == expected.primary_key
        });
    if matches {
        Ok(())
    } else {
        Err(CacheError::SchemaMismatch { table, actual })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    name: String,
    data_type: String,
    not_null: bool,
    primary_key: i64,
}

#[derive(Clone, Copy)]
struct ExpectedColumn {
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    primary_key: i64,
}

impl ExpectedColumn {
    const fn required(name: &'static str, data_type: &'static str) -> Self {
        Self {
            name,
            data_type,
            not_null: true,
            primary_key: 0,
        }
    }

    const fn optional(name: &'static str, data_type: &'static str) -> Self {
        Self {
            name,
            data_type,
            not_null: false,
            primary_key: 0,
        }
    }

    const fn required_pk(name: &'static str, data_type: &'static str, position: i64) -> Self {
        Self {
            name,
            data_type,
            not_null: true,
            primary_key: position,
        }
    }
}

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cached event JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cache writes require parser version {CACHE_PARSER_VERSION}, got {0}")]
    UnsupportedParserVersion(i64),
    #[error("cache table {table} does not match the TokenMeter schema")]
    SchemaMismatch {
        table: &'static str,
        actual: Vec<Column>,
    },
    #[error("fixture cache is {actual} bytes, exceeding the {maximum}-byte migration limit")]
    FixtureTooLarge { actual: u64, maximum: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_origin(cache: &TokenEventCache, path: &str, source: &str) {
        cache
            .connection()
            .execute(
                "INSERT INTO origin_files (
                    origin_kind, origin_path, source, file_size, modified_at, parser_version,
                    device_id, parse_error, event_count, first_event_at, last_event_at, scanned_at
                 ) VALUES ('local_log', ?1, ?2, 10, 1.0, ?3, 'mac-a', 0, 1, 1.0, 1.0, 1.0)",
                params![path, source, CACHE_PARSER_VERSION],
            )
            .unwrap();
    }

    #[test]
    fn reads_legacy_swift_json_without_reencoding_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.sqlite");
        let cache = TokenEventCache::open_or_create(&path).unwrap();
        insert_origin(&cache, "/removed/codex.jsonl", "codex");
        let event_json = br#"{"id":"event-a","source":"codex","timestamp":796608000.0,"deviceId":"mac-a","deviceName":"Mac A","projectPath":"/tmp/project","sessionId":"session-a","model":"gpt","usage":{"input":3,"cachedInput":2,"cacheCreation":0,"cacheRead":0,"output":1,"reasoning":0,"total":4},"rawFilePath":"/removed/codex.jsonl"}"#.to_vec();
        cache
            .insert_event_raw(&CachedEventRecord {
                origin_kind: "local_log".into(),
                origin_path: "/removed/codex.jsonl".into(),
                device_id: "mac-a".into(),
                event_id: "event-a".into(),
                timestamp: 1_774_915_200.0,
                source: "codex".into(),
                priority: 2,
                event_json: event_json.clone(),
            })
            .unwrap();

        let rows = cache
            .events_for_origin(OriginKind::LocalLog, "/removed/codex.jsonl")
            .unwrap();
        assert_eq!(rows[0].event_json, event_json);
        assert_eq!(rows[0].event_value().unwrap()["usage"]["cachedInput"], 2);
        assert_eq!(cache.events(None).unwrap()[0].usage.total, 4);
        assert_eq!(
            cache
                .events(DateTime::from_timestamp(1_774_915_199, 0))
                .unwrap()
                .len(),
            1
        );
        assert!(
            cache
                .events(DateTime::from_timestamp(1_774_915_201, 0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fixture_marker_preserves_removed_codex_history_and_hermes_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.sqlite");
        {
            let cache = TokenEventCache::open_or_create(&path).unwrap();
            insert_origin(&cache, "/removed/codex.jsonl", "codex");
            cache
                .insert_event_raw(&CachedEventRecord {
                    origin_kind: "local_log".into(),
                    origin_path: "/removed/codex.jsonl".into(),
                    device_id: "mac-a".into(),
                    event_id: "old-event".into(),
                    timestamp: 1.0,
                    source: "codex".into(),
                    priority: 2,
                    event_json: br#"{"id":"old-event","source":"codex","timestamp":-978307199.0,"deviceId":"mac-a","deviceName":"Mac A","projectPath":"/removed","sessionId":"old-session","model":"gpt","usage":{"input":10,"cachedInput":0,"cacheCreation":0,"cacheRead":0,"output":0,"reasoning":0,"total":10},"rawFilePath":"/removed/codex.jsonl"}"#.to_vec(),
                })
                .unwrap();
            cache
                .insert_hermes_checkpoint(&HermesCheckpoint {
                    origin_path: "/Library/Application Support/Hermes/hermes.db".into(),
                    device_id: "mac-a".into(),
                    session_id: "session-a".into(),
                    input_tokens: 10,
                    cache_write_tokens: 2,
                    cache_read_tokens: 3,
                    output_tokens: 4,
                    reasoning_tokens: 1,
                    event_sequence: 7,
                    updated_at: 99.0,
                })
                .unwrap();
        }

        TokenEventCache::mark_fixture_migrated(&path, "rust-v1").unwrap();
        let cache = TokenEventCache::open_or_create(&path).unwrap();
        assert!(cache.has_migration_marker("rust-v1").unwrap());
        assert_eq!(
            cache
                .events_for_origin(OriginKind::LocalLog, "/removed/codex.jsonl")
                .unwrap()[0]
                .event_id,
            "old-event"
        );
        assert_eq!(cache.events(None).unwrap()[0].usage.total, 10);
        assert_eq!(
            cache
                .hermes_checkpoints("/Library/Application Support/Hermes/hermes.db", "mac-a")
                .unwrap()[0]
                .event_sequence,
            7
        );
    }

    #[test]
    fn schema_mismatch_is_rejected_before_any_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wrong.sqlite");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute("CREATE TABLE origin_files (origin_kind TEXT)", [])
                .unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            TokenEventCache::mark_fixture_migrated(&path, "must-not-write"),
            Err(CacheError::SchemaMismatch { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn existing_cache_keeps_the_multi_process_busy_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.sqlite");
        drop(TokenEventCache::open_or_create(&path).unwrap());
        let cache = TokenEventCache::open_or_create(&path).unwrap();
        let timeout: i64 = cache
            .connection()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5_000);
    }

    #[test]
    fn opens_pre_hermes_cache_additively_without_rewriting_existing_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pre-hermes.sqlite");
        let event_json = br#"{"id":"legacy-event","source":"codex","timestamp":-978306200.0,"deviceId":"mac-a","deviceName":"Mac A","projectPath":"/legacy/project","sessionId":"legacy-session","model":"gpt","usage":{"input":12,"cachedInput":0,"cacheCreation":0,"cacheRead":0,"output":0,"reasoning":0,"total":12},"rawFilePath":"/legacy/session.jsonl"}"#.to_vec();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE origin_files (
                        origin_kind TEXT NOT NULL, origin_path TEXT NOT NULL, source TEXT,
                        file_size INTEGER NOT NULL, modified_at REAL NOT NULL,
                        parser_version INTEGER NOT NULL, device_id TEXT, parse_error INTEGER NOT NULL,
                        event_count INTEGER NOT NULL, first_event_at REAL, last_event_at REAL,
                        scanned_at REAL NOT NULL, PRIMARY KEY (origin_kind, origin_path)
                     );
                     CREATE TABLE event_records (
                        origin_kind TEXT NOT NULL, origin_path TEXT NOT NULL, device_id TEXT NOT NULL,
                        event_id TEXT NOT NULL, timestamp REAL NOT NULL, source TEXT NOT NULL,
                        priority INTEGER NOT NULL, event_json BLOB NOT NULL,
                        PRIMARY KEY (origin_kind, origin_path, device_id, event_id)
                     );
                     CREATE INDEX idx_event_records_origin
                        ON event_records(origin_kind, origin_path);
                     CREATE INDEX idx_event_records_origin_time
                        ON event_records(origin_kind, origin_path, timestamp, event_id);
                     CREATE INDEX idx_event_records_timestamp ON event_records(timestamp);
                     CREATE INDEX idx_event_records_identity
                        ON event_records(device_id, event_id, priority);",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO origin_files VALUES
                     ('local_log', '/legacy/session.jsonl', 'codex', 321, 1001.25, 3,
                      'mac-a', 0, 1, 1000.0, 1000.0, 1002.5)",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO event_records VALUES
                     ('local_log', '/legacy/session.jsonl', 'mac-a', 'legacy-event',
                      1000.0, 'codex', 2, ?1)",
                    [&event_json],
                )
                .unwrap();
        }

        let before_validation = fs::read(&path).unwrap();
        TokenEventCache::validate_file_without_writing(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before_validation);

        let cache = TokenEventCache::open_or_create(&path).unwrap();
        let origin = cache
            .origin_file(OriginKind::LocalLog, "/legacy/session.jsonl")
            .unwrap()
            .unwrap();
        assert_eq!(origin.file_size, 321);
        assert_eq!(origin.modified_at, 1001.25);
        assert_eq!(origin.parser_version, 3);
        assert_eq!(origin.first_event_at, Some(1000.0));
        assert_eq!(origin.last_event_at, Some(1000.0));
        assert_eq!(origin.scanned_at, 1002.5);
        assert_eq!(cache.events(None).unwrap()[0].usage.total, 12);
        assert_eq!(
            cache
                .events_for_origin(OriginKind::LocalLog, "/legacy/session.jsonl")
                .unwrap()[0]
                .event_json,
            event_json
        );

        let indexes = cache
            .connection()
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND name NOT LIKE 'sqlite_autoindex%'
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            indexes,
            [
                "idx_event_records_identity",
                "idx_event_records_origin",
                "idx_event_records_origin_time",
                "idx_event_records_source_status",
                "idx_event_records_timestamp",
                "idx_origin_files_rebuild",
            ]
        );

        cache
            .insert_hermes_checkpoint(&HermesCheckpoint {
                origin_path: "/legacy/hermes.db".into(),
                device_id: "mac-a".into(),
                session_id: "hermes-session".into(),
                input_tokens: 5,
                cache_write_tokens: 1,
                cache_read_tokens: 2,
                output_tokens: 3,
                reasoning_tokens: 1,
                event_sequence: 4,
                updated_at: 1003.0,
            })
            .unwrap();
        assert_eq!(
            cache
                .hermes_checkpoints("/legacy/hermes.db", "mac-a")
                .unwrap()[0]
                .event_sequence,
            4
        );
    }

    #[test]
    fn malformed_hermes_table_is_not_treated_as_pre_hermes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("malformed-hermes.sqlite");
        {
            let source = TokenEventCache::open_or_create(&path).unwrap();
            source
                .connection()
                .execute_batch(
                    "DROP TABLE hermes_checkpoints;
                     CREATE TABLE hermes_checkpoints (origin_path TEXT);",
                )
                .unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            TokenEventCache::open_or_create(&path),
            Err(CacheError::SchemaMismatch {
                table: "hermes_checkpoints",
                ..
            })
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
