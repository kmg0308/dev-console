use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    #[default]
    All,
    Codex,
    Claude,
}

impl TokenSource {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input: i64,
    pub cached_input: i64,
    pub cache_creation: i64,
    pub cache_read: i64,
    pub output: i64,
    pub reasoning: i64,
    pub total: i64,
}

impl TokenUsage {
    pub const ZERO: Self = Self {
        input: 0,
        cached_input: 0,
        cache_creation: 0,
        cache_read: 0,
        output: 0,
        reasoning: 0,
        total: 0,
    };

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input: i64,
        cached_input: i64,
        cache_creation: i64,
        cache_read: i64,
        output: i64,
        reasoning: i64,
        total: Option<i64>,
    ) -> Self {
        let input = input.max(0);
        let cached_input = cached_input.max(0).min(input);
        let cache_creation = cache_creation.max(0);
        let cache_read = cache_read.max(0);
        let output = output.max(0);
        let reasoning = reasoning.max(0);
        let total = total.map_or_else(
            || {
                [input, cache_creation, cache_read, output]
                    .into_iter()
                    .fold(0_i64, i64::saturating_add)
            },
            |value| value.max(0),
        );
        Self {
            input,
            cached_input,
            cache_creation,
            cache_read,
            output,
            reasoning,
            total,
        }
    }

    pub fn adding(self, other: Self) -> Self {
        Self::new(
            self.input.max(0).saturating_add(other.input.max(0)),
            self.cached_input
                .max(0)
                .saturating_add(other.cached_input.max(0)),
            self.cache_creation
                .max(0)
                .saturating_add(other.cache_creation.max(0)),
            self.cache_read
                .max(0)
                .saturating_add(other.cache_read.max(0)),
            self.output.max(0).saturating_add(other.output.max(0)),
            self.reasoning.max(0).saturating_add(other.reasoning.max(0)),
            Some(self.total.max(0).saturating_add(other.total.max(0))),
        )
    }

    pub fn display_components(self, source: TokenSource) -> Vec<TokenComponent> {
        let plain_input = if source == TokenSource::Claude {
            self.input
        } else {
            self.input.saturating_sub(self.cached_input).max(0)
        };
        let cache = if source == TokenSource::Claude {
            self.cache_creation.saturating_add(self.cache_read)
        } else {
            self.cached_input
                .saturating_add(self.cache_creation)
                .saturating_add(self.cache_read)
        };
        [
            TokenComponent::new(TokenComponentKind::Input, plain_input),
            TokenComponent::new(TokenComponentKind::Cache, cache),
            TokenComponent::new(
                TokenComponentKind::Output,
                self.output.saturating_sub(self.reasoning).max(0),
            ),
            TokenComponent::new(TokenComponentKind::Reasoning, self.reasoning),
        ]
        .into_iter()
        .filter(|component| component.value > 0)
        .collect()
    }
}

impl Default for TokenUsage {
    fn default() -> Self {
        Self::ZERO
    }
}

impl<'de> Deserialize<'de> for TokenUsage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct StoredUsage {
            input: Option<i64>,
            cached_input: Option<i64>,
            cache_creation: Option<i64>,
            cache_read: Option<i64>,
            output: Option<i64>,
            reasoning: Option<i64>,
            total: Option<i64>,
        }

        let stored = StoredUsage::deserialize(deserializer)?;
        Ok(Self::new(
            stored.input.unwrap_or_default(),
            stored.cached_input.unwrap_or_default(),
            stored.cache_creation.unwrap_or_default(),
            stored.cache_read.unwrap_or_default(),
            stored.output.unwrap_or_default(),
            stored.reasoning.unwrap_or_default(),
            stored.total.filter(|total| *total > 0),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TokenComponentKind {
    Input,
    Cache,
    Output,
    Reasoning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenComponent {
    pub kind: TokenComponentKind,
    pub value: i64,
}

impl TokenComponent {
    const fn new(kind: TokenComponentKind, value: i64) -> Self {
        Self { kind, value }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenEvent {
    pub id: String,
    pub source: TokenSource,
    #[serde(with = "swift_date")]
    pub timestamp: DateTime<Utc>,
    pub device_id: String,
    pub device_name: String,
    pub project_path: String,
    pub session_id: String,
    pub model: String,
    pub usage: TokenUsage,
    pub raw_file_path: String,
}

impl TokenEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        source: TokenSource,
        timestamp: DateTime<Utc>,
        device_id: impl Into<String>,
        device_name: impl Into<String>,
        project_path: impl Into<String>,
        session_id: impl Into<String>,
        model: impl Into<String>,
        usage: TokenUsage,
        raw_file_path: impl Into<String>,
    ) -> Self {
        let device_id = device_id.into();
        let device_name = device_name.into();
        Self {
            id: id.into(),
            source,
            timestamp,
            device_id: fallback(device_id, TokenDeviceMetadata::LOCAL_ID),
            device_name: fallback(device_name, TokenDeviceMetadata::LOCAL_NAME),
            project_path: fallback(project_path.into(), "Unknown"),
            session_id: fallback(session_id.into(), "Unknown"),
            model: fallback(model.into(), "Unknown"),
            usage,
            raw_file_path: raw_file_path.into(),
        }
    }

    pub fn with_device(mut self, device: &TokenDeviceMetadata) -> Self {
        self.device_id.clone_from(&device.id);
        self.device_name.clone_from(&device.name);
        self
    }
}

impl<'de> Deserialize<'de> for TokenEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StoredEvent {
            id: String,
            source: TokenSource,
            #[serde(with = "swift_date")]
            timestamp: DateTime<Utc>,
            device_id: Option<String>,
            device_name: Option<String>,
            project_path: Option<String>,
            session_id: Option<String>,
            model: Option<String>,
            usage: TokenUsage,
            raw_file_path: String,
        }

        let event = StoredEvent::deserialize(deserializer)?;
        Ok(Self::new(
            event.id,
            event.source,
            event.timestamp,
            event.device_id.unwrap_or_default(),
            event.device_name.unwrap_or_default(),
            event.project_path.unwrap_or_default(),
            event.session_id.unwrap_or_default(),
            event.model.unwrap_or_default(),
            event.usage,
            event.raw_file_path,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenDeviceMetadata {
    pub id: String,
    pub name: String,
}

impl TokenDeviceMetadata {
    pub const LOCAL_ID: &'static str = "local-device";
    pub const LOCAL_NAME: &'static str = "This Mac";

    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: fallback(id.into(), Self::LOCAL_ID),
            name: fallback(name.into(), Self::LOCAL_NAME),
        }
    }

    pub fn local_fallback() -> Self {
        Self::new(Self::LOCAL_ID, Self::LOCAL_NAME)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScanResult {
    pub events: Vec<TokenEvent>,
    pub sync_devices: Vec<TokenDeviceMetadata>,
    pub codex_file_count: usize,
    pub claude_file_count: usize,
    pub parse_error_count: usize,
    pub source_statuses: Vec<ScanSourceStatus>,
    pub sync_status: SyncFolderStatus,
    #[serde(with = "swift_date")]
    pub scanned_at: DateTime<Utc>,
}

impl Default for ScanResult {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            sync_devices: Vec::new(),
            codex_file_count: 0,
            claude_file_count: 0,
            parse_error_count: 0,
            source_statuses: Vec::new(),
            sync_status: SyncFolderStatus::default(),
            scanned_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncFolderStatus {
    pub path: Option<String>,
    pub exists: bool,
    pub device_file_count: usize,
    pub imported_event_count: usize,
    pub exported_event_count: usize,
    pub parse_error_count: usize,
    pub export_error: Option<String>,
    #[serde(with = "swift_date::option")]
    pub last_synced_at: Option<DateTime<Utc>>,
}

impl SyncFolderStatus {
    pub const fn is_configured(&self) -> bool {
        self.path.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScanSourceStatus {
    pub source: TokenSource,
    pub label: String,
    pub path: String,
    pub exists: bool,
    pub total_file_count: usize,
    pub scanned_file_count: usize,
    pub parse_error_count: usize,
}

impl Default for ScanSourceStatus {
    fn default() -> Self {
        Self {
            source: TokenSource::All,
            label: String::new(),
            path: String::new(),
            exists: false,
            total_file_count: 0,
            scanned_file_count: 0,
            parse_error_count: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeBucket {
    #[serde(with = "swift_date")]
    pub start: DateTime<Utc>,
    pub usage: TokenUsage,
    pub source_usage: BTreeMap<TokenSource, TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedUsageRow {
    pub key: String,
    pub usage: TokenUsage,
    pub count: usize,
    #[serde(with = "swift_date")]
    pub last_active: DateTime<Utc>,
}

fn fallback(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

pub(crate) mod swift_date {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer, de};

    const REFERENCE_UNIX_SECONDS: i64 = 978_307_200;

    pub fn serialize<S>(date: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let seconds = date.timestamp() - REFERENCE_UNIX_SECONDS;
        serializer.serialize_f64(seconds as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Timestamp::deserialize(deserializer)?
            .into_date()
            .map_err(de::Error::custom)
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Timestamp {
        Signed(i64),
        Unsigned(u64),
        Float(f64),
        Text(String),
    }

    impl Timestamp {
        fn into_date(self) -> Result<DateTime<Utc>, &'static str> {
            match self {
                Self::Signed(value) => from_reference_seconds(value as f64),
                Self::Unsigned(value) if value <= i64::MAX as u64 => {
                    from_reference_seconds(value as f64)
                }
                Self::Float(value) => from_reference_seconds(value),
                Self::Text(value) => parse_iso(&value).ok_or("invalid ISO timestamp"),
                Self::Unsigned(_) => Err("timestamp is out of range"),
            }
        }
    }

    fn from_reference_seconds(value: f64) -> Result<DateTime<Utc>, &'static str> {
        let unix = value + REFERENCE_UNIX_SECONDS as f64;
        if !unix.is_finite() || unix < i64::MIN as f64 || unix > i64::MAX as f64 {
            return Err("timestamp is out of range");
        }
        let mut seconds = unix.floor() as i64;
        let mut nanos = ((unix - seconds as f64) * 1e9).round() as u32;
        if nanos == 1_000_000_000 {
            seconds = seconds.checked_add(1).ok_or("timestamp is out of range")?;
            nanos = 0;
        }
        DateTime::from_timestamp(seconds, nanos).ok_or("timestamp is out of range")
    }

    pub(crate) fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
        let bytes = value.as_bytes();
        if bytes.len() < 20
            || bytes.get(4) != Some(&b'-')
            || bytes.get(7) != Some(&b'-')
            || bytes.get(10) != Some(&b'T')
            || bytes.get(13) != Some(&b':')
            || bytes.get(16) != Some(&b':')
            || std::str::from_utf8(&bytes[17..19])
                .ok()
                .and_then(|seconds| seconds.parse::<u8>().ok())
                .is_none_or(|seconds| seconds > 59)
        {
            return None;
        }

        let timezone_start = bytes[19..]
            .iter()
            .position(|byte| matches!(byte, b'Z' | b'+' | b'-'))?
            + 19;
        let fraction = &bytes[19..timezone_start];
        if !fraction.is_empty()
            && (fraction.first() != Some(&b'.')
                || fraction.len() == 1
                || !fraction[1..].iter().all(u8::is_ascii_digit))
        {
            return None;
        }
        let timezone = &bytes[timezone_start..];
        let normalized;
        let value = match timezone {
            b"Z" => value,
            [b'+' | b'-', hour1, hour2, minute1, minute2]
                if [hour1, hour2, minute1, minute2]
                    .into_iter()
                    .all(u8::is_ascii_digit) =>
            {
                normalized = format!(
                    "{}{}{}:{}",
                    &value[..timezone_start],
                    timezone[0] as char,
                    std::str::from_utf8(&timezone[1..3]).ok()?,
                    std::str::from_utf8(&timezone[3..]).ok()?
                );
                &normalized
            }
            [b'+' | b'-', hour1, hour2, b':', minute1, minute2]
                if [hour1, hour2, minute1, minute2]
                    .into_iter()
                    .all(u8::is_ascii_digit) =>
            {
                value
            }
            _ => return None,
        };
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|date| date.with_timezone(&Utc))
    }

    pub mod option {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S>(date: &Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match date {
                Some(date) => serializer.serialize_some(&Date(date)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<super::Timestamp>::deserialize(deserializer)?
                .map(super::Timestamp::into_date)
                .transpose()
                .map_err(serde::de::Error::custom)
        }

        struct Date<'a>(&'a DateTime<Utc>);

        impl serde::Serialize for Date<'_> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                super::serialize(self.0, serializer)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn usage_normalizes_and_saturates() {
        let usage = TokenUsage::new(-100, 50, 2, 3, 5, -2, None);
        assert_eq!(usage, TokenUsage::new(0, 0, 2, 3, 5, 0, Some(10)));
        assert_eq!(
            TokenUsage::new(i64::MAX, 0, 0, 0, 1, 0, None).total,
            i64::MAX
        );
        assert_eq!(
            TokenUsage::new(0, 0, 0, 0, 0, 0, Some(i64::MAX))
                .adding(TokenUsage::new(0, 0, 0, 0, 0, 0, Some(1)))
                .total,
            i64::MAX
        );
        let usage = TokenUsage::new(10, 50, i64::MAX, i64::MAX, 5, 1, None);
        assert_eq!(usage.cached_input, 10);
        assert_eq!(
            usage
                .display_components(TokenSource::Codex)
                .into_iter()
                .find(|component| component.kind == TokenComponentKind::Cache)
                .unwrap()
                .value,
            i64::MAX
        );
    }

    #[test]
    fn legacy_json_normalizes_fields_and_dates() {
        let numeric = r#"{
            "id":"legacy","source":"claude","timestamp":0,
            "deviceId":"","deviceName":"","projectPath":"","sessionId":"","model":"",
            "usage":{"input":-1,"cacheCreation":2,"output":3,"total":-1},
            "rawFilePath":"/tmp/legacy.jsonl"
        }"#;
        let event: TokenEvent = serde_json::from_str(numeric).unwrap();
        assert_eq!(
            event.timestamp,
            Utc.with_ymd_and_hms(2001, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(event.device_id, TokenDeviceMetadata::LOCAL_ID);
        assert_eq!(event.project_path, "Unknown");
        assert_eq!(event.usage.total, 5);
        assert!(serde_json::to_value(&event).unwrap()["timestamp"].is_number());

        let iso = numeric.replacen(
            "\"timestamp\":0",
            "\"timestamp\":\"2026-01-01T00:00:00.000Z\"",
            1,
        );
        let event: TokenEvent = serde_json::from_str(&iso).unwrap();
        assert_eq!(event.timestamp.timestamp(), 1_767_225_600);

        let nulls = numeric
            .replace("\"input\":-1", "\"input\":null")
            .replace("\"deviceId\":\"\"", "\"deviceId\":null")
            .replace("\"projectPath\":\"\"", "\"projectPath\":null");
        let event: TokenEvent = serde_json::from_str(&nulls).unwrap();
        assert_eq!(event.usage.input, 0);
        assert_eq!(event.device_id, TokenDeviceMetadata::LOCAL_ID);
        assert_eq!(event.project_path, "Unknown");
    }

    #[test]
    fn iso_parser_matches_swift_formatter_boundaries() {
        for valid in [
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00.123456Z",
            "2026-01-01T00:00:00+09:00",
            "2026-01-01T00:00:00+0900",
        ] {
            assert!(swift_date::parse_iso(valid).is_some(), "{valid}");
        }
        for invalid in [
            "2026-01-01 00:00:00Z",
            "2026-01-01t00:00:00z",
            "2026-01-01T00:00:00UTC",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:60Z",
        ] {
            assert!(swift_date::parse_iso(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn serde_uses_lower_camel_fields() {
        let usage = TokenUsage::new(10, 4, 2, 3, 5, 1, None);
        let json = serde_json::to_value(usage).unwrap();
        assert_eq!(json["cachedInput"], 4);
        assert_eq!(json["cacheCreation"], 2);
        assert!(json.get("cached_input").is_none());
    }
}
