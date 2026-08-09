use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc, Weekday};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    account::{CodexAccountUsage, CodexRateLimitWindow, CodexResetCreditSummary},
    aggregation::{self, BucketInterval, DateInterval, TimeRangePreset},
    models::{
        GroupedUsageRow, ScanResult, ScanSourceStatus, SyncFolderStatus, TokenDeviceMetadata,
        TokenEvent, TokenSource, TokenUsage,
    },
    settings::TokenMeterSettings,
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardRequest {
    pub source: TokenSource,
    pub range: String,
    pub bucket: String,
    #[serde(default)]
    pub filters: DashboardFilters,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardFilters {
    pub project: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardAccountState {
    Available(CodexAccountUsage),
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DashboardSnapshot {
    pub generated_at: DateTime<Utc>,
    pub selection: DashboardSelection,
    pub total: TokenUsage,
    pub previous_total: TokenUsage,
    pub change_percent: Option<f64>,
    pub buckets: Vec<DashboardBucket>,
    pub groups: DashboardGroups,
    pub filter_options: DashboardFilterOptions,
    pub source_statuses: Vec<ScanSourceStatus>,
    pub sync_status: SyncFolderStatus,
    pub codex_account: Option<DashboardAccount>,
    pub settings: DashboardSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSelection {
    pub source: TokenSource,
    pub range: String,
    pub bucket: String,
    pub filters: DashboardFilters,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardBucket {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub usage: TokenUsage,
    pub source_usage: BTreeMap<TokenSource, TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardUsageRow {
    pub key: String,
    pub usage: TokenUsage,
    pub event_count: usize,
    pub last_active: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardGroups {
    pub projects: Vec<DashboardUsageRow>,
    pub models: Vec<DashboardUsageRow>,
    pub sessions: Vec<DashboardUsageRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardFilterOptions {
    pub projects: Vec<String>,
    pub models: Vec<String>,
    pub devices: Vec<TokenDeviceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSettings {
    pub show_full_token_numbers: bool,
    pub sync_folder_path: Option<String>,
    pub local_device_id: String,
    pub local_device_name: String,
    pub codex_home: Option<String>,
    pub claude_projects_path: Option<String>,
    pub hermes_database_path: Option<String>,
    pub codex_executable_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardAccount {
    pub status: String,
    pub message: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub five_hour: Option<DashboardRateLimitWindow>,
    pub weekly: Option<DashboardRateLimitWindow>,
    pub reset_credits: Option<CodexResetCreditSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardRateLimitWindow {
    pub used_percent: i64,
    pub remaining_percent: i64,
    pub resets_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DashboardError {
    #[error("unsupported dashboard range: {0}")]
    InvalidRange(String),
    #[error("unsupported dashboard bucket: {0}")]
    InvalidBucket(String),
}

/// Builds a dashboard without reading files or depending on a UI/runtime.
///
/// Options and rows use locale-independent byte ordering (and grouped rows use
/// the stable ordering in `aggregation::grouped`). A UI may apply localized
/// sorting when it presents these values.
#[allow(clippy::too_many_arguments)]
pub fn compose_dashboard<Tz: TimeZone>(
    request: &DashboardRequest,
    scan: &ScanResult,
    settings: &TokenMeterSettings,
    local_device_name: &str,
    account: Option<&DashboardAccountState>,
    now: DateTime<Utc>,
    timezone: &Tz,
    first_weekday: Weekday,
) -> Result<DashboardSnapshot, DashboardError>
where
    Tz::Offset: Copy,
{
    let range = parse_range(&request.range)?;
    let bucket = parse_bucket(&request.bucket, range)?;
    let devices = device_options(scan, settings, local_device_name);
    let requested_device = request
        .filters
        .device
        .as_deref()
        .filter(|id| devices.iter().any(|device| device.id == *id));
    let normalized = aggregation::normalized_filters(
        &scan.events,
        request.source,
        range,
        request.filters.project.as_deref(),
        request.filters.model.as_deref(),
        requested_device,
        now,
        timezone,
    );
    let filters = DashboardFilters {
        project: normalized.project,
        model: normalized.model,
        device: requested_device.map(str::to_owned),
    };
    let filtered = aggregation::filter_range(
        &scan.events,
        request.source,
        range,
        filters.project.as_deref(),
        filters.model.as_deref(),
        filters.device.as_deref(),
        now,
        timezone,
    );
    let earliest = scan.events.iter().map(|event| event.timestamp).min();
    let latest = scan.events.iter().map(|event| event.timestamp).max();
    let interval = range.interval(now, timezone, earliest, latest);
    let previous_interval = range.previous_interval(now, timezone, earliest, latest);
    let previous_total = aggregation::total_usage(&aggregation::filter_interval(
        &scan.events,
        request.source,
        previous_interval,
        filters.project.as_deref(),
        filters.model.as_deref(),
        filters.device.as_deref(),
    ));
    let total = aggregation::total_usage(&filtered);
    let buckets = dashboard_buckets(&filtered, range, bucket, interval, timezone, first_weekday);
    let projects = rows(&filtered, |event| &event.project_path, 12);
    let models = rows(&filtered, |event| &event.model, 12);
    let sessions = rows(&filtered, |event| &event.session_id, 20);
    let (projects_options, models_options) =
        filter_options(&scan.events, request.source, range, &filters, now, timezone);

    let mut source_statuses = scan.source_statuses.clone();
    source_statuses.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(DashboardSnapshot {
        generated_at: now,
        selection: DashboardSelection {
            source: request.source,
            range: request.range.clone(),
            bucket: request.bucket.clone(),
            filters,
        },
        total,
        previous_total,
        change_percent: change_percent(total.total, previous_total.total),
        buckets,
        groups: DashboardGroups {
            projects,
            models,
            sessions,
        },
        filter_options: DashboardFilterOptions {
            projects: projects_options,
            models: models_options,
            devices,
        },
        source_statuses,
        sync_status: scan.sync_status.clone(),
        codex_account: account.map(DashboardAccount::from),
        settings: DashboardSettings {
            show_full_token_numbers: settings.show_full_token_numbers,
            sync_folder_path: settings.sync_folder_path.clone(),
            local_device_id: settings.local_device_id.clone(),
            local_device_name: local_device_name.to_owned(),
            codex_home: settings.codex_home.clone(),
            claude_projects_path: settings.claude_projects_path.clone(),
            hermes_database_path: settings.hermes_database_path.clone(),
            codex_executable_path: settings.codex_executable_path.clone(),
        },
    })
}

fn parse_range(raw: &str) -> Result<TimeRangePreset, DashboardError> {
    match raw {
        "30m" => Ok(TimeRangePreset::Last30Minutes),
        "1h" => Ok(TimeRangePreset::Last1Hour),
        "3h" => Ok(TimeRangePreset::Last3Hours),
        "6h" => Ok(TimeRangePreset::Last6Hours),
        "8h" => Ok(TimeRangePreset::Last8Hours),
        "12h" => Ok(TimeRangePreset::Last12Hours),
        "24h" => Ok(TimeRangePreset::Last24Hours),
        "Today" => Ok(TimeRangePreset::Today),
        "Yesterday" => Ok(TimeRangePreset::Yesterday),
        "7d" => Ok(TimeRangePreset::Last7Days),
        "30d" => Ok(TimeRangePreset::Last30Days),
        "3m" => Ok(TimeRangePreset::Last3Months),
        "6m" => Ok(TimeRangePreset::Last6Months),
        "12m" => Ok(TimeRangePreset::Last12Months),
        "All" => Ok(TimeRangePreset::All),
        _ => Err(DashboardError::InvalidRange(raw.to_owned())),
    }
}

fn parse_bucket(raw: &str, range: TimeRangePreset) -> Result<BucketInterval, DashboardError> {
    match raw {
        "auto" => Ok(match range {
            TimeRangePreset::Last30Minutes | TimeRangePreset::Last1Hour => BucketInterval::Minute,
            TimeRangePreset::Last3Hours => BucketInterval::FiveMinutes,
            TimeRangePreset::Last6Hours | TimeRangePreset::Last8Hours => BucketInterval::TenMinutes,
            TimeRangePreset::Last12Hours => BucketInterval::TwentyMinutes,
            TimeRangePreset::Today | TimeRangePreset::Yesterday | TimeRangePreset::Last24Hours => {
                BucketInterval::Hour
            }
            TimeRangePreset::Last7Days | TimeRangePreset::Last30Days => BucketInterval::Day,
            TimeRangePreset::Last3Months | TimeRangePreset::Last6Months => BucketInterval::Week,
            TimeRangePreset::Last12Months | TimeRangePreset::All => BucketInterval::Month,
        }),
        "1m" => Ok(BucketInterval::Minute),
        "5m" => Ok(BucketInterval::FiveMinutes),
        "10m" => Ok(BucketInterval::TenMinutes),
        "20m" => Ok(BucketInterval::TwentyMinutes),
        "30m" => Ok(BucketInterval::ThirtyMinutes),
        "1h" => Ok(BucketInterval::Hour),
        "1d" => Ok(BucketInterval::Day),
        "1w" => Ok(BucketInterval::Week),
        "1mo" => Ok(BucketInterval::Month),
        _ => Err(DashboardError::InvalidBucket(raw.to_owned())),
    }
}

fn dashboard_buckets<Tz: TimeZone>(
    events: &[TokenEvent],
    range: TimeRangePreset,
    bucket: BucketInterval,
    interval: DateInterval,
    timezone: &Tz,
    first_weekday: Weekday,
) -> Vec<DashboardBucket>
where
    Tz::Offset: Copy,
{
    let grouped = aggregation::buckets(events, bucket, timezone, first_weekday);
    let visible = if estimated_bucket_count(bucket, interval, timezone, first_weekday) > 2_000 {
        grouped
    } else {
        aggregation::filled_buckets(
            &grouped,
            range,
            bucket,
            interval,
            max_bucket_count(bucket),
            timezone,
            first_weekday,
        )
    };
    visible
        .into_iter()
        .filter_map(|item| {
            bucket
                .next_start(item.start, timezone)
                .map(|end| DashboardBucket {
                    start: item.start,
                    end,
                    usage: item.usage,
                    source_usage: item.source_usage,
                })
        })
        .collect()
}

fn estimated_bucket_count<Tz: TimeZone>(
    bucket: BucketInterval,
    interval: DateInterval,
    timezone: &Tz,
    first_weekday: Weekday,
) -> usize
where
    Tz::Offset: Copy,
{
    let end = bucket.start(interval.end, timezone, first_weekday);
    let mut current = bucket.start(interval.start, timezone, first_weekday);
    for count in 1..=2_001 {
        if current >= end {
            return count;
        }
        let Some(next) = bucket
            .next_start(current, timezone)
            .filter(|next| *next > current)
        else {
            return count;
        };
        current = next;
    }
    2_001
}

const fn max_bucket_count(bucket: BucketInterval) -> usize {
    match bucket {
        BucketInterval::Minute
        | BucketInterval::FiveMinutes
        | BucketInterval::TenMinutes
        | BucketInterval::TwentyMinutes
        | BucketInterval::ThirtyMinutes => 2_000,
        BucketInterval::Hour => 800,
        BucketInterval::Day | BucketInterval::Week | BucketInterval::Month => 400,
    }
}

fn rows(
    events: &[TokenEvent],
    key: impl Fn(&TokenEvent) -> &str,
    limit: usize,
) -> Vec<DashboardUsageRow> {
    aggregation::grouped(events, key)
        .into_iter()
        .take(limit)
        .map(DashboardUsageRow::from)
        .collect()
}

fn filter_options<Tz: TimeZone>(
    events: &[TokenEvent],
    source: TokenSource,
    range: TimeRangePreset,
    filters: &DashboardFilters,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> (Vec<String>, Vec<String>)
where
    Tz::Offset: Copy,
{
    let projects = aggregation::filter_range(
        events,
        source,
        range,
        None,
        filters.model.as_deref(),
        filters.device.as_deref(),
        now,
        timezone,
    )
    .into_iter()
    .map(|event| event.project_path)
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect();
    let models = aggregation::filter_range(
        events,
        source,
        range,
        filters.project.as_deref(),
        None,
        filters.device.as_deref(),
        now,
        timezone,
    )
    .into_iter()
    .map(|event| event.model)
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect();
    (projects, models)
}

fn device_options(
    scan: &ScanResult,
    settings: &TokenMeterSettings,
    local_device_name: &str,
) -> Vec<TokenDeviceMetadata> {
    let mut devices = BTreeMap::new();
    for device in &scan.sync_devices {
        devices.insert(device.id.clone(), device.name.clone());
    }
    for event in &scan.events {
        devices
            .entry(event.device_id.clone())
            .or_insert_with(|| event.device_name.clone());
    }
    devices.remove(&settings.local_device_id);
    std::iter::once(TokenDeviceMetadata::new(
        &settings.local_device_id,
        local_device_name,
    ))
    .chain(
        devices
            .into_iter()
            .map(|(id, name)| TokenDeviceMetadata::new(id, name)),
    )
    .collect()
}

fn change_percent(current: i64, previous: i64) -> Option<f64> {
    match (current, previous) {
        (0, 0) => Some(0.0),
        (_, 0) => None,
        _ => Some((current as f64 - previous as f64) / previous as f64 * 100.0),
    }
}

impl From<GroupedUsageRow> for DashboardUsageRow {
    fn from(row: GroupedUsageRow) -> Self {
        Self {
            key: row.key,
            usage: row.usage,
            event_count: row.count,
            last_active: row.last_active,
        }
    }
}

impl From<&DashboardAccountState> for DashboardAccount {
    fn from(state: &DashboardAccountState) -> Self {
        match state {
            DashboardAccountState::Available(usage) => Self {
                status: "available".to_owned(),
                message: None,
                fetched_at: Some(usage.fetched_at),
                five_hour: usage.five_hour_window.as_ref().map(Into::into),
                weekly: usage.seven_day_window.as_ref().map(Into::into),
                reset_credits: usage.reset_credits.clone(),
            },
            DashboardAccountState::Unavailable(message) => Self {
                status: "unavailable".to_owned(),
                message: Some(message.clone()),
                fetched_at: None,
                five_hour: None,
                weekly: None,
                reset_credits: None,
            },
        }
    }
}

impl From<&CodexRateLimitWindow> for DashboardRateLimitWindow {
    fn from(window: &CodexRateLimitWindow) -> Self {
        Self {
            used_percent: window.used_percent,
            remaining_percent: window.remaining_percent(),
            resets_at: window.resets_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSnapshotDto {
    pub generated_at: DateTime<Utc>,
    pub selection: DashboardSelection,
    pub total: WireTokenUsage,
    pub previous_total: WireTokenUsage,
    pub change_percent: Option<f64>,
    pub buckets: Vec<DashboardBucketDto>,
    pub groups: DashboardGroupsDto,
    pub filter_options: DashboardFilterOptions,
    pub source_statuses: Vec<ScanSourceStatus>,
    pub sync_status: SyncFolderStatus,
    pub codex_account: Option<DashboardAccount>,
    pub settings: DashboardSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTokenUsage {
    pub input: String,
    pub cached_input: String,
    pub cache_creation: String,
    pub cache_read: String,
    pub output: String,
    pub reasoning: String,
    pub total: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardBucketDto {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub usage: WireTokenUsage,
    pub source_usage: BTreeMap<TokenSource, WireTokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardUsageRowDto {
    pub key: String,
    pub usage: WireTokenUsage,
    pub event_count: usize,
    pub last_active: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardGroupsDto {
    pub projects: Vec<DashboardUsageRowDto>,
    pub models: Vec<DashboardUsageRowDto>,
    pub sessions: Vec<DashboardUsageRowDto>,
}

impl From<DashboardSnapshot> for DashboardSnapshotDto {
    fn from(snapshot: DashboardSnapshot) -> Self {
        Self {
            generated_at: snapshot.generated_at,
            selection: snapshot.selection,
            total: snapshot.total.into(),
            previous_total: snapshot.previous_total.into(),
            change_percent: snapshot.change_percent,
            buckets: snapshot.buckets.into_iter().map(Into::into).collect(),
            groups: snapshot.groups.into(),
            filter_options: snapshot.filter_options,
            source_statuses: snapshot.source_statuses,
            sync_status: snapshot.sync_status,
            codex_account: snapshot.codex_account,
            settings: snapshot.settings,
        }
    }
}

impl From<TokenUsage> for WireTokenUsage {
    fn from(usage: TokenUsage) -> Self {
        Self {
            input: usage.input.to_string(),
            cached_input: usage.cached_input.to_string(),
            cache_creation: usage.cache_creation.to_string(),
            cache_read: usage.cache_read.to_string(),
            output: usage.output.to_string(),
            reasoning: usage.reasoning.to_string(),
            total: usage.total.to_string(),
        }
    }
}

impl From<DashboardBucket> for DashboardBucketDto {
    fn from(bucket: DashboardBucket) -> Self {
        Self {
            start: bucket.start,
            end: bucket.end,
            usage: bucket.usage.into(),
            source_usage: bucket
                .source_usage
                .into_iter()
                .map(|(source, usage)| (source, usage.into()))
                .collect(),
        }
    }
}

impl From<DashboardUsageRow> for DashboardUsageRowDto {
    fn from(row: DashboardUsageRow) -> Self {
        Self {
            key: row.key,
            usage: row.usage.into(),
            event_count: row.event_count,
            last_active: row.last_active,
        }
    }
}

impl From<DashboardGroups> for DashboardGroupsDto {
    fn from(groups: DashboardGroups) -> Self {
        Self {
            projects: groups.projects.into_iter().map(Into::into).collect(),
            models: groups.models.into_iter().map(Into::into).collect(),
            sessions: groups.sessions.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, FixedOffset, TimeZone};

    use super::*;
    use crate::settings::SETTINGS_SCHEMA_VERSION;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 9, hour, minute, 0).unwrap()
    }

    fn event(
        id: &str,
        timestamp: DateTime<Utc>,
        project: &str,
        model: &str,
        total: i64,
    ) -> TokenEvent {
        TokenEvent::new(
            id,
            TokenSource::Codex,
            timestamp,
            "local",
            "Mac",
            project,
            id,
            model,
            TokenUsage::new(0, 0, 0, 0, 0, 0, Some(total)),
            format!("/{id}.jsonl"),
        )
    }

    fn settings() -> TokenMeterSettings {
        TokenMeterSettings {
            schema_version: SETTINGS_SCHEMA_VERSION,
            show_full_token_numbers: false,
            local_device_id: "local".into(),
            sync_folder_path: None,
            codex_home: None,
            claude_projects_path: None,
            hermes_database_path: None,
            codex_executable_path: None,
        }
    }

    fn request(range: &str, bucket: &str) -> DashboardRequest {
        DashboardRequest {
            source: TokenSource::All,
            range: range.into(),
            bucket: bucket.into(),
            filters: DashboardFilters::default(),
        }
    }

    #[test]
    fn preserves_legacy_automatic_bucket_mapping() {
        let cases = [
            ("30m", BucketInterval::Minute),
            ("1h", BucketInterval::Minute),
            ("3h", BucketInterval::FiveMinutes),
            ("6h", BucketInterval::TenMinutes),
            ("8h", BucketInterval::TenMinutes),
            ("12h", BucketInterval::TwentyMinutes),
            ("24h", BucketInterval::Hour),
            ("Today", BucketInterval::Hour),
            ("Yesterday", BucketInterval::Hour),
            ("7d", BucketInterval::Day),
            ("30d", BucketInterval::Day),
            ("3m", BucketInterval::Week),
            ("6m", BucketInterval::Week),
            ("12m", BucketInterval::Month),
            ("All", BucketInterval::Month),
        ];
        for (range, expected) in cases {
            assert_eq!(
                parse_bucket("auto", parse_range(range).unwrap()),
                Ok(expected)
            );
        }
    }

    #[test]
    fn normalizes_filters_compares_previous_and_emits_bucket_ends() {
        let now = at(12, 0);
        let scan = ScanResult {
            events: vec![
                event("previous", now - Duration::minutes(90), "alpha", "m2", 10),
                event("current", now - Duration::minutes(30), "beta", "m2", 20),
            ],
            ..ScanResult::default()
        };
        let mut request = request("1h", "30m");
        request.filters.project = Some("alpha".into());
        request.filters.model = Some("m2".into());
        let timezone = FixedOffset::east_opt(0).unwrap();

        let snapshot = compose_dashboard(
            &request,
            &scan,
            &settings(),
            "This Mac",
            None,
            now,
            &timezone,
            Weekday::Mon,
        )
        .unwrap();

        assert_eq!(
            snapshot.selection.filters,
            DashboardFilters {
                project: None,
                model: Some("m2".into()),
                device: None,
            }
        );
        assert_eq!(snapshot.total.total, 20);
        assert_eq!(snapshot.previous_total.total, 10);
        assert_eq!(snapshot.change_percent, Some(100.0));
        assert_eq!(snapshot.buckets.len(), 2);
        assert_eq!(snapshot.buckets[0].start, at(11, 0));
        assert_eq!(snapshot.buckets[0].end, at(11, 30));
        assert_eq!(snapshot.buckets[1].end, at(12, 0));
    }

    #[test]
    fn wire_usage_keeps_i64_max_exact() {
        let snapshot = DashboardSnapshot {
            generated_at: at(12, 0),
            selection: DashboardSelection {
                source: TokenSource::All,
                range: "All".into(),
                bucket: "auto".into(),
                filters: DashboardFilters::default(),
            },
            total: TokenUsage::new(
                i64::MAX,
                i64::MAX,
                i64::MAX,
                i64::MAX,
                i64::MAX,
                i64::MAX,
                Some(i64::MAX),
            ),
            previous_total: TokenUsage::ZERO,
            change_percent: None,
            buckets: vec![],
            groups: DashboardGroups {
                projects: vec![],
                models: vec![],
                sessions: vec![],
            },
            filter_options: DashboardFilterOptions {
                projects: vec![],
                models: vec![],
                devices: vec![],
            },
            source_statuses: vec![],
            sync_status: SyncFolderStatus::default(),
            codex_account: None,
            settings: DashboardSettings {
                show_full_token_numbers: false,
                sync_folder_path: None,
                local_device_id: "local".into(),
                local_device_name: "Mac".into(),
                codex_home: None,
                claude_projects_path: None,
                hermes_database_path: None,
                codex_executable_path: None,
            },
        };
        let json = serde_json::to_value(DashboardSnapshotDto::from(snapshot)).unwrap();
        for field in [
            "input",
            "cachedInput",
            "cacheCreation",
            "cacheRead",
            "output",
            "reasoning",
            "total",
        ] {
            assert_eq!(json["total"][field], i64::MAX.to_string());
            assert!(json["total"][field].is_string());
        }
    }
}
