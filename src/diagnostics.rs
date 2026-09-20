use crate::config::{RawLogConfig, RawLogLevel};
use crate::redaction::{redact_json, redact_json_string_values, truncate_json_strings};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

const RAW_LOG_STRING_MAX_CHARS: usize = 512;

pub fn write_raw_event(config: &RawLogConfig, kind: &'static str, fields: Value) {
    if !config.level.allows_metadata() {
        return;
    }

    if let Err(error) = write_raw_event_inner(config, kind, fields) {
        warn!(kind, error = %error, "failed to write raw diagnostic event");
    }
}

pub fn write_raw_content_event(config: &RawLogConfig, kind: &'static str, fields: Value) {
    if !config.level.allows_content() {
        return;
    }

    if let Err(error) = write_raw_event_inner(config, kind, fields) {
        warn!(kind, error = %error, "failed to write raw diagnostic content event");
    }
}

pub fn request_content_snapshot(config: &RawLogConfig, body: &[u8]) -> Value {
    let snapshot = content_snapshot(config, body);
    let extracted = snapshot
        .get("parsed")
        .map(extract_request_attributes)
        .unwrap_or_else(|| json!({}));
    let extracted = match config.level {
        RawLogLevel::ContentRedacted => redact_json_string_values(&extracted),
        RawLogLevel::Off | RawLogLevel::Metadata | RawLogLevel::FullContent => extracted,
    };

    json!({
        "schema_version": 1,
        "direction": "request",
        "content_level": config.level.as_str(),
        "bytes": {
            "total": snapshot["body_bytes"],
            "captured": snapshot["captured_bytes"],
            "truncated": snapshot["truncated"],
        },
        "format": snapshot["format"],
        "extracted": extracted,
        "content": snapshot["content"],
    })
}

pub fn response_content_snapshot(config: &RawLogConfig, body: &[u8]) -> Value {
    let snapshot = content_snapshot(config, body);
    let parsed_present = snapshot["parsed"].is_object();

    json!({
        "schema_version": 1,
        "direction": "response",
        "content_level": config.level.as_str(),
        "bytes": {
            "total": snapshot["body_bytes"],
            "captured": snapshot["captured_bytes"],
            "truncated": snapshot["truncated"],
        },
        "format": snapshot["format"],
        "parsed_json": parsed_present,
        "content": snapshot["content"],
    })
}

fn content_snapshot(config: &RawLogConfig, body: &[u8]) -> Value {
    let bounded = if body.len() > config.content_max_bytes {
        &body[..config.content_max_bytes]
    } else {
        body
    };
    let truncated = body.len() > bounded.len();

    if let Ok(parsed) = serde_json::from_slice::<Value>(bounded) {
        let content = match config.level {
            RawLogLevel::ContentRedacted => redact_json_string_values(&redact_json(&parsed)),
            RawLogLevel::FullContent => redact_json(&parsed),
            RawLogLevel::Off | RawLogLevel::Metadata => Value::Null,
        };
        return json!({
            "format": "json",
            "body_bytes": body.len(),
            "captured_bytes": bounded.len(),
            "truncated": truncated,
            "parsed": parsed,
            "content": content,
        });
    }

    let text = String::from_utf8_lossy(bounded).to_string();
    let preview = match config.level {
        RawLogLevel::ContentRedacted => Value::String("<redacted-content>".to_owned()),
        RawLogLevel::FullContent => Value::String(text),
        RawLogLevel::Off | RawLogLevel::Metadata => Value::Null,
    };

    json!({
        "format": "text",
        "body_bytes": body.len(),
        "captured_bytes": bounded.len(),
        "truncated": truncated,
        "parsed": Value::Null,
        "content": preview,
    })
}

fn extract_request_attributes(parsed: &Value) -> Value {
    let model = parsed.get("model").cloned().unwrap_or(Value::Null);
    let stream = parsed.get("stream").cloned().unwrap_or(Value::Null);
    let store = parsed.get("store").cloned().unwrap_or(Value::Null);
    let reasoning_effort = parsed
        .pointer("/reasoning/effort")
        .cloned()
        .unwrap_or(Value::Null);
    let reasoning_summary = parsed
        .pointer("/reasoning/summary")
        .cloned()
        .unwrap_or(Value::Null);
    let previous_response_id_present = parsed
        .get("previous_response_id")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    let include_count = parsed
        .get("include")
        .and_then(Value::as_array)
        .map(std::vec::Vec::len)
        .unwrap_or(0);
    let input_count = parsed
        .get("input")
        .and_then(Value::as_array)
        .map(std::vec::Vec::len)
        .unwrap_or(0);
    let tool_names = parsed
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .take(20)
                .map(std::borrow::ToOwned::to_owned)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    json!({
        "model": model,
        "stream": stream,
        "store": store,
        "reasoning_effort": reasoning_effort,
        "reasoning_summary": reasoning_summary,
        "previous_response_id_present": previous_response_id_present,
        "include_count": include_count,
        "input_count": input_count,
        "tools": {
            "count": parsed.get("tools").and_then(Value::as_array).map(std::vec::Vec::len).unwrap_or(0),
            "names": tool_names,
        }
    })
}

fn write_raw_event_inner(
    config: &RawLogConfig,
    kind: &'static str,
    fields: Value,
) -> std::io::Result<()> {
    if let Some(parent) = config.file.parent() {
        fs::create_dir_all(parent)?;
    }

    let event = json!({
        "timestamp_unix_ms": unix_millis(),
        "kind": kind,
        "fields": fields,
    });
    let event = truncate_json_strings(&redact_json(&event), RAW_LOG_STRING_MAX_CHARS);
    let mut line = serde_json::to_string(&event).expect("raw diagnostic event should serialize");

    if line.len() > config.max_bytes {
        line = serde_json::to_string(&json!({
            "timestamp_unix_ms": unix_millis(),
            "kind": "diagnostic_event_truncated",
            "fields": {
                "original_kind": kind,
                "max_bytes": config.max_bytes,
                "original_bytes": line.len(),
            }
        }))
        .expect("raw diagnostic truncation event should serialize");
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.file)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RawLogLevel;
    use tempfile::NamedTempFile;

    #[test]
    fn raw_events_are_redacted_and_bounded() {
        let file = NamedTempFile::new().unwrap();
        let config = RawLogConfig {
            level: RawLogLevel::Metadata,
            file: file.path().to_path_buf(),
            max_bytes: 4096,
            content_max_bytes: 1024,
        };

        write_raw_event(
            &config,
            "test_event",
            json!({
                "authorization": "Bearer secret-token",
                "body_preview": "not a body capture",
                "safe": "visible",
            }),
        );

        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("test_event"));
        assert!(text.contains("visible"));
        assert!(!text.contains("secret-token"));
    }

    #[test]
    fn request_content_snapshot_redacts_strings_when_content_redacted() {
        let config = RawLogConfig {
            level: RawLogLevel::ContentRedacted,
            file: NamedTempFile::new().unwrap().path().to_path_buf(),
            max_bytes: 4096,
            content_max_bytes: 1024,
        };

        let payload = request_content_snapshot(
            &config,
            br#"{"model":"gpt-test","reasoning":{"effort":"high"},"input":"secret prompt"}"#,
        );
        let text = payload.to_string();
        assert!(text.contains("schema_version"));
        assert!(text.contains("reasoning_effort"));
        assert!(text.contains("<redacted-content>"));
        assert!(!text.contains("secret prompt"));
        assert!(!text.contains("high"));
    }

    #[test]
    fn request_content_snapshot_extracts_effort_and_tools_in_full_mode() {
        let config = RawLogConfig {
            level: RawLogLevel::FullContent,
            file: NamedTempFile::new().unwrap().path().to_path_buf(),
            max_bytes: 4096,
            content_max_bytes: 1024,
        };

        let payload = request_content_snapshot(
            &config,
            br#"{"model":"gpt-5.5","reasoning":{"effort":"high"},"tools":[{"name":"mcp__memory__recall"}],"stream":true}"#,
        );

        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["direction"], "request");
        assert_eq!(payload["extracted"]["reasoning_effort"], "high");
        assert_eq!(payload["extracted"]["tools"]["count"], 1);
        assert_eq!(
            payload["extracted"]["tools"]["names"][0],
            "mcp__memory__recall"
        );
    }
}
