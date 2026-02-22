/// Normalize text for WER comparison: lowercase, strip punctuation, collapse whitespace.
pub fn normalize_text(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split normalized text into words.
fn normalize_words(text: &str) -> Vec<String> {
    normalize_text(text)
        .split_whitespace()
        .map(|w| w.to_string())
        .collect()
}

/// Word-level Levenshtein edit distance.
pub fn levenshtein_words(ref_words: &[String], hyp_words: &[String]) -> usize {
    let m = ref_words.len();
    let n = hyp_words.len();
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if ref_words[i - 1] == hyp_words[j - 1] {
                0
            } else {
                1
            };
            curr[j] = std::cmp::min(
                std::cmp::min(prev[j] + 1, curr[j - 1] + 1),
                prev[j - 1] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Computes Word Error Rate between reference and hypothesis text.
///
/// Returns `(wer, edit_distance, reference_word_count)`.
pub fn word_error_rate(reference: &str, hypothesis: &str) -> (f64, usize, usize) {
    let ref_words = normalize_words(reference);
    let hyp_words = normalize_words(hypothesis);

    if ref_words.is_empty() {
        return if hyp_words.is_empty() {
            (0.0, 0, 0)
        } else {
            (1.0, hyp_words.len(), 0)
        };
    }

    let distance = levenshtein_words(&ref_words, &hyp_words);
    let wer = distance as f64 / ref_words.len() as f64;
    (wer, distance, ref_words.len())
}

/// Computes micro-averaged WER across multiple reference/hypothesis pairs.
///
/// Returns `(wer, total_edit_distance, total_reference_words)`.
pub fn aggregate_wer(pairs: &[(String, String)]) -> (f64, usize, usize) {
    let mut total_edits = 0usize;
    let mut total_ref_words = 0usize;

    for (reference, hypothesis) in pairs {
        let (_, edits, ref_count) = word_error_rate(reference, hypothesis);
        total_edits += edits;
        total_ref_words += ref_count;
    }

    if total_ref_words == 0 {
        return (0.0, 0, 0);
    }

    let wer = total_edits as f64 / total_ref_words as f64;
    (wer, total_edits, total_ref_words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wer_identical() {
        let (wer, _, _) = word_error_rate("hello world", "hello world");
        assert!((wer - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_wer_one_substitution() {
        let (wer, _, _) = word_error_rate("hello world", "hello earth");
        assert!((wer - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_wer_case_insensitive() {
        let (wer, _, _) = word_error_rate("Hello World", "hello world");
        assert!((wer - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_wer_ignores_punctuation() {
        let (wer, _, _) = word_error_rate("Hello, world!", "hello world");
        assert!((wer - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_wer_empty() {
        let (wer, _, _) = word_error_rate("", "");
        assert!((wer - 0.0).abs() < 0.001);
        let (wer, _, _) = word_error_rate("", "some text");
        assert!((wer - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_aggregate_wer() {
        let pairs = vec![
            ("hello world".to_string(), "hello world".to_string()),
            ("foo bar baz".to_string(), "foo baz".to_string()),
        ];
        let (wer, edits, total) = aggregate_wer(&pairs);
        // 0 edits on first, ~2 edits on second (1 deletion + 1 substitution or similar)
        assert!(wer > 0.0);
        assert!(total == 5); // 2 + 3
        assert!(edits > 0);
    }

    #[test]
    fn test_normalize_text() {
        assert_eq!(
            normalize_text("Hello, World!  How  Are You?"),
            "hello world how are you"
        );
    }

    #[test]
    fn test_normalize_german() {
        assert_eq!(normalize_text("Österreich: 25°C!"), "österreich 25c");
    }
}
