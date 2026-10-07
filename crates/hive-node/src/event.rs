// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! What a Hive session records.

use serde::{Deserialize, Serialize};

/// Token counts a harness reports for a turn, in the Messages API's terms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

/// One recorded event of a session — the unit the replicaset stores, chains and
/// replicates, and the browser renders.
///
/// Internally tagged by `kind`, so a stored event is self-describing JSON.
///
/// ⚠️ This enum is a *view*, not the record. Members of one replicaset can run
/// different daemon versions, and a member must never refuse a `kind` it has
/// not heard of — the same rule as the agent's capability verbs, where an
/// additive list is only additive if old readers skip what they don't know. The
/// chain therefore carries each event as its exact JSON text
/// ([`crate::chain::EventEnvelope::event_json`]); [`TranscriptEvent::from_json`]
/// answers `None` for an unknown kind, and the envelope is stored and forwarded
/// byte for byte all the same.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// The harness started or resumed (stream-json `system/init`).
    SessionInit {
        /// The harness's own session id.
        harness_session_id: String,
        model: Option<String>,
        cwd: Option<String>,
        #[serde(default)]
        tools: Vec<String>,
    },
    /// A person's prompt, attributed to its author when one is known.
    UserMessage {
        #[serde(default)]
        author: Option<String>,
        text: String,
    },
    /// A complete assistant text block. Streaming deltas are not recorded — the
    /// complete block always follows them (see [`crate::stream_json::Parsed`]).
    AssistantText { text: String },
    /// A thinking block's readable summary — never a raw chain of thought, and
    /// only when the model returned a non-empty one.
    Thinking { summary: String },
    /// The model asked for a tool.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// What a tool returned.
    ToolResult {
        tool_use_id: String,
        ok: bool,
        output: String,
        /// The output was cut to the device's limit; the head and the tail are kept.
        #[serde(default)]
        truncated: bool,
    },
    /// The end of a turn (stream-json `result`): what the turn cost and how it ended.
    Turn {
        ok: bool,
        #[serde(default)]
        subtype: Option<String>,
        num_turns: Option<u32>,
        duration_ms: Option<u64>,
        cost_usd: Option<f64>,
        usage: Option<Usage>,
    },
    /// The harness compacted its context.
    Compaction {
        trigger: Option<String>,
        pre_tokens: Option<u64>,
    },
    /// Something Hive itself says into the transcript — a resume note, a move.
    Note { text: String },
}

impl TranscriptEvent {
    /// The wire tag, as it appears in the JSON's `kind` field.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionInit { .. } => "session_init",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantText { .. } => "assistant_text",
            Self::Thinking { .. } => "thinking",
            Self::ToolUse { .. } => "tool_use",
            Self::ToolResult { .. } => "tool_result",
            Self::Turn { .. } => "turn",
            Self::Compaction { .. } => "compaction",
            Self::Note { .. } => "note",
        }
    }

    /// The event as the JSON text the chain carries.
    pub fn to_json(&self) -> String {
        // Every field is a string, a number, a bool or an already-valid
        // `serde_json::Value`; serialisation cannot fail.
        serde_json::to_string(self).expect("a TranscriptEvent always serialises")
    }

    /// The typed view of an event's JSON, or `None` when the kind is unknown to
    /// this build or the text is not an event.
    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    /// The text a full-text index should hold for this event, if any.
    pub fn search_text(&self) -> Option<String> {
        let text = match self {
            Self::UserMessage { text, .. } | Self::AssistantText { text } => text.clone(),
            Self::Thinking { summary } => summary.clone(),
            Self::ToolUse { name, input, .. } => format!("{name} {input}"),
            Self::ToolResult { output, .. } => output.clone(),
            Self::Note { text } => text.clone(),
            Self::SessionInit { .. } | Self::Turn { .. } | Self::Compaction { .. } => return None,
        };
        (!text.trim().is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json_with_the_kind_tag() {
        let ev = TranscriptEvent::ToolUse {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "cargo test"}),
        };
        let json = ev.to_json();
        assert!(json.contains(r#""kind":"tool_use""#), "{json}");
        assert_eq!(TranscriptEvent::from_json(&json), Some(ev));
    }

    #[test]
    fn the_kind_method_matches_the_serialised_tag() {
        let all = [
            TranscriptEvent::SessionInit {
                harness_session_id: "s".into(),
                model: None,
                cwd: None,
                tools: vec![],
            },
            TranscriptEvent::UserMessage {
                author: None,
                text: "t".into(),
            },
            TranscriptEvent::AssistantText { text: "t".into() },
            TranscriptEvent::Thinking {
                summary: "t".into(),
            },
            TranscriptEvent::ToolUse {
                id: "i".into(),
                name: "n".into(),
                input: serde_json::Value::Null,
            },
            TranscriptEvent::ToolResult {
                tool_use_id: "i".into(),
                ok: true,
                output: "o".into(),
                truncated: false,
            },
            TranscriptEvent::Turn {
                ok: true,
                subtype: None,
                num_turns: None,
                duration_ms: None,
                cost_usd: None,
                usage: None,
            },
            TranscriptEvent::Compaction {
                trigger: None,
                pre_tokens: None,
            },
            TranscriptEvent::Note { text: "t".into() },
        ];
        for ev in all {
            let v: serde_json::Value = serde_json::from_str(&ev.to_json()).unwrap();
            assert_eq!(v["kind"], ev.kind(), "{ev:?}");
        }
    }

    #[test]
    fn an_unknown_kind_is_not_an_error_just_not_a_view() {
        // A newer daemon's event: an older member must still store it.
        assert_eq!(
            TranscriptEvent::from_json(r#"{"kind":"approval","tool":"Bash"}"#),
            None
        );
    }

    #[test]
    fn search_text_skips_metadata_and_blank_text() {
        assert_eq!(
            TranscriptEvent::Turn {
                ok: true,
                subtype: None,
                num_turns: Some(3),
                duration_ms: None,
                cost_usd: None,
                usage: None
            }
            .search_text(),
            None
        );
        assert_eq!(
            TranscriptEvent::AssistantText { text: "  ".into() }.search_text(),
            None
        );
        assert_eq!(
            TranscriptEvent::ToolUse {
                id: "i".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command":"ls"})
            }
            .search_text()
            .as_deref(),
            Some(r#"Bash {"command":"ls"}"#)
        );
    }
}
