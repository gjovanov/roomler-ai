use std::path::Path;

/// A single reference transcript entry parsed from a .txt file.
///
/// Format per entry:
/// ```text
/// SPK_N
/// M:SS
/// Transcript text here.
///
/// ```
#[derive(Debug, Clone)]
pub struct TxtEntry {
    pub speaker: String,
    pub start_secs: f64,
    /// Estimated end time (start of next entry, or start + 5s for last entry).
    pub end_secs: f64,
    pub text: String,
}

/// Parses a reference transcript .txt file.
///
/// Expected format: blocks of (speaker, timestamp M:SS, text) separated by blank lines.
pub fn parse_txt(path: impl AsRef<Path>) -> anyhow::Result<Vec<TxtEntry>> {
    let content = std::fs::read_to_string(path.as_ref())
        .map_err(|e| anyhow::anyhow!("Failed to read TXT '{}': {}", path.as_ref().display(), e))?;

    let mut entries = Vec::new();
    let mut lines = content.lines().peekable();

    while lines.peek().is_some() {
        // Skip blank lines
        while lines.peek().is_some_and(|l| l.trim().is_empty()) {
            lines.next();
        }

        // Speaker line (e.g. "SPK_1")
        let speaker = match lines.next() {
            Some(l) if l.trim().starts_with("SPK_") => l.trim().to_string(),
            Some(_) => continue,
            None => break,
        };

        // Timestamp line (e.g. "0:49" or "25:07")
        let ts_line = match lines.next() {
            Some(l) => l.trim().to_string(),
            None => break,
        };
        let start_secs = match parse_mm_ss(&ts_line) {
            Some(s) => s,
            None => continue,
        };

        // Text line(s) until blank line or next speaker
        let mut text_parts = Vec::new();
        while lines
            .peek()
            .is_some_and(|l| !l.trim().is_empty() && !l.trim().starts_with("SPK_"))
        {
            text_parts.push(lines.next().unwrap().trim().to_string());
        }
        let text = text_parts.join(" ");

        if !text.is_empty() {
            entries.push(TxtEntry {
                speaker,
                start_secs,
                end_secs: 0.0, // filled in below
                text,
            });
        }
    }

    // Compute end_secs: start of next entry, or start + 5s for last
    for i in 0..entries.len() {
        entries[i].end_secs = if i + 1 < entries.len() {
            entries[i + 1].start_secs
        } else {
            entries[i].start_secs + 5.0
        };
    }

    Ok(entries)
}

/// Parses "M:SS" timestamp to seconds (e.g. "1:06" -> 66.0, "25:07" -> 1507.0).
fn parse_mm_ss(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let minutes: f64 = parts[0].parse().ok()?;
    let seconds: f64 = parts[1].parse().ok()?;
    Some(minutes * 60.0 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mm_ss() {
        assert!((parse_mm_ss("0:00").unwrap() - 0.0).abs() < 0.001);
        assert!((parse_mm_ss("0:49").unwrap() - 49.0).abs() < 0.001);
        assert!((parse_mm_ss("1:06").unwrap() - 66.0).abs() < 0.001);
        assert!((parse_mm_ss("25:07").unwrap() - 1507.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_txt_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(
            &path,
            "SPK_1\n0:00\nHello world.\n\nSPK_2\n0:05\nGoodbye world.\n",
        )
        .unwrap();
        let entries = parse_txt(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].speaker, "SPK_1");
        assert!((entries[0].start_secs - 0.0).abs() < 0.001);
        assert!((entries[0].end_secs - 5.0).abs() < 0.001);
        assert_eq!(entries[0].text, "Hello world.");
        assert_eq!(entries[1].speaker, "SPK_2");
        assert!((entries[1].start_secs - 5.0).abs() < 0.001);
        assert_eq!(entries[1].text, "Goodbye world.");
    }
}
