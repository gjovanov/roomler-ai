// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The replica store every member of a session's replicaset keeps.
//!
//! One SQLite database per device holds the events of every session the device
//! carries — the same choice Hermes Agent makes with its single `state.db` —
//! plus an FTS5 index over their text, which is what full-text session search
//! runs on (design §10.4). The daemon owns the file; no local user can read it.
//!
//! Every append is checked against the session's chain tip
//! ([`crate::chain::check_next`]) inside one transaction with its index row, so
//! the store can never hold a gap, a broken link or a stale writer's event, and
//! the index never disagrees with the events.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};

use crate::chain::{ChainError, ChainTip, EventEnvelope, check_next};
use crate::event::TranscriptEvent;

/// The schema this build writes. A newer file is refused rather than written
/// with an older idea of its shape.
const SCHEMA_VERSION: i64 = 1;

/// At most this many sessions per search filter — a view grant names sessions
/// explicitly, and a filter larger than this is a caller bug.
pub const MAX_SEARCH_SESSIONS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Chain(#[from] ChainError),
    #[error(
        "the store was written by a newer build (schema {found}, this build knows {SCHEMA_VERSION})"
    )]
    NewerSchema { found: i64 },
    #[error("a stored event of session {session} seq {seq} has a malformed hash")]
    CorruptHash { session: String, seq: u64 },
    #[error("a search may name at most {MAX_SEARCH_SESSIONS} sessions, not {0}")]
    TooManySessions(usize),
}

/// One full-text match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub session: String,
    pub seq: u64,
    /// The matching text around the hit, with matches wrapped in `[` `]`.
    pub snippet: String,
}

/// The replica store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the store at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        // WAL: a reader (a viewer paging a transcript) never blocks the
        // primary's appends. NORMAL is durable at WAL checkpoints, which is the
        // right trade for a replica that also exists on other devices.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    /// An in-memory store, for tests and dry runs.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema { found });
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 session    TEXT    NOT NULL,
                 seq        INTEGER NOT NULL,
                 fence      INTEGER NOT NULL,
                 ts_ms      INTEGER NOT NULL,
                 prev_hash  BLOB    NOT NULL,
                 hash       BLOB    NOT NULL,
                 kind       TEXT,
                 event_json TEXT    NOT NULL,
                 PRIMARY KEY (session, seq)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS tips (
                 session TEXT PRIMARY KEY,
                 seq     INTEGER NOT NULL,
                 hash    BLOB    NOT NULL,
                 fence   INTEGER NOT NULL
             ) WITHOUT ROWID;
             CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                 text,
                 session UNINDEXED,
                 seq UNINDEXED,
                 tokenize = 'unicode61 remove_diacritics 2'
             );",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// The newest event this store holds for `session`.
    pub fn tip(&self, session: &str) -> Result<Option<ChainTip>, StoreError> {
        Self::tip_in(&self.conn, session)
    }

    fn tip_in(conn: &Connection, session: &str) -> Result<Option<ChainTip>, StoreError> {
        let row: Option<(i64, Vec<u8>, i64)> = conn
            .query_row(
                "SELECT seq, hash, fence FROM tips WHERE session = ?1",
                params![session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(seq, hash, fence)| {
            Ok(ChainTip {
                seq: seq as u64,
                hash: to_hash(&hash, session, seq as u64)?,
                fence: fence as u64,
            })
        })
        .transpose()
    }

    /// Append one event: checked against the tip, written with its index row
    /// and the new tip in one transaction.
    pub fn append(&mut self, env: &EventEnvelope) -> Result<ChainTip, StoreError> {
        let tx = self.conn.transaction()?;
        let tip = Self::tip_in(&tx, &env.session)?;
        check_next(tip, env)?;
        let next = env.tip();
        Self::insert(&tx, env, &next)?;
        tx.commit()?;
        Ok(next)
    }

    fn insert(
        tx: &Transaction<'_>,
        env: &EventEnvelope,
        next: &ChainTip,
    ) -> Result<(), StoreError> {
        let event = env.event();
        tx.execute(
            "INSERT INTO events (session, seq, fence, ts_ms, prev_hash, hash, kind, event_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                env.session,
                env.seq as i64,
                env.fence as i64,
                env.ts_ms,
                env.prev_hash.as_slice(),
                next.hash.as_slice(),
                event.as_ref().map(TranscriptEvent::kind),
                env.event_json,
            ],
        )?;
        if let Some(text) = event.as_ref().and_then(TranscriptEvent::search_text) {
            tx.execute(
                "INSERT INTO events_fts (text, session, seq) VALUES (?1, ?2, ?3)",
                params![text, env.session, env.seq as i64],
            )?;
        }
        tx.execute(
            "INSERT INTO tips (session, seq, hash, fence) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session) DO UPDATE SET seq = ?2, hash = ?3, fence = ?4",
            params![
                env.session,
                next.seq as i64,
                next.hash.as_slice(),
                next.fence as i64
            ],
        )?;
        Ok(())
    }

    /// Events of `session` after `after_seq`, oldest first, at most `limit`.
    pub fn page(
        &self,
        session: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, fence, ts_ms, prev_hash, event_json FROM events
             WHERE session = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session, after_seq as i64, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, fence, ts_ms, prev, event_json) = row?;
            out.push(EventEnvelope {
                session: session.to_string(),
                seq: seq as u64,
                fence: fence as u64,
                ts_ms,
                prev_hash: to_hash(&prev, session, seq as u64)?,
                event_json,
            });
        }
        Ok(out)
    }

    /// Full-text search, best matches first (FTS5 `rank`). `sessions` limits
    /// the search to the sessions a view grant names; `None` searches
    /// everything this store holds (the daemon's own use, never a viewer's).
    ///
    /// The query is taken as plain words — each becomes a quoted FTS5 phrase,
    /// all of which must match — so a viewer's search box can never inject FTS
    /// syntax or fail on a stray quote.
    pub fn search(
        &self,
        query: &str,
        sessions: Option<&[String]>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let Some(fts_query) = plain_words_query(query) else {
            return Ok(Vec::new());
        };
        if let Some(list) = sessions {
            if list.is_empty() {
                return Ok(Vec::new());
            }
            if list.len() > MAX_SEARCH_SESSIONS {
                return Err(StoreError::TooManySessions(list.len()));
            }
        }
        let mut sql = String::from(
            "SELECT session, seq, snippet(events_fts, 0, '[', ']', '…', 12)
             FROM events_fts WHERE events_fts MATCH ?1",
        );
        let mut args: Vec<rusqlite::types::Value> = vec![fts_query.into()];
        if let Some(list) = sessions {
            sql.push_str(" AND session IN (");
            for (i, s) in list.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("?{}", i + 2));
                args.push(s.clone().into());
            }
            sql.push(')');
        }
        sql.push_str(&format!(" ORDER BY rank LIMIT {}", limit.min(1_000)));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok(SearchHit {
                session: r.get(0)?,
                seq: r.get::<_, i64>(1)? as u64,
                snippet: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Delete everything this store holds for `session` — the purge a tombstone
    /// orders. Returns how many events were removed.
    pub fn purge(&mut self, session: &str) -> Result<u64, StoreError> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM events WHERE session = ?1", params![session])?;
        tx.execute(
            "DELETE FROM events_fts WHERE session = ?1",
            params![session],
        )?;
        tx.execute("DELETE FROM tips WHERE session = ?1", params![session])?;
        tx.commit()?;
        Ok(n as u64)
    }

    /// Every session this store holds, with its tip.
    pub fn sessions(&self) -> Result<Vec<(String, ChainTip)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT session, seq, hash, fence FROM tips ORDER BY session")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (session, seq, hash, fence) = row?;
            let hash = to_hash(&hash, &session, seq as u64)?;
            out.push((
                session,
                ChainTip {
                    seq: seq as u64,
                    hash,
                    fence: fence as u64,
                },
            ));
        }
        Ok(out)
    }
}

fn to_hash(raw: &[u8], session: &str, seq: u64) -> Result<[u8; 32], StoreError> {
    raw.try_into().map_err(|_| StoreError::CorruptHash {
        session: session.to_string(),
        seq,
    })
}

/// Plain words → an FTS5 query: every whitespace-separated word becomes a quoted
/// phrase (an embedded `"` doubled), all required. `None` when there are none.
fn plain_words_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::GENESIS;

    fn ev(text: &str) -> TranscriptEvent {
        TranscriptEvent::AssistantText { text: text.into() }
    }

    fn append_all(store: &mut Store, session: &str, fence: u64, texts: &[&str]) -> ChainTip {
        let mut tip = store.tip(session).unwrap();
        for (i, t) in texts.iter().enumerate() {
            let env = EventEnvelope::next(session, tip, fence, i as i64, &ev(t));
            tip = Some(store.append(&env).unwrap());
        }
        tip.unwrap()
    }

    #[test]
    fn appends_chain_and_pages_back_byte_for_byte() {
        let mut s = Store::open_in_memory().unwrap();
        let tip = append_all(&mut s, "s1", 1, &["one", "two", "three"]);
        assert_eq!(tip.seq, 3);
        let page = s.page("s1", 0, 10).unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].prev_hash, GENESIS);
        assert_eq!(page[1].prev_hash, page[0].hash());
        assert_eq!(page[2].hash(), tip.hash);
        assert_eq!(s.page("s1", 2, 10).unwrap().len(), 1);
        assert_eq!(s.page("s1", 0, 2).unwrap().len(), 2);
    }

    #[test]
    fn refuses_what_does_not_follow_the_tip_and_keeps_the_index_consistent() {
        let mut s = Store::open_in_memory().unwrap();
        let tip = append_all(&mut s, "s1", 3, &["a", "b"]);
        // A stale writer: refused, and nothing of it is indexed.
        let stale = EventEnvelope::next("s1", Some(tip), 2, 9, &ev("ghostword"));
        assert!(matches!(
            s.append(&stale),
            Err(StoreError::Chain(ChainError::StaleFence { .. }))
        ));
        assert!(s.search("ghostword", None, 10).unwrap().is_empty());
        // A gap: refused.
        let mut gap = EventEnvelope::next("s1", Some(tip), 3, 9, &ev("c"));
        gap.seq = 5;
        assert!(matches!(
            s.append(&gap),
            Err(StoreError::Chain(ChainError::Gap { .. }))
        ));
        assert_eq!(s.tip("s1").unwrap(), Some(tip), "the tip did not move");
    }

    #[test]
    fn a_promotion_continues_the_chain_under_the_new_fence() {
        let mut s = Store::open_in_memory().unwrap();
        let tip = append_all(&mut s, "s1", 1, &["before"]);
        let after = EventEnvelope::next("s1", Some(tip), 2, 5, &ev("after"));
        let tip = s.append(&after).unwrap();
        assert_eq!((tip.seq, tip.fence), (2, 2));
    }

    #[test]
    fn search_finds_words_scoped_to_granted_sessions() {
        let mut s = Store::open_in_memory().unwrap();
        append_all(&mut s, "s1", 1, &["the kubelet restarted on node-a"]);
        append_all(&mut s, "s2", 1, &["kubelet logs were clean"]);
        let all = s.search("kubelet", None, 10).unwrap();
        assert_eq!(all.len(), 2);
        let only_s2 = s.search("kubelet", Some(&["s2".into()]), 10).unwrap();
        assert_eq!(only_s2.len(), 1);
        assert_eq!(only_s2[0].session, "s2");
        assert!(only_s2[0].snippet.contains("[kubelet]"), "{:?}", only_s2[0]);
        // Every word must match.
        assert_eq!(s.search("kubelet node-a", None, 10).unwrap().len(), 1);
        // An empty grant searches nothing — never "everything".
        assert!(s.search("kubelet", Some(&[]), 10).unwrap().is_empty());
    }

    #[test]
    fn search_input_cannot_inject_fts_syntax() {
        let mut s = Store::open_in_memory().unwrap();
        append_all(&mut s, "s1", 1, &["NEAR and OR are words here"]);
        // Each of these is FTS5 syntax that would error or change the meaning if
        // passed through raw.
        for q in ["\"unbalanced", "NEAR(", "a OR", "*", "col:x", "-"] {
            s.search(q, None, 10)
                .unwrap_or_else(|e| panic!("query {q:?} errored: {e}"));
        }
        assert_eq!(s.search("OR", None, 10).unwrap().len(), 1);
        assert!(s.search("   ", None, 10).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_kind_is_stored_and_chained_but_not_indexed() {
        let mut s = Store::open_in_memory().unwrap();
        let env = EventEnvelope {
            session: "s1".into(),
            seq: 1,
            fence: 1,
            ts_ms: 0,
            prev_hash: GENESIS,
            event_json: r#"{"kind":"from_the_future","text":"zebra"}"#.into(),
        };
        s.append(&env).unwrap();
        assert_eq!(s.page("s1", 0, 10).unwrap(), vec![env]);
        assert!(s.search("zebra", None, 10).unwrap().is_empty());
    }

    #[test]
    fn purge_removes_events_index_and_tip_of_one_session_only() {
        let mut s = Store::open_in_memory().unwrap();
        append_all(&mut s, "s1", 1, &["alpha", "beta"]);
        append_all(&mut s, "s2", 1, &["alpha"]);
        assert_eq!(s.purge("s1").unwrap(), 2);
        assert_eq!(s.tip("s1").unwrap(), None);
        assert!(s.page("s1", 0, 10).unwrap().is_empty());
        let hits = s.search("alpha", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "s2");
        assert_eq!(s.sessions().unwrap().len(), 1);
    }

    #[test]
    fn a_file_store_survives_reopen_and_refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hive.db");
        {
            let mut s = Store::open(&path).unwrap();
            append_all(&mut s, "s1", 1, &["persisted"]);
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.tip("s1").unwrap().map(|t| t.seq), Some(1));
        assert_eq!(s.search("persisted", None, 5).unwrap().len(), 1);
        drop(s);
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerSchema { .. })
        ));
    }
}
