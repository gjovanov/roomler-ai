use std::collections::HashSet;
use std::path::Path;

/// A single SRT subtitle entry.
#[derive(Debug, Clone)]
pub struct SrtEntry {
    pub index: usize,
    pub start_secs: f64,
    pub end_secs: f64,
    pub speaker: Option<String>,
    pub text: String,
}

/// Parses an SRT file, deduplicates by (start_time, text), and sorts by time.
pub fn parse_srt(path: impl AsRef<Path>) -> anyhow::Result<Vec<SrtEntry>> {
    let content = std::fs::read_to_string(path.as_ref())
        .map_err(|e| anyhow::anyhow!("Failed to read SRT '{}': {}", path.as_ref().display(), e))?;

    let mut entries = Vec::new();
    let mut lines = content.lines().peekable();

    while lines.peek().is_some() {
        // Skip blank lines
        while lines.peek().is_some_and(|l| l.trim().is_empty()) {
            lines.next();
        }

        // Index line
        let index_line = match lines.next() {
            Some(l) => l.trim().to_string(),
            None => break,
        };
        let index: usize = match index_line.parse() {
            Ok(i) => i,
            Err(_) => continue,
        };

        // Timestamp line: "HH:MM:SS,mmm --> HH:MM:SS,mmm"
        let ts_line = match lines.next() {
            Some(l) => l.trim().to_string(),
            None => break,
        };
        let (start_secs, end_secs) = match parse_timestamp_line(&ts_line) {
            Some(t) => t,
            None => continue,
        };

        // Text lines (until blank line or EOF)
        let mut text_parts = Vec::new();
        while lines.peek().is_some_and(|l| !l.trim().is_empty()) {
            text_parts.push(lines.next().unwrap().trim().to_string());
        }
        let raw_text = text_parts.join(" ");

        // Parse "Speaker: text" format
        let (speaker, text) = if let Some(colon_pos) = raw_text.find(": ") {
            let candidate = &raw_text[..colon_pos];
            // Heuristic: speaker names don't contain spaces beyond a reasonable length
            if candidate.len() < 50 && !candidate.contains("  ") {
                (
                    Some(candidate.to_string()),
                    raw_text[colon_pos + 2..].to_string(),
                )
            } else {
                (None, raw_text)
            }
        } else {
            (None, raw_text)
        };

        entries.push(SrtEntry {
            index,
            start_secs,
            end_secs,
            speaker,
            text,
        });
    }

    // Deduplicate by (start_time rounded to ms, text)
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for entry in entries {
        let key = (
            (entry.start_secs * 1000.0).round() as i64,
            entry.text.clone(),
        );
        if seen.insert(key) {
            deduped.push(entry);
        }
    }

    // Sort by start time
    deduped.sort_by(|a, b| a.start_secs.partial_cmp(&b.start_secs).unwrap());

    // Re-index
    for (i, entry) in deduped.iter_mut().enumerate() {
        entry.index = i + 1;
    }

    Ok(deduped)
}

/// Parses a timestamp line like "00:00:02,965 --> 00:00:04,277"
fn parse_timestamp_line(line: &str) -> Option<(f64, f64)> {
    let parts: Vec<&str> = line.split("-->").collect();
    if parts.len() != 2 {
        return None;
    }
    let start = parse_srt_time(parts[0].trim())?;
    let end = parse_srt_time(parts[1].trim())?;
    Some((start, end))
}

/// Parses SRT time format "HH:MM:SS,mmm" to seconds.
fn parse_srt_time(s: &str) -> Option<f64> {
    // Handle both comma and dot separators
    let s = s.replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours: f64 = parts[0].parse().ok()?;
    let minutes: f64 = parts[1].parse().ok()?;
    let seconds: f64 = parts[2].parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_srt_time() {
        assert!((parse_srt_time("00:00:02,965").unwrap() - 2.965).abs() < 0.001);
        assert!((parse_srt_time("00:01:30,500").unwrap() - 90.5).abs() < 0.001);
        assert!((parse_srt_time("01:00:00,000").unwrap() - 3600.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_timestamp_line() {
        let (s, e) = parse_timestamp_line("00:00:02,965 --> 00:00:04,277").unwrap();
        assert!((s - 2.965).abs() < 0.001);
        assert!((e - 4.277).abs() < 0.001);
    }
}
