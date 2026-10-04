//! Observation log (#2 memory) — ported from `memory/observations.py`.
//!
//! An append-only, time-stamped record of facts / preferences / decisions /
//! constraints / tasks / audits. Storage is one SQLite table; retrieval is
//! either keyword LIKE search (original `search`) or cosine semantic search
//! over the shared embedding space (shared with the knowledge graph).

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use lingmiao_core::LingmiaoError;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::embed::{Embedder, cosine, decode_blob, encode_blob};
use crate::id::{now_iso, short_id};
use crate::store::{STORE, lock, open_db};

const DB_NAME: &str = "observations.db";

/// Keyword rank field weight for a hit in `name`.
///
/// These three constants are the **single source of truth** for how a keyword
/// hit is scored ([`rank_by_relevance`]); the 需求⑤ `meta_show search/ranking`
/// view publishes *these* values, so the ranker and the meta platform can never
/// drift. Pinned by [`tests::field_weights_are_the_documented_contract`].
pub const FIELD_WEIGHT_NAME: i32 = 10;
/// Keyword rank field weight for a hit in `topic`.
pub const FIELD_WEIGHT_TOPIC: i32 = 5;
/// Keyword rank field weight for a hit in `content`.
pub const FIELD_WEIGHT_CONTENT: i32 = 3;

/// One observation record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// `obs-<12hex>`.
    pub id: String,
    /// RFC-3339 creation time.
    pub at: String,
    /// Record kind (`fact` / `preference` / `decision` / `constraint` / …).
    pub kind: String,
    /// Grouping topic.
    pub topic: String,
    /// Short unique label.
    pub name: String,
    /// Full body text.
    pub content: String,
    /// Comma-separated search keywords.
    pub keywords: String,
    /// Originating turn id.
    pub turn_id: String,
    /// Producing source (`consolidation` by default).
    pub source: String,
    /// Pipeline stage that produced it.
    pub stage: String,
    /// JSON array of extra topics.
    pub topics: String,
}

/// Fields for a new observation; `id`/`at` are assigned by [`Observations::insert`].
#[derive(Debug, Clone)]
pub struct NewObservation<'a> {
    /// Record kind.
    pub kind: &'a str,
    /// Grouping topic (defaults to `kind` in the original's batch path).
    pub topic: &'a str,
    /// Short label.
    pub name: &'a str,
    /// Body text.
    pub content: &'a str,
    /// Search keywords.
    pub keywords: &'a str,
    /// Originating turn.
    pub turn_id: &'a str,
    /// Producing source.
    pub source: &'a str,
    /// Producing stage.
    pub stage: &'a str,
    /// JSON array of topics.
    pub topics: &'a str,
}

impl NewObservation<'_> {
    /// Convenience constructor with the original's defaults.
    pub fn new<'a>(kind: &'a str, name: &'a str, content: &'a str) -> NewObservation<'a> {
        NewObservation {
            kind,
            topic: kind,
            name,
            content,
            keywords: "",
            turn_id: "",
            source: "consolidation",
            stage: "",
            topics: "[]",
        }
    }
}

/// An observation plus its similarity/relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredObservation {
    /// The record.
    pub observation: Observation,
    /// Higher is better (cosine for semantic, weighted score for keyword).
    pub score: f32,
}

/// The append-only observation store.
pub struct Observations {
    conn: Mutex<Connection>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl Observations {
    /// Open (creating if needed) the store at `path`.
    pub fn open(path: &Path, embedder: Option<Arc<dyn Embedder>>) -> Result<Self, LingmiaoError> {
        let conn = open_db(path)?;
        Self::init_schema(&conn)?;
        crate::store::record_embedder_backend(&conn, embedder.as_ref().map(|e| e.backend()));
        Ok(Self {
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Open the store inside a zone directory (e.g. `memory/main/`).
    pub fn open_in_dir(
        dir: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open(&dir.join(DB_NAME), embedder)
    }

    /// Open an **existing** store read-only (cross-project search).
    ///
    /// Unlike [`Observations::open`] this never creates the file nor runs any
    /// schema DDL/migration, so probing another project's memory cannot mutate
    /// it. The file must already exist.
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

    /// Read-only variant of [`Observations::open_in_dir`].
    pub fn open_in_dir_readonly(
        dir: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open_readonly(&dir.join(DB_NAME), embedder)
    }

    /// The shared embedder handle, if any (cross-project search reuses the
    /// running process's vector space instead of reloading a model).
    pub fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.embedder.clone()
    }

    fn init_schema(conn: &Connection) -> Result<(), LingmiaoError> {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS observations (
                id        TEXT PRIMARY KEY,
                at        TEXT NOT NULL,
                kind      TEXT NOT NULL,
                topic     TEXT NOT NULL,
                name      TEXT NOT NULL,
                content   TEXT NOT NULL,
                keywords  TEXT DEFAULT '',
                turn_id   TEXT NOT NULL,
                source    TEXT DEFAULT 'consolidation',
                stage     TEXT DEFAULT '',
                topics    TEXT DEFAULT '[]',
                embedding BLOB
            );
            {}",
            crate::store::META_TABLE_DDL,
        ))
        .map_err(|e| LingmiaoError::memory(STORE, format!("create observations: {e}")))?;
        Self::migrate(conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_obs_at    ON observations(at DESC);
             CREATE INDEX IF NOT EXISTS idx_obs_topic ON observations(topic);
             CREATE INDEX IF NOT EXISTS idx_obs_kind  ON observations(kind);
             CREATE INDEX IF NOT EXISTS idx_obs_turn  ON observations(turn_id);
             CREATE INDEX IF NOT EXISTS idx_obs_stage ON observations(stage);",
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("create obs indexes: {e}")))?;
        Ok(())
    }

    /// Ensure columns added after the first schema release exist.
    fn migrate(conn: &Connection) -> Result<(), LingmiaoError> {
        let mut cols: Vec<String> = Vec::new();
        let mut stmt = conn
            .prepare("PRAGMA table_info(observations)")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        for r in rows {
            cols.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        if !cols.iter().any(|c| c == "stage") {
            conn.execute(
                "ALTER TABLE observations ADD COLUMN stage TEXT DEFAULT ''",
                [],
            )
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        }
        if !cols.iter().any(|c| c == "topics") {
            conn.execute(
                "ALTER TABLE observations ADD COLUMN topics TEXT DEFAULT '[]'",
                [],
            )
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        }
        if !cols.iter().any(|c| c == "embedding") {
            conn.execute("ALTER TABLE observations ADD COLUMN embedding BLOB", [])
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        }
        Ok(())
    }

    /// Embed a **stored** observation body (the document side of the
    /// asymmetric convention — see `Embedder::embed_document`).
    fn embed_doc(&self, text: &str) -> Option<Vec<u8>> {
        self.embedder
            .as_ref()
            .map(|e| encode_blob(&e.embed_document(text)))
    }

    /// Insert one observation, returning its new id.
    pub fn insert(&self, new: &NewObservation<'_>) -> Result<String, LingmiaoError> {
        let id = short_id("obs");
        let at = now_iso();
        let embedding = self.embed_doc(new.content);
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO observations
             (id, at, kind, topic, name, content, keywords, turn_id, source, stage, topics, embedding)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                id,
                at,
                new.kind,
                new.topic,
                new.name,
                new.content,
                new.keywords,
                new.turn_id,
                new.source,
                new.stage,
                new.topics,
                embedding,
            ],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("insert observation: {e}")))?;
        Ok(id)
    }

    /// Insert a batch, sharing one `turn_id` / `stage`.
    pub fn insert_batch(
        &self,
        items: &[NewObservation<'_>],
        turn_id: &str,
        source: &str,
        stage: &str,
    ) -> Result<Vec<String>, LingmiaoError> {
        let mut ids = Vec::with_capacity(items.len());
        for it in items {
            let mut n = it.clone();
            n.turn_id = turn_id;
            n.source = source;
            n.stage = stage;
            ids.push(self.insert(&n)?);
        }
        Ok(ids)
    }

    /// Fetch one observation by id.
    pub fn get(&self, id: &str) -> Result<Option<Observation>, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT * FROM observations WHERE id=?1",
            params![id],
            row_to_obs,
        )
        .optional()
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Fetch several observations, newest first.
    pub fn get_by_ids(&self, ids: &[String]) -> Result<Vec<Observation>, LingmiaoError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql =
            format!("SELECT * FROM observations WHERE id IN ({placeholders}) ORDER BY at DESC");
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), row_to_obs)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        collect(rows)
    }

    /// Distinct kinds present, sorted.
    pub fn all_kinds(&self) -> Result<Vec<String>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare("SELECT DISTINCT kind FROM observations ORDER BY kind")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let k = r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            if !k.is_empty() {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Observations of a given kind, newest first.
    pub fn by_kind(&self, kind: &str, limit: usize) -> Result<Vec<Observation>, LingmiaoError> {
        self.rows(
            "SELECT * FROM observations WHERE kind=?1 ORDER BY at DESC LIMIT ?2",
            params![kind, limit as i64],
        )
    }

    /// Observations of a given topic, newest first.
    pub fn by_topic(&self, topic: &str, limit: usize) -> Result<Vec<Observation>, LingmiaoError> {
        self.rows(
            "SELECT * FROM observations WHERE topic=?1 ORDER BY at DESC LIMIT ?2",
            params![topic, limit as i64],
        )
    }

    /// Newest observations.
    pub fn recent(&self, limit: usize) -> Result<Vec<Observation>, LingmiaoError> {
        self.rows(
            "SELECT * FROM observations ORDER BY at DESC LIMIT ?1",
            params![limit as i64],
        )
    }

    fn rows(
        &self,
        sql: &str,
        params_value: impl rusqlite::Params,
    ) -> Result<Vec<Observation>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map(params_value, row_to_obs)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        collect(rows)
    }

    /// Paginated enumeration, newest first, optional kind filter.
    pub fn list_all(
        &self,
        limit: usize,
        offset: usize,
        kind: &str,
    ) -> Result<Vec<Observation>, LingmiaoError> {
        let conn = lock(&self.conn);
        if kind.is_empty() {
            let mut stmt = conn
                .prepare("SELECT * FROM observations ORDER BY at DESC LIMIT ?1 OFFSET ?2")
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let rows = stmt
                .query_map(params![limit as i64, offset as i64], row_to_obs)
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            collect(rows)
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM observations WHERE kind=?1 ORDER BY at DESC LIMIT ?2 OFFSET ?3",
                )
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let rows = stmt
                .query_map(params![kind, limit as i64, offset as i64], row_to_obs)
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            collect(rows)
        }
    }

    /// Multi-word keyword search across topic / name / content.
    ///
    /// Whitespace-separated words are OR-matched (avoids the original's
    /// zero-hit exact-substring problem for CJK compounds). When more rows match
    /// than `limit`, results are ranked by field weight (name=10, topic=5,
    /// content=3) plus a small recency bonus.
    pub fn search(
        &self,
        keyword: &str,
        limit: usize,
        kind: &str,
    ) -> Result<Vec<ScoredObservation>, LingmiaoError> {
        let words: Vec<String> = keyword.split_whitespace().map(|w| w.to_string()).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let word_clause =
            vec!["(topic LIKE ? OR name LIKE ? OR content LIKE ?)"; words.len()].join(" OR ");
        let mut bindings: Vec<String> = Vec::with_capacity(words.len() * 3);
        for w in &words {
            let like = format!("%{w}%");
            bindings.push(like.clone());
            bindings.push(like.clone());
            bindings.push(like);
        }
        let conn = lock(&self.conn);
        let (sql, all): (String, Vec<String>) = if kind.is_empty() {
            (
                format!("SELECT * FROM observations WHERE {word_clause} ORDER BY at DESC"),
                bindings,
            )
        } else {
            let mut b = vec![kind.to_string()];
            b.extend(bindings);
            (
                format!(
                    "SELECT * FROM observations WHERE kind=?1 AND ({word_clause}) ORDER BY at DESC"
                ),
                b,
            )
        };
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map(params_from_iter(all.iter()), row_to_obs)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let obs = collect(rows)?;
        Ok(rank_by_relevance(obs, &words, limit))
    }

    /// Cosine semantic search over stored embeddings.
    pub fn search_semantic(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<ScoredObservation>, LingmiaoError> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let qv = embedder.embed_query(query);
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare("SELECT * FROM observations WHERE embedding IS NOT NULL")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut hits: Vec<ScoredObservation> = Vec::new();
        let rows = stmt
            .query_map([], |r| {
                let obs = row_to_obs(r)?;
                let blob: Option<Vec<u8>> = r.get("embedding")?;
                Ok((obs, blob))
            })
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        for row in rows {
            let (obs, blob) = row.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let ev = blob.map(|b| decode_blob(&b)).unwrap_or_default();
            hits.push(ScoredObservation {
                observation: obs,
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

    /// Total observation count.
    pub fn stats(&self) -> Result<u64, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row("SELECT COUNT(*) FROM observations", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n.max(0) as u64)
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Number of rows whose `embedding` column is NULL (backfill pending).
    pub fn missing_embeddings(&self) -> Result<u64, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT COUNT(*) FROM observations WHERE embedding IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// The backend identifier of this store's embedder (需求⑤ `embed` view).
    /// `None` when the store was opened without an embedder (lexical-only).
    pub fn embedder_backend(&self) -> Option<&'static str> {
        self.embedder.as_ref().map(|e| e.backend())
    }

    /// Recompute every row's embedding with the current embedder and refresh the
    /// recorded provenance — the one-time vector-space migration after switching
    /// embedder backends (④真语义). Returns the number of rows updated; a no-op
    /// (`0`) when the store was opened without an embedder.
    pub fn reembed_all(&self) -> Result<u64, LingmiaoError> {
        let Some(embedder) = &self.embedder else {
            return Ok(0);
        };
        let rows: Vec<(String, String)> = {
            let conn = lock(&self.conn);
            let mut stmt = conn
                .prepare("SELECT id, content FROM observations")
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            collect(rows)?
        };
        let conn = lock(&self.conn);
        let mut n = 0u64;
        for (id, content) in rows {
            let blob = encode_blob(&embedder.embed_document(&content));
            conn.execute(
                "UPDATE observations SET embedding=?2 WHERE id=?1",
                params![id, blob],
            )
            .map_err(|e| LingmiaoError::memory(STORE, format!("reembed observation: {e}")))?;
            n += 1;
        }
        crate::store::write_meta(
            &conn,
            crate::store::META_EMBEDDER_BACKEND,
            embedder.backend(),
        )?;
        Ok(n)
    }

    /// The embedder backend this store **recorded** when its vectors were
    /// written (cross-project space guard), or `None` for a store written before
    /// the metadata table existed. Read from the live DB, so it works on a
    /// read-only connection too.
    pub fn recorded_embedder_backend(&self) -> Option<String> {
        let conn = lock(&self.conn);
        crate::store::read_meta(&conn, crate::store::META_EMBEDDER_BACKEND)
    }

    /// The runtime schema of this store (需求⑤ `store`/`schema` view), read from
    /// the live SQLite catalog (`PRAGMA table_info` + `sqlite_master`) so it can
    /// never drift from `init_schema`.
    pub fn schema(&self) -> Result<Vec<crate::store::TableSchema>, LingmiaoError> {
        let conn = lock(&self.conn);
        Ok(crate::store::read_table_schema(&conn, "observations")?
            .into_iter()
            .collect())
    }
}

fn row_to_obs(r: &Row<'_>) -> rusqlite::Result<Observation> {
    Ok(Observation {
        id: r.get("id")?,
        at: r.get("at")?,
        kind: r.get("kind")?,
        topic: r.get("topic")?,
        name: r.get("name")?,
        content: r.get("content")?,
        keywords: r.get("keywords")?,
        turn_id: r.get("turn_id")?,
        source: r.get("source")?,
        stage: r.get("stage")?,
        topics: r.get("topics")?,
    })
}

fn collect<T, I>(rows: I) -> Result<Vec<T>, LingmiaoError>
where
    I: Iterator<Item = rusqlite::Result<T>>,
{
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
    }
    Ok(out)
}

/// Rank keyword hits by field weight + recency, mirroring `_rank_by_relevance`.
fn rank_by_relevance(
    rows: Vec<Observation>,
    words: &[String],
    limit: usize,
) -> Vec<ScoredObservation> {
    let n = rows.len();
    let mut scored: Vec<(f32, usize, Observation)> = rows
        .into_iter()
        .enumerate()
        .map(|(idx, obs)| {
            let name = obs.name.to_lowercase();
            let topic = obs.topic.to_lowercase();
            let content = obs.content.to_lowercase();
            let mut score = 0.0f32;
            for w in words {
                let wl = w.to_lowercase();
                if name.contains(&wl) {
                    score += FIELD_WEIGHT_NAME as f32;
                }
                if topic.contains(&wl) {
                    score += FIELD_WEIGHT_TOPIC as f32;
                }
                if content.contains(&wl) {
                    score += FIELD_WEIGHT_CONTENT as f32;
                }
            }
            let recency = if n == 0 {
                0.0
            } else {
                (n - idx) as f32 / n as f32
            };
            (score + recency, idx, obs)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(score, _, observation)| ScoredObservation { observation, score })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashingEmbedder;

    fn temp_store() -> (Observations, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lingmiao-obs-{}", short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            Observations::open_in_dir(&dir, Some(Arc::new(HashingEmbedder::new()))).unwrap();
        (store, dir)
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let (store, dir) = temp_store();
        let id = store
            .insert(&NewObservation::new(
                "fact",
                "project",
                "lingmiao is a rewrite",
            ))
            .unwrap();
        let got = store.get(&id).unwrap().expect("found");
        assert_eq!(got.kind, "fact");
        assert_eq!(got.name, "project");
        assert!(!got.at.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn batch_shares_turn_id() {
        let (store, dir) = temp_store();
        let items = [
            NewObservation::new("fact", "a", "alpha"),
            NewObservation::new("preference", "b", "beta"),
        ];
        let ids = store
            .insert_batch(&items, "turn-1", "consolidation", "")
            .unwrap();
        assert_eq!(ids.len(), 2);
        let got = store.get_by_ids(&ids).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|o| o.turn_id == "turn-1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keyword_search_ranks_name_match_first() {
        let (store, dir) = temp_store();
        store
            .insert(&NewObservation::new(
                "fact",
                "sqlite",
                "the memory uses sqlite",
            ))
            .unwrap();
        store
            .insert(&NewObservation::new(
                "fact",
                "other",
                "sqlite appears in body only",
            ))
            .unwrap();
        let hits = store.search("sqlite", 10, "").unwrap();
        assert_eq!(hits.len(), 2);
        // Name match (10 pts) must outrank content-only match (3 pts).
        assert_eq!(hits[0].observation.name, "sqlite");
        assert!(hits[0].score > hits[1].score);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn field_weights_are_the_documented_contract() {
        // The 需求⑤ `meta_show search/ranking` view publishes these publics; the
        // ranker consumes the same consts, so this pins the published contract.
        assert_eq!(FIELD_WEIGHT_NAME, 10);
        assert_eq!(FIELD_WEIGHT_TOPIC, 5);
        assert_eq!(FIELD_WEIGHT_CONTENT, 3);
    }

    #[test]
    fn keyword_scores_follow_field_weights() {
        // `rank_by_relevance` adds a recency bonus of (n - idx)/n; with a single
        // row that is exactly 1.0, so score = field weight + 1.0. This is a
        // lockstep check that the ranker really uses the consts (not a copy) and
        // in the documented order name > topic > content.
        let recency = 1.0f32;
        let (store, dir) = temp_store();
        store
            .insert(&NewObservation::new("fact", "zterm", "plain body"))
            .unwrap();
        let name = store.search("zterm", 1, "").unwrap()[0].score;
        assert_eq!(name, FIELD_WEIGHT_NAME as f32 + recency);

        let (store, dir2) = temp_store();
        // NewObservation::new sets topic = kind, so kind="zterm" → topic hit only.
        store
            .insert(&NewObservation::new("zterm", "unrelated", "plain body"))
            .unwrap();
        let topic = store.search("zterm", 1, "").unwrap()[0].score;
        assert_eq!(topic, FIELD_WEIGHT_TOPIC as f32 + recency);

        let (store, dir3) = temp_store();
        store
            .insert(&NewObservation::new("kind", "unrelated", "zterm here"))
            .unwrap();
        let content = store.search("zterm", 1, "").unwrap()[0].score;
        assert_eq!(content, FIELD_WEIGHT_CONTENT as f32 + recency);

        assert!(name > topic && topic > content);
        for d in [dir, dir2, dir3] {
            std::fs::remove_dir_all(&d).ok();
        }
    }

    #[test]
    fn schema_is_read_from_the_runtime_catalog() {
        let (store, dir) = temp_store();
        let schema = store.schema().unwrap();
        assert_eq!(schema.len(), 1);
        assert_eq!(schema[0].name, "observations");
        let cols: Vec<&str> = schema[0].columns.iter().map(|c| c.name.as_str()).collect();
        for required in ["id", "at", "kind", "topic", "name", "content", "embedding"] {
            assert!(cols.contains(&required), "missing column {required}");
        }
        assert!(schema[0].indexes.iter().any(|i| i == "idx_obs_at"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn records_embedder_backend_for_cross_project_guard() {
        // Opening an embedding store records the backend that wrote its vectors
        // so a cross-project search can detect a vector-space mismatch later.
        let (store, dir) = temp_store();
        assert_eq!(
            store.recorded_embedder_backend().as_deref(),
            Some("hashing")
        );
        std::fs::remove_dir_all(&dir).ok();

        // A store opened without an embedder records no backend (None).
        let dir2 = std::env::temp_dir().join(format!("lingmiao-obs-noemb-{}", short_id("t")));
        std::fs::create_dir_all(&dir2).unwrap();
        let store2 = Observations::open_in_dir(&dir2, None).unwrap();
        assert_eq!(store2.recorded_embedder_backend(), None);
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[test]
    fn multiword_search_ors_words() {
        let (store, dir) = temp_store();
        store
            .insert(&NewObservation::new("fact", "a", "alpha only"))
            .unwrap();
        store
            .insert(&NewObservation::new("fact", "b", "beta only"))
            .unwrap();
        let hits = store.search("alpha beta", 10, "").unwrap();
        assert_eq!(hits.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn semantic_search_returns_scored_hits() {
        let (store, dir) = temp_store();
        store
            .insert(&NewObservation::new(
                "fact",
                "mem",
                "sqlite observations store",
            ))
            .unwrap();
        store
            .insert(&NewObservation::new(
                "fact",
                "unrelated",
                "banana helicopter",
            ))
            .unwrap();
        let hits = store.search_semantic("observations store", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score >= hits[1].score);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kinds_and_stats_and_pagination() {
        let (store, dir) = temp_store();
        store
            .insert(&NewObservation::new("fact", "a", "1"))
            .unwrap();
        store
            .insert(&NewObservation::new("constraint", "b", "2"))
            .unwrap();
        store
            .insert(&NewObservation::new("fact", "c", "3"))
            .unwrap();
        assert_eq!(store.stats().unwrap(), 3);
        let kinds = store.all_kinds().unwrap();
        assert_eq!(kinds, vec!["constraint".to_string(), "fact".to_string()]);
        assert_eq!(store.by_kind("fact", 10).unwrap().len(), 2);
        assert_eq!(store.list_all(2, 0, "").unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
