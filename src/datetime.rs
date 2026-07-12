use std::borrow::Cow;

use chrono::{DateTime, Local, Utc};

const DISPLAY_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

pub fn now() -> String {
    Local::now().format(DISPLAY_FORMAT).to_string()
}

pub fn format_unix_ms(unix_ms: u128) -> String {
    i64::try_from(unix_ms)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format(DISPLAY_FORMAT)
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn timestamp_line(line: &str) -> String {
    timestamp_line_at(line, &now())
}

pub fn timestamp_bytes(bytes: &[u8]) -> Vec<u8> {
    timestamp_bytes_at(bytes, &now())
}

pub fn strip_timestamp_prefix(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    if bytes.len() >= 22
        && bytes[0] == b'['
        && bytes[5] == b'-'
        && bytes[8] == b'-'
        && bytes[11] == b' '
        && bytes[14] == b':'
        && bytes[17] == b':'
        && bytes[20] == b']'
        && bytes[21] == b' '
    {
        Cow::Borrowed(&text[22..])
    } else {
        Cow::Borrowed(text)
    }
}

fn timestamp_line_at(line: &str, timestamp: &str) -> String {
    format!("[{timestamp}] {line}\n")
}

fn timestamp_bytes_at(bytes: &[u8], timestamp: &str) -> Vec<u8> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let prefix = format!("[{timestamp}] ");
    let mut output = Vec::with_capacity(bytes.len().saturating_add(prefix.len()));
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        output.extend_from_slice(prefix.as_bytes());
        output.extend_from_slice(line);
        if !line.ends_with(b"\n") {
            output.push(b'\n');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_each_log_line_and_terminates_partial_lines() {
        assert_eq!(
            timestamp_bytes_at(b"first\nsecond", "2026-07-12 10:20:30"),
            b"[2026-07-12 10:20:30] first\n[2026-07-12 10:20:30] second\n"
        );
        assert_eq!(
            timestamp_line_at("started", "2026-07-12 10:20:30"),
            "[2026-07-12 10:20:30] started\n"
        );
    }

    #[test]
    fn strips_only_well_shaped_timestamp_prefixes() {
        assert_eq!(
            strip_timestamp_prefix("[2026-07-12 10:20:30] job succeeded"),
            "job succeeded"
        );
        assert_eq!(
            strip_timestamp_prefix("[status] job succeeded"),
            "[status] job succeeded"
        );
    }

    #[test]
    fn formats_unix_milliseconds_as_a_local_datetime() {
        let formatted = format_unix_ms(0);
        assert_eq!(formatted.len(), 19);
        assert_eq!(&formatted[4..5], "-");
        assert_eq!(&formatted[7..8], "-");
        assert_eq!(&formatted[10..11], " ");
        assert_eq!(&formatted[13..14], ":");
        assert_eq!(&formatted[16..17], ":");
    }
}
