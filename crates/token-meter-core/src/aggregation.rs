use std::{cmp::Ordering, collections::BTreeMap};

use chrono::{
    DateTime, Datelike, Days, Duration, LocalResult, Months, NaiveDateTime, TimeZone, Timelike,
    Utc, Weekday,
};
use serde::{Deserialize, Serialize};

use crate::models::{GroupedUsageRow, TimeBucket, TokenEvent, TokenSource, TokenUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeRangePreset {
    #[serde(rename = "Today")]
    Today,
    #[serde(rename = "Yesterday")]
    Yesterday,
    #[serde(rename = "30m")]
    Last30Minutes,
    #[serde(rename = "1h")]
    Last1Hour,
    #[serde(rename = "3h")]
    Last3Hours,
    #[serde(rename = "6h")]
    Last6Hours,
    #[serde(rename = "8h")]
    Last8Hours,
    #[serde(rename = "12h")]
    Last12Hours,
    #[serde(rename = "24h")]
    Last24Hours,
    #[serde(rename = "7d")]
    Last7Days,
    #[serde(rename = "30d")]
    Last30Days,
    #[serde(rename = "3m")]
    Last3Months,
    #[serde(rename = "6m")]
    Last6Months,
    #[serde(rename = "12m")]
    Last12Months,
    #[serde(rename = "All")]
    All,
}

impl TimeRangePreset {
    pub const DASHBOARD_CASES: [Self; 15] = [
        Self::Last30Minutes,
        Self::Last1Hour,
        Self::Last3Hours,
        Self::Last6Hours,
        Self::Last8Hours,
        Self::Last12Hours,
        Self::Last24Hours,
        Self::Today,
        Self::Yesterday,
        Self::Last7Days,
        Self::Last30Days,
        Self::Last3Months,
        Self::Last6Months,
        Self::Last12Months,
        Self::All,
    ];

    pub fn interval<Tz: TimeZone>(
        self,
        now: DateTime<Utc>,
        timezone: &Tz,
        earliest: Option<DateTime<Utc>>,
        latest: Option<DateTime<Utc>>,
    ) -> DateInterval
    where
        Tz::Offset: Copy,
    {
        match self {
            Self::Today => DateInterval::new(start_of_day(now, timezone), now),
            Self::Yesterday => {
                let today = start_of_day(now, timezone);
                DateInterval::new(add_local_days(today, -1, timezone).unwrap_or(today), today)
            }
            Self::Last30Minutes => DateInterval::new(now - Duration::minutes(30), now),
            Self::Last1Hour => DateInterval::new(now - Duration::hours(1), now),
            Self::Last3Hours => DateInterval::new(now - Duration::hours(3), now),
            Self::Last6Hours => DateInterval::new(now - Duration::hours(6), now),
            Self::Last8Hours => DateInterval::new(now - Duration::hours(8), now),
            Self::Last12Hours => DateInterval::new(now - Duration::hours(12), now),
            Self::Last24Hours => DateInterval::new(now - Duration::hours(24), now),
            Self::Last7Days => {
                let today = start_of_day(now, timezone);
                DateInterval::new(add_local_days(today, -6, timezone).unwrap_or(today), now)
            }
            Self::Last30Days => {
                let today = start_of_day(now, timezone);
                DateInterval::new(add_local_days(today, -29, timezone).unwrap_or(today), now)
            }
            Self::Last3Months => DateInterval::new(subtract_months(now, 3, timezone), now),
            Self::Last6Months => DateInterval::new(subtract_months(now, 6, timezone), now),
            Self::Last12Months => DateInterval::new(subtract_months(now, 12, timezone), now),
            Self::All => {
                let start = earliest.or(latest).unwrap_or(DateTime::UNIX_EPOCH);
                let end = latest.unwrap_or(now);
                DateInterval::new(start.min(end), start.max(end))
            }
        }
    }

    pub fn previous_interval<Tz: TimeZone>(
        self,
        now: DateTime<Utc>,
        timezone: &Tz,
        earliest: Option<DateTime<Utc>>,
        latest: Option<DateTime<Utc>>,
    ) -> DateInterval
    where
        Tz::Offset: Copy,
    {
        let current = self.interval(now, timezone, earliest, latest);
        match self {
            Self::Today => {
                let start = add_local_days(current.start, -1, timezone)
                    .unwrap_or(current.start - current.duration());
                DateInterval::new(start, (start + current.duration()).min(current.start))
            }
            Self::Yesterday => DateInterval::new(
                add_local_days(current.start, -1, timezone)
                    .unwrap_or(current.start - current.duration()),
                current.start,
            ),
            Self::All => DateInterval::new(current.start, current.start),
            _ => DateInterval::new(current.start - current.duration(), current.start),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BucketInterval {
    #[serde(rename = "1m")]
    Minute,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "10m")]
    TenMinutes,
    #[serde(rename = "20m")]
    TwentyMinutes,
    #[serde(rename = "30m")]
    ThirtyMinutes,
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "1d")]
    Day,
    #[serde(rename = "1w")]
    Week,
    #[serde(rename = "1mo")]
    Month,
}

impl BucketInterval {
    pub const DASHBOARD_CASES: [Self; 9] = [
        Self::Minute,
        Self::FiveMinutes,
        Self::TenMinutes,
        Self::TwentyMinutes,
        Self::ThirtyMinutes,
        Self::Hour,
        Self::Day,
        Self::Week,
        Self::Month,
    ];

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Minute => "1 min",
            Self::FiveMinutes => "5 min",
            Self::TenMinutes => "10 min",
            Self::TwentyMinutes => "20 min",
            Self::ThirtyMinutes => "30 min",
            Self::Hour => "Hourly",
            Self::Day => "Daily",
            Self::Week => "Weekly",
            Self::Month => "Monthly",
        }
    }

    pub fn start<Tz: TimeZone>(
        self,
        date: DateTime<Utc>,
        timezone: &Tz,
        first_weekday: Weekday,
    ) -> DateTime<Utc>
    where
        Tz::Offset: Copy,
    {
        let local = date.with_timezone(timezone);
        let naive = match self {
            Self::Minute => local
                .naive_local()
                .with_second(0)
                .and_then(|date| date.with_nanosecond(0)),
            Self::FiveMinutes => minute_bucket(local.naive_local(), 5),
            Self::TenMinutes => minute_bucket(local.naive_local(), 10),
            Self::TwentyMinutes => minute_bucket(local.naive_local(), 20),
            Self::ThirtyMinutes => minute_bucket(local.naive_local(), 30),
            Self::Hour => local
                .naive_local()
                .with_minute(0)
                .and_then(|date| date.with_second(0))
                .and_then(|date| date.with_nanosecond(0)),
            Self::Day => local.date_naive().and_hms_opt(0, 0, 0),
            Self::Week => local
                .date_naive()
                .checked_sub_days(Days::new(days_from_week_start(
                    local.weekday(),
                    first_weekday,
                )))
                .and_then(|date| date.and_hms_opt(0, 0, 0)),
            Self::Month => local
                .date_naive()
                .with_day(1)
                .and_then(|date| date.and_hms_opt(0, 0, 0)),
        };
        naive
            .and_then(|date| resolve_local(timezone, date))
            .unwrap_or(date)
    }

    pub fn next_start<Tz: TimeZone>(
        self,
        date: DateTime<Utc>,
        timezone: &Tz,
    ) -> Option<DateTime<Utc>>
    where
        Tz::Offset: Copy,
    {
        match self {
            Self::Minute => date.checked_add_signed(Duration::minutes(1)),
            Self::FiveMinutes => date.checked_add_signed(Duration::minutes(5)),
            Self::TenMinutes => date.checked_add_signed(Duration::minutes(10)),
            Self::TwentyMinutes => date.checked_add_signed(Duration::minutes(20)),
            Self::ThirtyMinutes => date.checked_add_signed(Duration::minutes(30)),
            Self::Hour => date.checked_add_signed(Duration::hours(1)),
            Self::Day => add_local_days(date, 1, timezone),
            Self::Week => add_local_days(date, 7, timezone),
            Self::Month => {
                let local = date.with_timezone(timezone);
                local
                    .naive_local()
                    .checked_add_months(Months::new(1))
                    .and_then(|next| resolve_local(timezone, next))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateInterval {
    #[serde(with = "crate::models::swift_date")]
    pub start: DateTime<Utc>,
    #[serde(with = "crate::models::swift_date")]
    pub end: DateTime<Utc>,
}

impl DateInterval {
    pub const fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        Self { start, end }
    }

    pub fn duration(self) -> Duration {
        self.end - self.start
    }

    pub fn contains(self, timestamp: DateTime<Utc>) -> bool {
        timestamp >= self.start && timestamp < self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenFilterSelection {
    pub project: Option<String>,
    pub model: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn filter_range<Tz: TimeZone>(
    events: &[TokenEvent],
    source: TokenSource,
    range: TimeRangePreset,
    project: Option<&str>,
    model: Option<&str>,
    device_id: Option<&str>,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> Vec<TokenEvent>
where
    Tz::Offset: Copy,
{
    let interval =
        (range != TimeRangePreset::All).then(|| range.interval(now, timezone, None, None));
    events
        .iter()
        .filter(|event| {
            matches_filters(event, source, project, model, device_id)
                && interval.is_none_or(|interval| interval.contains(event.timestamp))
        })
        .cloned()
        .collect()
}

pub fn filter_interval(
    events: &[TokenEvent],
    source: TokenSource,
    interval: DateInterval,
    project: Option<&str>,
    model: Option<&str>,
    device_id: Option<&str>,
) -> Vec<TokenEvent> {
    events
        .iter()
        .filter(|event| {
            interval.contains(event.timestamp)
                && matches_filters(event, source, project, model, device_id)
        })
        .cloned()
        .collect()
}

pub fn total_usage(events: &[TokenEvent]) -> TokenUsage {
    events
        .iter()
        .fold(TokenUsage::ZERO, |total, event| total.adding(event.usage))
}

#[allow(clippy::too_many_arguments)]
pub fn normalized_filters<Tz: TimeZone>(
    events: &[TokenEvent],
    source: TokenSource,
    range: TimeRangePreset,
    project: Option<&str>,
    model: Option<&str>,
    device_id: Option<&str>,
    now: DateTime<Utc>,
    timezone: &Tz,
) -> TokenFilterSelection
where
    Tz::Offset: Copy,
{
    let base = filter_range(events, source, range, None, None, device_id, now, timezone);
    let mut normalized_project = project;
    let mut normalized_model = model;

    if model.is_some_and(|model| !base.iter().any(|event| event.model == model)) {
        normalized_model = None;
    }
    if project.is_some_and(|project| {
        !base.iter().any(|event| {
            event.project_path == project
                && normalized_model.is_none_or(|model| event.model == model)
        })
    }) {
        normalized_project = None;
    }
    if normalized_model.is_some_and(|model| {
        !base.iter().any(|event| {
            event.model == model
                && normalized_project.is_none_or(|project| event.project_path == project)
        })
    }) {
        normalized_model = None;
    }

    TokenFilterSelection {
        project: normalized_project.map(str::to_owned),
        model: normalized_model.map(str::to_owned),
    }
}

pub fn buckets<Tz: TimeZone>(
    events: &[TokenEvent],
    bucket: BucketInterval,
    timezone: &Tz,
    first_weekday: Weekday,
) -> Vec<TimeBucket>
where
    Tz::Offset: Copy,
{
    let mut grouped: BTreeMap<DateTime<Utc>, BTreeMap<TokenSource, TokenUsage>> = BTreeMap::new();
    for event in events {
        let source_usage = grouped
            .entry(bucket.start(event.timestamp, timezone, first_weekday))
            .or_default();
        source_usage
            .entry(event.source)
            .and_modify(|usage| *usage = usage.adding(event.usage))
            .or_insert(event.usage);
    }
    grouped
        .into_iter()
        .map(|(start, source_usage)| TimeBucket {
            start,
            usage: source_usage
                .values()
                .copied()
                .fold(TokenUsage::ZERO, TokenUsage::adding),
            source_usage,
        })
        .collect()
}

pub fn filled_buckets<Tz: TimeZone>(
    buckets: &[TimeBucket],
    range: TimeRangePreset,
    bucket: BucketInterval,
    interval: DateInterval,
    max_count: usize,
    timezone: &Tz,
    first_weekday: Weekday,
) -> Vec<TimeBucket>
where
    Tz::Offset: Copy,
{
    if buckets.is_empty() || max_count == 0 {
        return Vec::new();
    }
    let start = bucket.start(interval.start, timezone, first_weekday);
    let end = bucket.start(interval.end, timezone, first_weekday);
    let include_end = range == TimeRangePreset::All || end < interval.end;
    let existing: BTreeMap<_, _> = buckets
        .iter()
        .cloned()
        .map(|bucket| (bucket.start, bucket))
        .collect();
    let mut result = Vec::new();
    let mut current = start;

    while (current < end || (include_end && current == end)) && result.len() < max_count {
        result.push(existing.get(&current).cloned().unwrap_or(TimeBucket {
            start: current,
            usage: TokenUsage::ZERO,
            source_usage: BTreeMap::new(),
        }));
        let Some(next) = bucket
            .next_start(current, timezone)
            .filter(|next| *next > current)
        else {
            break;
        };
        current = next;
    }
    result
}

pub fn grouped(events: &[TokenEvent], key: impl Fn(&TokenEvent) -> &str) -> Vec<GroupedUsageRow> {
    let mut rows: BTreeMap<String, GroupedUsageRow> = BTreeMap::new();
    for event in events {
        let key = key(event).to_owned();
        rows.entry(key.clone())
            .and_modify(|row| {
                row.usage = row.usage.adding(event.usage);
                row.count = row.count.saturating_add(1);
                row.last_active = row.last_active.max(event.timestamp);
            })
            .or_insert(GroupedUsageRow {
                key,
                usage: event.usage,
                count: 1,
                last_active: event.timestamp,
            });
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|left, right| {
        right
            .usage
            .total
            .cmp(&left.usage.total)
            .then_with(|| right.last_active.cmp(&left.last_active))
            .then_with(|| compare_case_insensitive(&left.key, &right.key))
    });
    rows
}

fn matches_filters(
    event: &TokenEvent,
    source: TokenSource,
    project: Option<&str>,
    model: Option<&str>,
    device_id: Option<&str>,
) -> bool {
    (source == TokenSource::All || event.source == source)
        && project.is_none_or(|project| event.project_path == project)
        && model.is_none_or(|model| event.model == model)
        && device_id.is_none_or(|device_id| event.device_id == device_id)
}

fn compare_case_insensitive(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

fn minute_bucket(date: NaiveDateTime, size: u32) -> Option<NaiveDateTime> {
    date.with_minute((date.minute() / size) * size)
        .and_then(|date| date.with_second(0))
        .and_then(|date| date.with_nanosecond(0))
}

fn days_from_week_start(weekday: Weekday, first_weekday: Weekday) -> u64 {
    u64::from((weekday.num_days_from_monday() + 7 - first_weekday.num_days_from_monday()) % 7)
}

fn start_of_day<Tz: TimeZone>(date: DateTime<Utc>, timezone: &Tz) -> DateTime<Utc>
where
    Tz::Offset: Copy,
{
    date.with_timezone(timezone)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|date| resolve_local(timezone, date))
        .unwrap_or(date)
}

fn add_local_days<Tz: TimeZone>(
    date: DateTime<Utc>,
    days: i64,
    timezone: &Tz,
) -> Option<DateTime<Utc>>
where
    Tz::Offset: Copy,
{
    let local = date.with_timezone(timezone).naive_local();
    let next = if days >= 0 {
        local.checked_add_days(Days::new(days as u64))
    } else {
        local.checked_sub_days(Days::new(days.unsigned_abs()))
    }?;
    resolve_local(timezone, next)
}

fn subtract_months<Tz: TimeZone>(date: DateTime<Utc>, months: u32, timezone: &Tz) -> DateTime<Utc>
where
    Tz::Offset: Copy,
{
    date.with_timezone(timezone)
        .naive_local()
        .checked_sub_months(Months::new(months))
        .and_then(|date| resolve_local(timezone, date))
        .unwrap_or(date)
}

fn resolve_local<Tz: TimeZone>(timezone: &Tz, date: NaiveDateTime) -> Option<DateTime<Utc>>
where
    Tz::Offset: Copy,
{
    match timezone.from_local_datetime(&date) {
        LocalResult::Single(date) | LocalResult::Ambiguous(date, _) => {
            return Some(date.with_timezone(&Utc));
        }
        LocalResult::None => {}
    }

    // ponytail: timezone APIs expose no transition lookup; scan at most two days,
    // and replace this only if chrono gains a direct first-valid-local-time API.
    let date = date.with_nanosecond(0)?;
    for seconds in 1..=48 * 60 * 60 {
        let candidate = date.checked_add_signed(Duration::seconds(seconds))?;
        match timezone.from_local_datetime(&candidate) {
            LocalResult::Single(date) | LocalResult::Ambiguous(date, _) => {
                return Some(date.with_timezone(&Utc));
            }
            LocalResult::None => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveDate, TimeZone};

    use super::*;
    use crate::models::TokenDeviceMetadata;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
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
            TokenDeviceMetadata::LOCAL_ID,
            TokenDeviceMetadata::LOCAL_NAME,
            project,
            id,
            model,
            TokenUsage::new(0, 0, 0, 0, 0, 0, Some(total)),
            format!("/tmp/{id}.jsonl"),
        )
    }

    #[test]
    fn ranges_use_exclusive_end_and_include_relative_days() {
        let timezone = FixedOffset::east_opt(0).unwrap();
        let now = utc(2026, 5, 11, 17, 37);
        let seven_days = TimeRangePreset::Last7Days.interval(now, &timezone, None, None);
        assert_eq!(seven_days.start, utc(2026, 5, 5, 0, 0));
        let future = utc(2026, 5, 12, 17, 37);
        let all = TimeRangePreset::All.interval(now, &timezone, Some(future), None);
        assert_eq!(all, DateInterval::new(now, future));
        assert_eq!(
            TimeRangePreset::All.previous_interval(now, &timezone, Some(future), None),
            DateInterval::new(now, now)
        );
        let midnight = utc(2026, 5, 11, 0, 0);
        let events = [
            event("before", midnight - Duration::seconds(1), "p", "m", 10),
            event("at", midnight, "p", "m", 20),
        ];
        assert_eq!(
            filter_range(
                &events,
                TokenSource::All,
                TimeRangePreset::Yesterday,
                None,
                None,
                None,
                utc(2026, 5, 11, 12, 0),
                &timezone,
            )[0]
            .id,
            "before"
        );
    }

    #[test]
    fn buckets_round_down_and_do_not_fill_exclusive_end() {
        let timezone = FixedOffset::east_opt(0).unwrap();
        let events = [
            event("a", utc(2026, 5, 11, 17, 7), "p", "m", 10),
            event("b", utc(2026, 5, 11, 17, 9), "p", "m", 20),
            event("c", utc(2026, 5, 11, 17, 11), "p", "m", 30),
        ];
        let buckets = buckets(
            &events,
            BucketInterval::FiveMinutes,
            &timezone,
            Weekday::Sun,
        );
        assert_eq!(
            buckets
                .iter()
                .map(|bucket| bucket.start)
                .collect::<Vec<_>>(),
            [utc(2026, 5, 11, 17, 5), utc(2026, 5, 11, 17, 10)]
        );
        assert_eq!(buckets[0].usage.total, 30);
        assert_eq!(
            BucketInterval::Week.start(utc(2026, 5, 11, 17, 7), &timezone, Weekday::Sun),
            utc(2026, 5, 10, 0, 0)
        );
        assert_eq!(
            BucketInterval::Week.start(utc(2026, 5, 11, 17, 7), &timezone, Weekday::Mon),
            utc(2026, 5, 11, 0, 0)
        );
        assert_eq!(
            filled_buckets(
                &buckets[..1],
                TimeRangePreset::Yesterday,
                BucketInterval::Day,
                DateInterval::new(utc(2026, 5, 10, 0, 0), utc(2026, 5, 11, 0, 0)),
                10,
                &timezone,
                Weekday::Sun,
            )
            .len(),
            1
        );
    }

    #[derive(Clone)]
    struct MidnightGap;

    impl TimeZone for MidnightGap {
        type Offset = FixedOffset;

        fn from_offset(_: &Self::Offset) -> Self {
            Self
        }

        fn offset_from_local_date(&self, _: &NaiveDate) -> LocalResult<Self::Offset> {
            LocalResult::Single(FixedOffset::east_opt(0).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let gap_start = NaiveDate::from_ymd_opt(2018, 11, 4)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap();
            let gap_end = gap_start + Duration::hours(1);
            if *local >= gap_start && *local < gap_end {
                LocalResult::None
            } else {
                LocalResult::Single(FixedOffset::east_opt(0).unwrap())
            }
        }

        fn offset_from_utc_date(&self, _: &NaiveDate) -> Self::Offset {
            FixedOffset::east_opt(0).unwrap()
        }

        fn offset_from_utc_datetime(&self, _: &NaiveDateTime) -> Self::Offset {
            FixedOffset::east_opt(0).unwrap()
        }
    }

    #[test]
    fn nonexistent_midnight_resolves_to_first_valid_local_instant() {
        let date = utc(2018, 11, 4, 12, 0);
        assert_eq!(
            BucketInterval::Day.start(date, &MidnightGap, Weekday::Sun),
            utc(2018, 11, 4, 1, 0)
        );
    }

    #[test]
    fn grouping_and_filter_normalization_are_stable() {
        let timezone = FixedOffset::east_opt(0).unwrap();
        let early = utc(2026, 1, 1, 10, 0);
        let late = utc(2026, 1, 2, 10, 0);
        let events = [
            event("alpha", early, "Alpha", "m1", 10),
            event("beta", late, "Beta", "m2", 10),
            event("gamma", early, "Gamma", "m3", 20),
        ];
        assert_eq!(
            grouped(&events, |event| &event.project_path)
                .into_iter()
                .map(|row| row.key)
                .collect::<Vec<_>>(),
            ["Gamma", "Beta", "Alpha"]
        );
        assert_eq!(
            normalized_filters(
                &events,
                TokenSource::All,
                TimeRangePreset::Last24Hours,
                Some("Alpha"),
                Some("m2"),
                None,
                late + Duration::hours(1),
                &timezone,
            ),
            TokenFilterSelection {
                project: None,
                model: Some("m2".to_owned())
            }
        );
    }
}
