use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    cache::{HermesCheckpoint, HermesCheckpointUpdate, TokenEventCache},
    models::{TokenDeviceMetadata, TokenEvent, TokenSource, TokenUsage},
};

const HERMES_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default, PartialEq)]
pub struct HermesScanOutcome {
    pub events: Vec<TokenEvent>,
    pub database_exists: bool,
    pub parse_error_count: usize,
}

pub struct HermesScanner<'a> {
    database_path: PathBuf,
    local_device: TokenDeviceMetadata,
    cache: Option<&'a TokenEventCache>,
}

impl<'a> HermesScanner<'a> {
    pub fn new(
        database_path: impl Into<PathBuf>,
        local_device: TokenDeviceMetadata,
        cache: Option<&'a TokenEventCache>,
    ) -> Self {
        Self {
            database_path: database_path.into(),
            local_device,
            cache,
        }
    }

    pub fn scan(&self, is_cancelled: impl Fn() -> bool) -> HermesScanOutcome {
        self.scan_at(Utc::now(), is_cancelled)
    }

    pub fn scan_at(
        &self,
        observed_at: DateTime<Utc>,
        is_cancelled: impl Fn() -> bool,
    ) -> HermesScanOutcome {
        if !self.database_path.is_file() {
            return HermesScanOutcome::default();
        }
        if is_cancelled() {
            return HermesScanOutcome {
                database_exists: true,
                ..HermesScanOutcome::default()
            };
        }
        match self.read_and_import(observed_at, &is_cancelled) {
            Ok(events) if !is_cancelled() => HermesScanOutcome {
                events,
                database_exists: true,
                parse_error_count: 0,
            },
            Ok(_) => HermesScanOutcome {
                database_exists: true,
                ..HermesScanOutcome::default()
            },
            Err(_) => HermesScanOutcome {
                database_exists: true,
                parse_error_count: 1,
                ..HermesScanOutcome::default()
            },
        }
    }

    fn read_and_import(
        &self,
        observed_at: DateTime<Utc>,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<TokenEvent>, HermesError> {
        let busy_deadline = Instant::now() + HERMES_BUSY_TIMEOUT;
        let connection = Connection::open_with_flags(
            &self.database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )?;
        apply_busy_deadline(&connection, busy_deadline)?;
        connection.execute_batch("PRAGMA query_only = ON")?;
        let sessions = read_sessions(&connection, busy_deadline, is_cancelled)?;
        if is_cancelled() {
            return Ok(Vec::new());
        }
        self.import_sessions(&sessions, observed_at, is_cancelled)
    }

    fn import_sessions(
        &self,
        sessions: &[HermesSession],
        observed_at: DateTime<Utc>,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<TokenEvent>, HermesError> {
        let origin_path = canonical_text(&self.database_path);
        let Some(cache) = self.cache else {
            return Ok(sessions
                .iter()
                .map(|session| {
                    self.make_event(
                        session,
                        session.usage,
                        session.baseline_timestamp(),
                        &usage_fingerprint(session.usage),
                    )
                })
                .collect());
        };
        let checkpoints = cache
            .hermes_checkpoints(&origin_path, &self.local_device.id)?
            .into_iter()
            .map(|checkpoint| (checkpoint.session_id.clone(), checkpoint))
            .collect::<HashMap<_, _>>();
        let mut updates = Vec::with_capacity(sessions.len());
        let mut events = Vec::new();

        for session in sessions {
            if is_cancelled() {
                return Ok(Vec::new());
            }
            let checkpoint = checkpoints.get(&session.id);
            let (sequence, event_usage, timestamp) = if let Some(checkpoint) = checkpoint {
                let previous = checkpoint_usage(checkpoint);
                let decreased = counters_decreased(session.usage, previous);
                let delta = subtract_usage(session.usage, previous);
                let event_usage = (!decreased && delta.total > 0).then_some(delta);
                (
                    checkpoint.event_sequence + i64::from(event_usage.is_some()),
                    event_usage,
                    session.last_activity_timestamp(observed_at),
                )
            } else {
                (1, Some(session.usage), session.baseline_timestamp())
            };
            let event = event_usage
                .map(|usage| self.make_event(session, usage, timestamp, &sequence.to_string()));
            if let Some(event) = &event {
                events.push(event.clone());
            }
            updates.push(HermesCheckpointUpdate {
                session_id: session.id.clone(),
                usage: session.usage,
                sequence: event.as_ref().map_or_else(
                    || checkpoint.map_or(0, |checkpoint| checkpoint.event_sequence),
                    |_| sequence,
                ),
                event,
            });
        }
        if is_cancelled() {
            return Ok(Vec::new());
        }
        cache.apply_hermes_updates(&origin_path, &self.local_device.id, &updates, observed_at)?;
        Ok(events)
    }

    fn make_event(
        &self,
        session: &HermesSession,
        usage: TokenUsage,
        timestamp: DateTime<Utc>,
        sequence: &str,
    ) -> TokenEvent {
        let digest = Sha256::digest(session.id.as_bytes());
        let session_hash = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        TokenEvent::new(
            format!("hermes-{session_hash}-{sequence}"),
            TokenSource::Codex,
            timestamp,
            &self.local_device.id,
            &self.local_device.name,
            &session.project_path,
            &session.id,
            &session.model,
            usage,
            format!("hermes://state.db/{session_hash}"),
        )
    }
}

fn read_sessions(
    connection: &Connection,
    busy_deadline: Instant,
    is_cancelled: &impl Fn() -> bool,
) -> Result<Vec<HermesSession>, HermesError> {
    let session_columns = columns(connection, "sessions", busy_deadline)?;
    if !["id", "billing_provider", "started_at"]
        .into_iter()
        .all(|column| session_columns.contains(column))
    {
        return Err(HermesError::IncompatibleSchema);
    }
    let token_columns = [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
    ];
    if !token_columns
        .iter()
        .any(|column| session_columns.contains(*column))
    {
        return Err(HermesError::IncompatibleSchema);
    }
    let message_columns = columns(connection, "messages", busy_deadline)?;
    let message_time =
        if message_columns.contains("session_id") && message_columns.contains("timestamp") {
            "(SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = s.id)"
        } else {
            "NULL"
        };
    let expression = |column: &str, fallback: &str| {
        if session_columns.contains(column) {
            format!("s.{column}")
        } else {
            fallback.to_owned()
        }
    };
    let positive = token_columns
        .iter()
        .filter(|column| session_columns.contains(**column))
        .map(|column| format!("COALESCE(s.{column}, 0) > 0"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!(
        "SELECT s.id, {}, s.started_at, {}, {}, {}, {}, {}, {}, {}, {message_time}
         FROM sessions s
         WHERE s.billing_provider = ?1 AND ({positive})
         ORDER BY s.id",
        expression("model", "NULL"),
        expression("ended_at", "NULL"),
        expression("input_tokens", "0"),
        expression("output_tokens", "0"),
        expression("cache_read_tokens", "0"),
        expression("cache_write_tokens", "0"),
        expression("reasoning_tokens", "0"),
        expression("cwd", "NULL"),
    );
    apply_busy_deadline(connection, busy_deadline)?;
    let mut statement = connection.prepare(&sql)?;
    apply_busy_deadline(connection, busy_deadline)?;
    let mut rows = statement.query(["openai-codex"])?;
    let mut sessions = Vec::new();
    loop {
        apply_busy_deadline(connection, busy_deadline)?;
        let Some(row) = rows.next()? else {
            break;
        };
        if is_cancelled() {
            return Ok(Vec::new());
        }
        let Some(started_at) = row.get::<_, Option<f64>>(2)?.and_then(normalized_date) else {
            continue;
        };
        let output = nonnegative(row.get::<_, i64>(5)?);
        let reasoning = nonnegative(row.get::<_, i64>(8)?).min(output);
        sessions.push(HermesSession {
            id: row.get(0)?,
            model: nonempty(row.get::<_, Option<String>>(1)?).unwrap_or_else(|| "Unknown".into()),
            started_at,
            ended_at: row.get::<_, Option<f64>>(3)?.and_then(normalized_date),
            usage: TokenUsage::new(
                nonnegative(row.get(4)?),
                0,
                nonnegative(row.get(7)?),
                nonnegative(row.get(6)?),
                output,
                reasoning,
                None,
            ),
            project_path: nonempty(row.get::<_, Option<String>>(9)?)
                .unwrap_or_else(|| "Unknown".into()),
            last_message_at: row.get::<_, Option<f64>>(10)?.and_then(normalized_date),
        });
    }
    Ok(sessions)
}

fn columns(
    connection: &Connection,
    table: &str,
    busy_deadline: Instant,
) -> Result<std::collections::HashSet<String>, rusqlite::Error> {
    apply_busy_deadline(connection, busy_deadline)?;
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    apply_busy_deadline(connection, busy_deadline)?;
    let mut rows = statement.query([])?;
    let mut columns = std::collections::HashSet::new();
    loop {
        apply_busy_deadline(connection, busy_deadline)?;
        let Some(row) = rows.next()? else {
            break;
        };
        columns.insert(row.get(1)?);
    }
    Ok(columns)
}

fn apply_busy_deadline(connection: &Connection, deadline: Instant) -> Result<(), rusqlite::Error> {
    connection.busy_timeout(deadline.saturating_duration_since(Instant::now()))
}

fn normalized_date(raw: f64) -> Option<DateTime<Utc>> {
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let seconds = if raw > 100_000_000_000_000_000.0 {
        raw / 1_000_000_000.0
    } else if raw > 100_000_000_000_000.0 {
        raw / 1_000_000.0
    } else if raw > 100_000_000_000.0 {
        raw / 1_000.0
    } else {
        raw
    };
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9).round();
    let (whole, nanos) = if nanos >= 1e9 {
        (whole as i64 + 1, 0)
    } else {
        (whole as i64, nanos as u32)
    };
    DateTime::from_timestamp(whole, nanos)
}

fn nonnegative(value: i64) -> i64 {
    value.max(0)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn checkpoint_usage(checkpoint: &HermesCheckpoint) -> TokenUsage {
    TokenUsage::new(
        checkpoint.input_tokens,
        0,
        checkpoint.cache_write_tokens,
        checkpoint.cache_read_tokens,
        checkpoint.output_tokens,
        checkpoint.reasoning_tokens,
        None,
    )
}

fn counters_decreased(current: TokenUsage, previous: TokenUsage) -> bool {
    current.input < previous.input
        || current.cache_creation < previous.cache_creation
        || current.cache_read < previous.cache_read
        || current.output < previous.output
        || current.reasoning < previous.reasoning
}

fn subtract_usage(current: TokenUsage, previous: TokenUsage) -> TokenUsage {
    TokenUsage::new(
        current.input.saturating_sub(previous.input),
        0,
        current
            .cache_creation
            .saturating_sub(previous.cache_creation),
        current.cache_read.saturating_sub(previous.cache_read),
        current.output.saturating_sub(previous.output),
        current.reasoning.saturating_sub(previous.reasoning),
        None,
    )
}

fn usage_fingerprint(usage: TokenUsage) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        usage.input, usage.cache_creation, usage.cache_read, usage.output, usage.reasoning
    )
}

fn canonical_text(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_owned())
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug)]
struct HermesSession {
    id: String,
    model: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    usage: TokenUsage,
    project_path: String,
    last_message_at: Option<DateTime<Utc>>,
}

impl HermesSession {
    fn baseline_timestamp(&self) -> DateTime<Utc> {
        self.ended_at
            .or(self.last_message_at)
            .unwrap_or(self.started_at)
    }

    fn last_activity_timestamp(&self, observed_at: DateTime<Utc>) -> DateTime<Utc> {
        self.ended_at
            .into_iter()
            .chain(self.last_message_at)
            .max()
            .unwrap_or(observed_at)
    }
}

#[derive(Debug, Error)]
enum HermesError {
    #[error("Hermes schema is incompatible")]
    IncompatibleSchema,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Cache(#[from] crate::cache::CacheError),
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, model TEXT, started_at REAL NOT NULL, ended_at REAL,
                input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0, cwd TEXT, billing_provider TEXT
             );
             CREATE TABLE messages (session_id TEXT NOT NULL, timestamp REAL NOT NULL);",
            )
            .unwrap();
        connection
    }

    #[test]
    fn imports_only_codex_billing_and_tracks_delta_reset_continuity() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("state.db");
        let connection = database(&database_path);
        connection.execute_batch(
            "INSERT INTO sessions VALUES
                ('active', 'gpt', 1700000000, NULL, 50, 20, 10, 0, 5, '/tmp/project', 'openai-codex'),
                ('other', 'gpt', 1700000000, NULL, 999, 999, 0, 0, 0, NULL, 'openrouter');
             INSERT INTO messages VALUES ('active', 1700000100);",
        ).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner = HermesScanner::new(
            &database_path,
            TokenDeviceMetadata::new("device-a", "Device A"),
            Some(&cache),
        );

        let first = scanner.scan_at(DateTime::from_timestamp(1_700_000_200, 0).unwrap(), || {
            false
        });
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].usage.total, 80);
        assert_eq!(first.events[0].timestamp.timestamp(), 1_700_000_100);
        assert!(first.events[0].raw_file_path.starts_with("hermes://"));

        connection.execute("UPDATE sessions SET input_tokens = 70, output_tokens = 30, cache_read_tokens = 15, reasoning_tokens = 7 WHERE id = 'active'", []).unwrap();
        let grown = scanner.scan_at(DateTime::from_timestamp(1_700_000_300, 0).unwrap(), || {
            false
        });
        assert_eq!(grown.events.len(), 1);
        assert_eq!(grown.events[0].usage.total, 35);
        assert_eq!(grown.events[0].usage.reasoning, 2);

        connection.execute("UPDATE sessions SET input_tokens = 1, output_tokens = 0, cache_read_tokens = 0, reasoning_tokens = 0 WHERE id = 'active'", []).unwrap();
        assert!(
            scanner
                .scan_at(DateTime::from_timestamp(1_700_000_400, 0).unwrap(), || {
                    false
                })
                .events
                .is_empty()
        );
        connection
            .execute(
                "UPDATE sessions SET input_tokens = 3 WHERE id = 'active'",
                [],
            )
            .unwrap();
        let resumed = scanner.scan_at(DateTime::from_timestamp(1_700_000_500, 0).unwrap(), || {
            false
        });
        assert_eq!(resumed.events[0].usage.input, 2);
        assert_eq!(cache.events(None).unwrap().len(), 3);
    }

    #[test]
    fn read_only_scan_handles_optional_columns_and_schema_drift() {
        let directory = tempdir().unwrap();
        let minimal_path = directory.path().join("minimal.db");
        let connection = Connection::open(&minimal_path).unwrap();
        connection.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, billing_provider TEXT, started_at REAL, input_tokens INTEGER);
             INSERT INTO sessions VALUES ('minimal', 'openai-codex', 1700000000, 12);",
        ).unwrap();
        drop(connection);
        let before = std::fs::read(&minimal_path).unwrap();
        let scanner =
            HermesScanner::new(&minimal_path, TokenDeviceMetadata::local_fallback(), None);
        let result = scanner.scan(|| false);
        assert_eq!(result.events[0].usage.total, 12);
        assert_eq!(std::fs::read(&minimal_path).unwrap(), before);

        let drift_path = directory.path().join("drift.db");
        Connection::open(&drift_path)
            .unwrap()
            .execute("CREATE TABLE sessions (id TEXT)", [])
            .unwrap();
        let drift = HermesScanner::new(&drift_path, TokenDeviceMetadata::local_fallback(), None)
            .scan(|| false);
        assert_eq!(drift.parse_error_count, 1);
        assert!(drift.events.is_empty());
    }

    #[test]
    fn cancellation_does_not_advance_checkpoint() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("state.db");
        let connection = database(&database_path);
        connection.execute("INSERT INTO sessions VALUES ('one', 'gpt', 1700000000, NULL, 10, 0, 0, 0, 0, NULL, 'openai-codex')", []).unwrap();
        let cache =
            TokenEventCache::open_or_create(&directory.path().join("cache.sqlite")).unwrap();
        let scanner = HermesScanner::new(
            &database_path,
            TokenDeviceMetadata::local_fallback(),
            Some(&cache),
        );
        let checks = std::cell::Cell::new(0);
        let result = scanner.scan(|| {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        });
        assert!(result.events.is_empty());
        assert!(
            cache
                .hermes_checkpoints(
                    &canonical_text(&database_path),
                    TokenDeviceMetadata::LOCAL_ID
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn locked_database_returns_one_source_error_within_timeout() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("state.db");
        let connection = database(&database_path);
        connection
            .execute_batch(
                "PRAGMA journal_mode = DELETE;
                 BEGIN EXCLUSIVE;",
            )
            .unwrap();
        let scanner =
            HermesScanner::new(&database_path, TokenDeviceMetadata::local_fallback(), None);
        let started = std::time::Instant::now();
        let result = scanner.scan(|| false);
        connection.execute_batch("ROLLBACK").unwrap();
        assert!(result.events.is_empty());
        assert_eq!(result.parse_error_count, 1);
        assert!(started.elapsed() < Duration::from_secs(7));
    }
}
