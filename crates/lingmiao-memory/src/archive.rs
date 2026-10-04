//! Turn archive (#1 memory) — ported from `memory/archive.py`.
//!
//! The full transcript of every completed turn: the user message, the assistant
//! reply, the system prompt / context handed to the model, the raw message list,
//! tool-call records and token accounting. This is the source material the
//! pipeline's search and consolidation stages read back.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use lingmiao_core::LingmiaoError;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::embed::{Embedder, cosine, decode_blob, encode_blob};
use crate::id::{now_iso, short_id};
use crate::store::{STORE, lock, open_db};

const DB_NAME: &str = "context_record.db";

/// One archived turn with its semantic relevance score（archive 的语义召回
/// 结果，与 [`crate::observations::ScoredObservation`] / 知识图谱的
/// `(Node, f32)` 同构——三层的 `collect_hits` 因此能统一打分）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredTurn {
    /// The archived turn.
    pub turn: Turn,
    /// Cosine similarity against the query embedding (higher is better).
    pub score: f32,
}

/// One archived turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// `turn-<12hex>`.
    pub id: String,
    /// RFC-3339 timestamp.
    pub at: String,
    /// The user's message.
    pub user_msg: String,
    /// The assistant's reply.
    pub assistant: String,
    /// System prompt used for this turn.
    pub system_prompt: String,
    /// Injected context prefix (retrieved memory).
    pub context_prefix: String,
    /// Full message list (JSON) sent to the model.
    pub full_messages: String,
    /// Tool-call records (JSON array).
    pub tool_calls: String,
    /// Prompt tokens.
    pub tokens_in: i64,
    /// Completion tokens.
    pub tokens_out: i64,
    /// Total tokens.
    pub tokens_total: i64,
    /// Groups turns belonging to one logical chain.
    pub chain_id: String,
    /// Position within the chain.
    pub chain_seq: i64,
    /// Model reasoning content, when exposed.
    pub reasoning: String,
    /// Turn summary.
    pub summary: String,
}

impl Turn {
    /// A minimal turn from user + assistant text; everything else defaults.
    pub fn new(user_msg: impl Into<String>, assistant: impl Into<String>) -> Self {
        Self {
            id: short_id("turn"),
            at: now_iso(),
            user_msg: user_msg.into(),
            assistant: assistant.into(),
            system_prompt: String::new(),
            context_prefix: String::new(),
            full_messages: String::new(),
            tool_calls: "[]".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            tokens_total: 0,
            chain_id: String::new(),
            chain_seq: 1,
            reasoning: String::new(),
            summary: String::new(),
        }
    }
}

/// The turn archive store.
pub struct Archive {
    conn: Mutex<Connection>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl Archive {
    /// Open (creating if needed) the archive at `path`.
    pub fn open(path: &Path, embedder: Option<Arc<dyn Embedder>>) -> Result<Self, LingmiaoError> {
        let conn = open_db(path)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Open the archive inside a zone directory.
    pub fn open_in_dir(
        dir: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open(&dir.join(DB_NAME), embedder)
    }

    /// Open an **existing** archive read-only (cross-project search); no file
    /// creation, no schema DDL.
    pub fn open_readonly(
        path: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        let conn = crate::store::open_db_readonly(path)?;
        Ok(Self {
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Read-only variant of [`Archive::open_in_dir`].
    pub fn open_in_dir_readonly(
        dir: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open_readonly(&dir.join(DB_NAME), embedder)
    }

    fn init_schema(conn: &Connection) -> Result<(), LingmiaoError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS turns (
                id            TEXT PRIMARY KEY,
                at            TEXT NOT NULL,
                user_msg      TEXT NOT NULL,
                assistant     TEXT DEFAULT '',
                system_prompt TEXT DEFAULT '',
                context_prefix TEXT DEFAULT '',
                full_messages TEXT DEFAULT '',
                tool_calls    TEXT DEFAULT '[]',
                tokens_in     INTEGER DEFAULT 0,
                tokens_out    INTEGER DEFAULT 0,
                tokens_total  INTEGER DEFAULT 0,
                chain_id      TEXT DEFAULT '',
                chain_seq     INTEGER DEFAULT 1,
                reasoning     TEXT DEFAULT '',
                summary       TEXT DEFAULT '',
                embedding     BLOB
            );
            CREATE INDEX IF NOT EXISTS idx_turns_at ON turns(at DESC);
            CREATE INDEX IF NOT EXISTS idx_turns_chain ON turns(chain_id, chain_seq);",
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("create archive schema: {e}")))?;
        Ok(())
    }

    /// Archive a turn, returning its id.
    pub fn save(&self, turn: &Turn) -> Result<String, LingmiaoError> {
        let embedding = self.embedder.as_ref().map(|e| {
            encode_blob(&e.embed_document(&format!("{}\n{}", turn.user_msg, turn.assistant)))
        });
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT OR REPLACE INTO turns
             (id, at, user_msg, assistant, system_prompt, context_prefix, full_messages, tool_calls,
              tokens_in, tokens_out, tokens_total, chain_id, chain_seq, reasoning, summary, embedding)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                turn.id,
                turn.at,
                turn.user_msg,
                turn.assistant,
                turn.system_prompt,
                turn.context_prefix,
                turn.full_messages,
                turn.tool_calls,
                turn.tokens_in,
                turn.tokens_out,
                turn.tokens_total,
                turn.chain_id,
                turn.chain_seq,
                turn.reasoning,
                turn.summary,
                embedding,
            ],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("save turn: {e}")))?;
        Ok(turn.id.clone())
    }

    /// Fetch a turn by id.
    pub fn get(&self, id: &str) -> Result<Option<Turn>, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row("SELECT * FROM turns WHERE id=?1", params![id], row_to_turn)
            .optional()
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Backfill a turn's `summary` (③ E存档). Deliberately a targeted `UPDATE`,
    /// **not** `INSERT OR REPLACE`: the latter would reset every other column
    /// and force a re-embed. Returns the number of rows changed (0 when the id
    /// is unknown — a summary backfill after a failed C stage is harmless).
    pub fn set_summary(&self, id: &str, summary: &str) -> Result<usize, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.execute(
            "UPDATE turns SET summary=?2 WHERE id=?1",
            params![id, summary],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("set turn summary: {e}")))
    }

    /// Newest turns first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Turn>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare("SELECT * FROM turns ORDER BY at DESC LIMIT ?1")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_turn)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        Ok(out)
    }

    /// Keyword search over user / assistant text, newest first.
    pub fn search(&self, keyword: &str, limit: usize) -> Result<Vec<Turn>, LingmiaoError> {
        let like = format!("%{keyword}%");
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT * FROM turns WHERE user_msg LIKE ?1 OR assistant LIKE ?1
                 ORDER BY at DESC LIMIT ?2",
            )
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map(params![like, limit as i64], row_to_turn)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        Ok(out)
    }

    /// Cosine semantic search over the archived turns' stored embeddings.
    ///
    /// The archive wrote a vector for every turn at [`Archive::save`] time
    /// (`user_msg` + newline + `assistant`) since ④真语义, but had **no query
    /// entry point** — the one missing pass that kept `search_memory` a hybrid
    /// search the archive could not fully honour (C 项 2026-10-05). Mirrors
    /// [`crate::observations::Observations::search_semantic`]: an embedder-less
    /// store returns an empty set rather than erroring.
    pub fn search_semantic(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<ScoredTurn>, LingmiaoError> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let qv = embedder.embed_query(query);
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare("SELECT * FROM turns WHERE embedding IS NOT NULL")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                let turn = row_to_turn(r)?;
                let blob: Option<Vec<u8>> = r.get("embedding")?;
                Ok((turn, blob))
            })
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut hits: Vec<ScoredTurn> = Vec::new();
        for row in rows {
            let (turn, blob) = row.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let ev = blob.map(|b| decode_blob(&b)).unwrap_or_default();
            hits.push(ScoredTurn {
                turn,
                score: cosine(&ev, &qv),
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    /// Total archived turns.
    pub fn count(&self) -> Result<u64, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row("SELECT COUNT(*) FROM turns", [], |r| r.get::<_, i64>(0))
            .map(|n| n.max(0) as u64)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Number of turns whose `embedding` column is NULL (backfill pending).
    pub fn missing_embeddings(&self) -> Result<u64, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE embedding IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Recompute every turn's embedding with the current embedder — the archive
    /// half of the ④真语义 vector-space migration. Returns the number of turns
    /// updated; a no-op (`0`) without an embedder. The embedded text mirrors
    /// [`Archive::save`] (`user_msg` + newline + `assistant`).
    pub fn reembed_all(&self) -> Result<u64, LingmiaoError> {
        let Some(embedder) = &self.embedder else {
            return Ok(0);
        };
        let rows: Vec<(String, String, String)> = {
            let conn = lock(&self.conn);
            let mut stmt = conn
                .prepare("SELECT id, user_msg, assistant FROM turns")
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
            }
            out
        };
        let conn = lock(&self.conn);
        let mut n = 0u64;
        for (id, user_msg, assistant) in rows {
            let blob = encode_blob(&embedder.embed_document(&format!("{user_msg}\n{assistant}")));
            conn.execute(
                "UPDATE turns SET embedding=?2 WHERE id=?1",
                params![id, blob],
            )
            .map_err(|e| LingmiaoError::memory(STORE, format!("reembed turn: {e}")))?;
            n += 1;
        }
        Ok(n)
    }

    /// The runtime schema of this store (需求⑤ `store`/`schema` view): the
    /// `turns` table, read live from the SQLite catalog.
    pub fn schema(&self) -> Result<Vec<crate::store::TableSchema>, LingmiaoError> {
        let conn = lock(&self.conn);
        Ok(crate::store::read_table_schema(&conn, "turns")?
            .into_iter()
            .collect())
    }
}

fn row_to_turn(r: &Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        id: r.get("id")?,
        at: r.get("at")?,
        user_msg: r.get("user_msg")?,
        assistant: r.get("assistant")?,
        system_prompt: r.get("system_prompt")?,
        context_prefix: r.get("context_prefix")?,
        full_messages: r.get("full_messages")?,
        tool_calls: r.get("tool_calls")?,
        tokens_in: r.get("tokens_in")?,
        tokens_out: r.get("tokens_out")?,
        tokens_total: r.get("tokens_total")?,
        chain_id: r.get("chain_id")?,
        chain_seq: r.get("chain_seq")?,
        reasoning: r.get("reasoning")?,
        summary: r.get("summary")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashingEmbedder;

    fn temp_archive() -> (Archive, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lingmiao-arc-{}", short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let a = Archive::open_in_dir(&dir, Some(Arc::new(HashingEmbedder::new()))).unwrap();
        (a, dir)
    }

    #[test]
    fn save_and_get_roundtrip() {
        let (a, dir) = temp_archive();
        let mut t = Turn::new("hello", "hi there");
        t.tokens_in = 3;
        t.tokens_out = 2;
        t.tokens_total = 5;
        let id = a.save(&t).unwrap();
        let got = a.get(&id).unwrap().unwrap();
        assert_eq!(got.user_msg, "hello");
        assert_eq!(got.tokens_total, 5);
        assert_eq!(a.count().unwrap(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_matches_either_side() {
        let (a, dir) = temp_archive();
        a.save(&Turn::new("tell me about sqlite", "sure")).unwrap();
        a.save(&Turn::new("unrelated", "sqlite is a database"))
            .unwrap();
        let hits = a.search("sqlite", 10).unwrap();
        assert_eq!(hits.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn semantic_search_ranks_turns_by_cosine() {
        // C 项 2026-10-05: the archive wrote a vector per turn since ④真语义 but
        // had no query method, so `search_memory` could only keyword-match it.
        let (a, dir) = temp_archive();
        a.save(&Turn::new(
            "how does the observation store work",
            "it is a sqlite table",
        ))
        .unwrap();
        a.save(&Turn::new("totally unrelated", "banana helicopter"))
            .unwrap();
        let hits = a.search_semantic("observation store", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score >= hits[1].score);
        assert!(
            hits[0].turn.user_msg.contains("observation store"),
            "best hit is the related turn: {:?}",
            hits[0].turn.user_msg
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
