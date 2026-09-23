use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::models::{TokenDeviceMetadata, TokenEvent, TokenSource, TokenUsage, swift_date};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("incremental parse must restart from the beginning of the file")]
    RequiresFullFile,
    #[error("failed to read token log {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn parse_codex_file(
    path: impl AsRef<Path>,
    start_offset: u64,
    is_cancelled: impl Fn() -> bool,
) -> Result<Vec<TokenEvent>, ParseError> {
    let path = path.as_ref();
    let path_text = path.to_string_lossy();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let session_id = session_id_from_file_name(file_name);
    let mut events = Vec::new();
    let mut project_path = "Unknown".to_owned();
    let mut model = "Unknown".to_owned();
    let mut previous_total = None;
    let mut forked_session_id = None;
    let mut skipping_inherited_history = false;

    for_each_json_line(
        path,
        start_offset,
        true,
        &is_cancelled,
        |index, emit, object| {
            let Some(payload) = object.get("payload").and_then(Value::as_object) else {
                return Ok(());
            };

            if index == 0
                && object.get("type").and_then(Value::as_str) == Some("session_meta")
                && non_empty_string(payload.get("forked_from_id")).is_some()
                && let Some(current_session_id) = non_empty_string(payload.get("id"))
            {
                forked_session_id = Some(current_session_id.to_owned());
                skipping_inherited_history = true;
            } else if skipping_inherited_history
                && let Some(forked_session_id) = forked_session_id.as_deref()
                && starts_task_at_or_after_session(payload, forked_session_id)
            {
                skipping_inherited_history = false;
                previous_total = None;
            }

            if let Some(cwd) = non_empty_string(payload.get("cwd")) {
                project_path = cwd.to_owned();
            }
            if let Some(payload_model) = non_empty_string(payload.get("model")) {
                model = payload_model.to_owned();
            }

            let total = nested_object(payload, &["info", "total_token_usage"]);
            let last = nested_object(payload, &["info", "last_token_usage"]);
            if (total.is_none() && last.is_none()) || skipping_inherited_history {
                return Ok(());
            }

            let timestamp = object
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp)
                .or_else(|| {
                    payload
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp)
                });
            let Some(timestamp) = timestamp else {
                if let Some(total) = total {
                    previous_total = Some(codex_usage(total));
                }
                return Ok(());
            };

            let usage = if let Some(total) = total {
                let current_total = codex_usage(total);
                let last_usage = last.map(codex_usage);
                let usage = if let Some(previous_total) = previous_total {
                    let delta = current_total.total.saturating_sub(previous_total.total);
                    if current_total.total < previous_total.total
                        || (current_total.total > previous_total.total
                            && last_usage.is_some_and(|usage| usage.total > delta))
                    {
                        last_usage.unwrap_or(current_total)
                    } else {
                        delta_usage(current_total, previous_total)
                    }
                } else if let Some(last_usage) = last_usage {
                    last_usage
                } else {
                    current_total
                };
                previous_total = Some(current_total);
                usage
            } else {
                codex_usage(last.expect("checked above"))
            };

            if emit && usage.total > 0 {
                events.push(TokenEvent::new(
                    stable_id(&[
                        "codex",
                        &session_id,
                        &index.to_string(),
                        &(timestamp.timestamp_millis() / 1_000).to_string(),
                        &usage.total.to_string(),
                    ]),
                    TokenSource::Codex,
                    timestamp,
                    TokenDeviceMetadata::LOCAL_ID,
                    TokenDeviceMetadata::LOCAL_NAME,
                    &project_path,
                    &session_id,
                    &model,
                    usage,
                    path_text.as_ref(),
                ));
            }
            Ok(())
        },
    )?;

    Ok(events)
}

pub fn parse_claude_file(
    path: impl AsRef<Path>,
    start_offset: u64,
    is_cancelled: impl Fn() -> bool,
) -> Result<Vec<TokenEvent>, ParseError> {
    let path = path.as_ref();
    let path_text = path.to_string_lossy();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let fallback_session_id = session_id_from_file_name(file_name);
    let mut events = Vec::new();
    let mut seen_requests = HashMap::new();
    let mut touched = Vec::new();

    for_each_json_line(
        path,
        start_offset,
        true,
        &is_cancelled,
        |index, emit, object| {
            let Some(message) = object.get("message").and_then(Value::as_object) else {
                return Ok(());
            };
            let Some(usage_value) = message.get("usage").and_then(Value::as_object) else {
                return Ok(());
            };

            let dedupe_key = non_empty_string(object.get("requestId"))
                .or_else(|| non_empty_string(object.get("uuid")))
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{path_text}#{index}"));
            let Some(timestamp) = object
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp)
            else {
                return Ok(());
            };
            let usage = TokenUsage::new(
                int_value(usage_value.get("input_tokens")),
                0,
                int_value(usage_value.get("cache_creation_input_tokens")),
                int_value(usage_value.get("cache_read_input_tokens")),
                int_value(usage_value.get("output_tokens")),
                0,
                None,
            );
            if usage.total == 0 {
                return Ok(());
            }

            if let Some(&existing) = seen_requests.get(&dedupe_key) {
                let event: &mut TokenEvent = &mut events[existing];
                let merged = TokenUsage::new(
                    event.usage.input.max(usage.input),
                    0,
                    event.usage.cache_creation.max(usage.cache_creation),
                    event.usage.cache_read.max(usage.cache_read),
                    event.usage.output.max(usage.output),
                    0,
                    None,
                );
                if merged.total > event.usage.total {
                    event.timestamp = timestamp;
                }
                event.usage = merged;
                if event.model == "Unknown"
                    && let Some(model) = non_empty_string(message.get("model"))
                {
                    event.model = model.to_owned();
                }
                if event.project_path == "Unknown"
                    && let Some(cwd) = non_empty_string(object.get("cwd"))
                {
                    event.project_path = cwd.to_owned();
                }
                touched[existing] |= emit;
                return Ok(());
            }

            seen_requests.insert(dedupe_key.clone(), events.len());
            touched.push(emit);

            events.push(TokenEvent::new(
                stable_id(&["claude", &path_text, &dedupe_key]),
                TokenSource::Claude,
                timestamp,
                TokenDeviceMetadata::LOCAL_ID,
                TokenDeviceMetadata::LOCAL_NAME,
                non_empty_string(object.get("cwd")).unwrap_or("Unknown"),
                non_empty_string(object.get("sessionId")).unwrap_or(&fallback_session_id),
                non_empty_string(message.get("model")).unwrap_or("Unknown"),
                usage,
                path_text.as_ref(),
            ));
            Ok(())
        },
    )?;

    Ok(events
        .into_iter()
        .zip(touched)
        .filter_map(|(event, emit)| emit.then_some(event))
        .collect())
}

fn for_each_json_line(
    path: &Path,
    start_offset: u64,
    replay_prefix: bool,
    is_cancelled: &impl Fn() -> bool,
    mut handle: impl FnMut(usize, bool, &Map<String, Value>) -> Result<(), ParseError>,
) -> Result<(), ParseError> {
    if is_cancelled() {
        return Ok(());
    }
    let data = fs::read(path).map_err(|source| ParseError::Read {
        path: path.to_owned(),
        source,
    })?;
    let emit_from = line_start_index(&data, start_offset)?;
    // ponytail: prefix replay costs O(file size) per append; persist parser state if large active logs make refresh slow.
    let scan_from = if replay_prefix { 0 } else { emit_from };
    let mut logical_index = data[..scan_from]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count();
    let mut checked_bytes = 0;
    let mut line_start = scan_from;

    for line in data[scan_from..].split(|byte| *byte == b'\n') {
        let emit = line_start >= emit_from;
        line_start = line_start.saturating_add(line.len() + 1);
        checked_bytes += line.len() + 1;
        if checked_bytes >= 16_384 {
            if is_cancelled() {
                return Ok(());
            }
            checked_bytes = 0;
        }
        if line.is_empty() {
            continue;
        }
        let index = logical_index;
        logical_index += 1;
        if let Ok(Value::Object(object)) = serde_json::from_slice(line) {
            handle(index, emit, &object)?;
        }
    }
    Ok(())
}

fn line_start_index(data: &[u8], start_offset: u64) -> Result<usize, ParseError> {
    if start_offset == 0 {
        return Ok(0);
    }
    let start = usize::try_from(start_offset)
        .unwrap_or(usize::MAX)
        .min(data.len());
    if start > 0 && data[start - 1] != b'\n' {
        return Err(ParseError::RequiresFullFile);
    }
    Ok(start)
}

fn nested_object<'a>(
    object: &'a Map<String, Value>,
    path: &[&str],
) -> Option<&'a Map<String, Value>> {
    let mut value = object.get(*path.first()?)?;
    for key in &path[1..] {
        value = value.as_object()?.get(*key)?;
    }
    value.as_object()
}

fn int_value(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::String(value)) => value.parse().unwrap_or(0),
        Some(Value::Number(value)) => value.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    swift_date::parse_iso(value)
}

fn codex_usage(object: &Map<String, Value>) -> TokenUsage {
    let total = object
        .get("total_tokens")
        .map(|value| int_value(Some(value)))
        .filter(|value| *value > 0);
    TokenUsage::new(
        int_value(object.get("input_tokens")),
        int_value(object.get("cached_input_tokens")),
        0,
        0,
        int_value(object.get("output_tokens")),
        int_value(object.get("reasoning_output_tokens")),
        total,
    )
}

fn delta_usage(current: TokenUsage, previous: TokenUsage) -> TokenUsage {
    TokenUsage::new(
        current.input.saturating_sub(previous.input).max(0),
        current
            .cached_input
            .saturating_sub(previous.cached_input)
            .max(0),
        current
            .cache_creation
            .saturating_sub(previous.cache_creation)
            .max(0),
        current
            .cache_read
            .saturating_sub(previous.cache_read)
            .max(0),
        current.output.saturating_sub(previous.output).max(0),
        current.reasoning.saturating_sub(previous.reasoning).max(0),
        Some(current.total.saturating_sub(previous.total).max(0)),
    )
}

fn starts_task_at_or_after_session(payload: &Map<String, Value>, session_id: &str) -> bool {
    payload.get("type").and_then(Value::as_str) == Some("task_started")
        && non_empty_string(payload.get("turn_id"))
            .or_else(|| non_empty_string(payload.get("id")))
            .and_then(uuid_v7_timestamp)
            .zip(uuid_v7_timestamp(session_id))
            .is_some_and(|(task, session)| task >= session)
}

fn uuid_v7_timestamp(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
        || bytes[14] != b'7'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 8 | 13 | 18 | 23) || byte.is_ascii_hexdigit())
    {
        return None;
    }
    u64::from_str_radix(&value[..8], 16)
        .ok()?
        .checked_shl(16)?
        .checked_add(u64::from_str_radix(&value[9..13], 16).ok()?)
}

fn session_id_from_file_name(file_name: &str) -> String {
    file_name.replace(".jsonl", "")
}

fn stable_id(parts: &[&str]) -> String {
    let digest = Sha256::digest(parts.join("|").as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::NamedTempFile;

    use super::*;

    fn fixture(contents: &str) -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), contents).unwrap();
        file
    }

    #[test]
    fn codex_uses_deltas_and_skips_fork_history() {
        let file = fixture(concat!(
            "{\"timestamp\":\"2026-01-01T00:00:10.000Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"01900000-2000-7000-8000-000000000000\",\"forked_from_id\":\"parent\",\"cwd\":\"/tmp/project\",\"model\":\"gpt-5\"}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:10.001Z\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":10,\"total_tokens\":10}}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:10.100Z\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"01900000-2001-7000-8000-000000000000\"}}\n",
            "{\"timestamp\":\"bad\",\"payload\":{\"info\":{\"total_token_usage\":{\"input_tokens\":18,\"total_tokens\":18}}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:11.000Z\",\"payload\":{\"info\":{\"total_token_usage\":{\"input_tokens\":24,\"total_tokens\":24}}}}\n"
        ));
        let events = parse_codex_file(file.path(), 0, || false).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].usage.total, 6);
        assert_eq!(events[0].project_path, "/tmp/project");
        assert_eq!(events[0].model, "gpt-5");
    }

    #[test]
    fn codex_uses_last_usage_then_cumulative_deltas() {
        let file = fixture(concat!(
            "{\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"payload\":{\"info\":{\"total_token_usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":2,\"total_tokens\":12},\"last_token_usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":2,\"total_tokens\":12}}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"payload\":{\"info\":{\"total_token_usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":2,\"total_tokens\":12}}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:03.000Z\",\"payload\":{\"info\":{\"total_token_usage\":{\"input_tokens\":18,\"cached_input_tokens\":8,\"output_tokens\":4,\"total_tokens\":22}}}}\n"
        ));
        let events = parse_codex_file(file.path(), 0, || false).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.usage.total)
                .collect::<Vec<_>>(),
            [12, 10]
        );
        assert_eq!(events[1].usage.cached_input, 3);
    }

    #[test]
    fn codex_append_replays_model_project_and_counters() {
        let initial = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"cwd":"/old"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-sol"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"total_tokens":10},"last_token_usage":{"input_tokens":10,"total_tokens":10}}}}"#,
            "\n",
        );
        let appended = concat!(
            r#"{"timestamp":"2026-01-01T00:00:03Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"total_tokens":15},"last_token_usage":{"input_tokens":5,"total_tokens":5}}}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"turn_context","payload":{"model":"gpt-luna","cwd":"/new"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:05Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":7,"total_tokens":7},"last_token_usage":{"input_tokens":7,"total_tokens":7}}}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:06Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":7,"total_tokens":7},"last_token_usage":{"input_tokens":7,"total_tokens":7}}}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:07Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":11,"total_tokens":11},"last_token_usage":{"input_tokens":4,"total_tokens":4}}}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:08Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20,"total_tokens":20},"last_token_usage":{"input_tokens":20,"total_tokens":20}}}}"#,
            "\n",
        );
        let file = fixture(initial);
        let offset = initial.len() as u64;
        fs::write(file.path(), format!("{initial}{appended}")).unwrap();

        let full = parse_codex_file(file.path(), 0, || false).unwrap();
        let tail = parse_codex_file(file.path(), offset, || false).unwrap();
        assert_eq!(
            full.iter()
                .map(|event| event.usage.total)
                .collect::<Vec<_>>(),
            [10, 5, 7, 4, 20]
        );
        assert_eq!(tail, full[1..]);
        assert_eq!(
            tail.iter()
                .map(|event| event.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-sol", "gpt-luna", "gpt-luna", "gpt-luna"]
        );
        assert_eq!(
            tail.iter()
                .map(|event| event.project_path.as_str())
                .collect::<Vec<_>>(),
            ["/old", "/new", "/new", "/new"]
        );
        assert!(matches!(
            parse_codex_file(file.path(), offset - 1, || false),
            Err(ParseError::RequiresFullFile)
        ));
    }

    #[test]
    fn codex_append_still_skips_inherited_fork_history() {
        let initial = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"01900000-2000-7000-8000-000000000000","forked_from_id":"parent","cwd":"/project"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-sol"}}"#,
            "\n",
        );
        let appended = concat!(
            r#"{"timestamp":"2026-01-01T00:00:02Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","payload":{"type":"task_started","turn_id":"01900000-2001-7000-8000-000000000000"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"total_tokens":10},"last_token_usage":{"input_tokens":10,"total_tokens":10}}}}"#,
            "\n",
        );
        let file = fixture(initial);
        let offset = initial.len() as u64;
        fs::write(file.path(), format!("{initial}{appended}")).unwrap();

        let full = parse_codex_file(file.path(), 0, || false).unwrap();
        let tail = parse_codex_file(file.path(), offset, || false).unwrap();
        assert_eq!(tail, full);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].usage.total, 10);
        assert_eq!(tail[0].model, "gpt-sol");
    }

    #[test]
    fn claude_deduplicates_only_valid_usage_and_ignores_bad_numbers() {
        let file = fixture(concat!(
            "{\"timestamp\":\"bad\",\"requestId\":\"same\",\"message\":{\"usage\":{\"input_tokens\":100}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"requestId\":\"same\",\"message\":{\"usage\":{\"input_tokens\":true,\"cache_creation_input_tokens\":1.5,\"cache_read_input_tokens\":9223372036854775808,\"output_tokens\":4}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"requestId\":\"same\",\"message\":{\"usage\":{\"output_tokens\":5}}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"requestId\":\"\",\"uuid\":\"u2\",\"message\":{\"usage\":{\"input_tokens\":-1,\"output_tokens\":2}}}\n"
        ));
        let events = parse_claude_file(file.path(), 0, || false).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .iter()
                .map(|event| event.usage.total)
                .collect::<Vec<_>>(),
            [5, 2]
        );
        assert_eq!(events[0].usage.input, 0);
    }

    #[test]
    fn claude_append_updates_the_request_usage() {
        let initial = concat!(
            r#"{"timestamp":"2026-01-01T00:00:01Z","requestId":"same","message":{"usage":{"input_tokens":1,"cache_read_input_tokens":10,"output_tokens":1}}}"#,
            "\n",
        );
        let appended = concat!(
            r#"{"timestamp":"2026-01-01T00:00:02Z","requestId":"same","cwd":"/project","message":{"model":"claude","usage":{"input_tokens":1,"cache_read_input_tokens":10,"output_tokens":8}}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","requestId":"same","message":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "\n",
        );
        let file = fixture(initial);
        let offset = initial.len() as u64;
        fs::write(file.path(), format!("{initial}{appended}")).unwrap();

        let full = parse_claude_file(file.path(), 0, || false).unwrap();
        let tail = parse_claude_file(file.path(), offset, || false).unwrap();
        assert_eq!(tail, full);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].usage.total, 19);
        assert_eq!(tail[0].model, "claude");
        assert_eq!(tail[0].project_path, "/project");
        assert_eq!(
            tail[0].timestamp,
            parse_timestamp("2026-01-01T00:00:02Z").unwrap()
        );
    }

    #[test]
    fn stable_ids_match_sha256_contract() {
        assert_eq!(
            stable_id(&["claude", "/tmp/sample.jsonl", "request-1"]),
            "6163f26e837cea83931d9c45592545c334e90c482b4b31dc89c58af0c471d2ce"
        );
        assert!(matches!(
            line_start_index(b"one\ntwo", 2),
            Err(ParseError::RequiresFullFile)
        ));
        assert!(parse_timestamp("2026-01-01T00:00:60.000Z").is_none());
        assert_eq!(
            session_id_from_file_name(Path::new("sample.jsonl").to_str().unwrap()),
            "sample"
        );
    }
}
