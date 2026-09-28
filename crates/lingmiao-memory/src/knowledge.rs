//! Knowledge graph (#3 memory) — ported from `memory/knowledge.py`.
//!
//! A directed graph of concepts (`nodes`) linked by typed `edges`. Nodes carry
//! embeddings in the same 384-dim space as observations (ADR A4), so the two
//! layers can be compared with a plain cosine.
//!
//! The original also ran small-world clustering (igraph/leidenalg). That is an
//! analysis afterthought, not part of retrieval, and is intentionally left out
//! of this port (it would drag in two heavy native deps for a feature the Rust
//! rewrite's `search_memory` path does not use).

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use lingmiao_core::LingmiaoError;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::embed::{Embedder, cosine, decode_blob, encode_blob};
use crate::id::{now_iso, short_id};
use crate::store::{STORE, lock, open_db};

const DB_NAME: &str = "knowledge.db";

/// A graph node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// `node-<12hex>`.
    pub id: String,
    /// Node kind (`project` / `person` / `technology` / `decision` / …).
    pub kind: String,
    /// Unique-within-kind name; the upsert key together with `kind`.
    pub name: String,
    /// One-line summary.
    pub summary: String,
    /// Full content.
    pub content: String,
    /// Comma-separated keywords (carries `project=CODE` for cross-store links).
    pub keywords: String,
    /// Linked observation ids (JSON array).
    pub obs_ids: String,
    /// Optional topic grouping.
    pub topic: String,
    /// RFC-3339 creation time.
    pub created_at: String,
    /// RFC-3339 last-update time.
    pub updated_at: String,
    /// Monotonic revision counter, bumped on every upsert of an existing node.
    pub version: i64,
}

/// A directed, typed edge between two nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// Rowid.
    pub id: i64,
    /// Source node id.
    pub from_id: String,
    /// Target node id.
    pub to_id: String,
    /// Relation verb (e.g. `depends_on`).
    pub relation: String,
    /// Free-text detail.
    pub description: String,
    /// RFC-3339 creation time.
    pub created_at: String,
}

/// Fields for a new node; id/timestamps/version are managed by the store.
#[derive(Debug, Clone)]
pub struct NewNode<'a> {
    /// Node kind.
    pub kind: &'a str,
    /// Unique-within-kind name.
    pub name: &'a str,
    /// One-line summary.
    pub summary: &'a str,
    /// Full content.
    pub content: &'a str,
    /// Search keywords.
    pub keywords: &'a str,
    /// Optional topic.
    pub topic: &'a str,
    /// Linked observation ids (JSON array).
    pub obs_ids: &'a str,
}

impl NewNode<'_> {
    /// Convenience constructor with the original's defaults.
    pub fn new<'a>(
        kind: &'a str,
        name: &'a str,
        summary: &'a str,
        content: &'a str,
    ) -> NewNode<'a> {
        NewNode {
            kind,
            name,
            summary,
            content,
            keywords: "",
            topic: "",
            obs_ids: "[]",
        }
    }
}

/// Node / edge counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStats {
    /// Number of nodes.
    pub nodes: u64,
    /// Number of edges.
    pub edges: u64,
}

/// The knowledge graph store.
pub struct KnowledgeGraph {
    conn: Mutex<Connection>,
    embedder: Option<Arc<dyn Embedder>>,
}

impl KnowledgeGraph {
    /// Open (creating if needed) the graph at `path`.
    pub fn open(path: &Path, embedder: Option<Arc<dyn Embedder>>) -> Result<Self, LingmiaoError> {
        let conn = open_db(path)?;
        Self::init_schema(&conn)?;
        crate::store::record_embedder_backend(&conn, embedder.as_ref().map(|e| e.backend()));
        Ok(Self {
            conn: Mutex::new(conn),
            embedder,
        })
    }

    /// Open the graph inside a zone directory.
    pub fn open_in_dir(
        dir: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open(&dir.join(DB_NAME), embedder)
    }

    /// Open an **existing** graph read-only (cross-project search); no file
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

    /// Read-only variant of [`KnowledgeGraph::open_in_dir`].
    pub fn open_in_dir_readonly(
        dir: &Path,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open_readonly(&dir.join(DB_NAME), embedder)
    }

    fn init_schema(conn: &Connection) -> Result<(), LingmiaoError> {
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS nodes (
                id         TEXT PRIMARY KEY,
                kind       TEXT NOT NULL,
                name       TEXT NOT NULL,
                summary    TEXT NOT NULL,
                content    TEXT DEFAULT '',
                keywords   TEXT DEFAULT '',
                embedding  BLOB,
                obs_ids    TEXT DEFAULT '[]',
                topic      TEXT DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                version    INTEGER DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS edges (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id     TEXT NOT NULL,
                to_id       TEXT NOT NULL,
                relation    TEXT NOT NULL,
                description TEXT DEFAULT '',
                created_at  TEXT NOT NULL,
                FOREIGN KEY (from_id) REFERENCES nodes(id),
                FOREIGN KEY (to_id)   REFERENCES nodes(id)
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_kind  ON nodes(kind);
            CREATE INDEX IF NOT EXISTS idx_nodes_name  ON nodes(name);
            CREATE INDEX IF NOT EXISTS idx_nodes_topic ON nodes(topic);
            CREATE INDEX IF NOT EXISTS idx_edges_from  ON edges(from_id);
            CREATE INDEX IF NOT EXISTS idx_edges_to    ON edges(to_id);
            {}",
            crate::store::META_TABLE_DDL,
        ))
        .map_err(|e| LingmiaoError::memory(STORE, format!("create knowledge schema: {e}")))?;
        Ok(())
    }

    fn embed(&self, text: &str) -> Option<Vec<u8>> {
        self.embedder.as_ref().map(|e| encode_blob(&e.embed(text)))
    }

    /// Insert or update a node keyed by `(kind, name)`.
    ///
    /// Returns the node id. On update the content/summary/keywords/topic/obs_ids
    /// are replaced, `updated_at` refreshed and `version` incremented — matching
    /// the original `update_knowledge` idempotency contract.
    pub fn upsert_node(&self, new: &NewNode<'_>) -> Result<String, LingmiaoError> {
        let conn = lock(&self.conn);
        let existing: Option<(String, i64)> = conn
            .query_row(
                "SELECT id, version FROM nodes WHERE kind=?1 AND name=?2",
                params![new.kind, new.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let now = now_iso();
        let embedding = self.embed(new.content);
        match existing {
            Some((id, version)) => {
                conn.execute(
                    "UPDATE nodes SET summary=?2, content=?3, keywords=?4, obs_ids=?5, topic=?6,
                     embedding=?7, updated_at=?8, version=?9 WHERE id=?1",
                    params![
                        id,
                        new.summary,
                        new.content,
                        new.keywords,
                        new.obs_ids,
                        new.topic,
                        embedding,
                        now,
                        version + 1,
                    ],
                )
                .map_err(|e| LingmiaoError::memory(STORE, format!("update node: {e}")))?;
                Ok(id)
            }
            None => {
                let id = short_id("node");
                conn.execute(
                    "INSERT INTO nodes
                     (id, kind, name, summary, content, keywords, embedding, obs_ids, topic, created_at, updated_at, version)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,1)",
                    params![
                        id,
                        new.kind,
                        new.name,
                        new.summary,
                        new.content,
                        new.keywords,
                        embedding,
                        new.obs_ids,
                        new.topic,
                        now,
                    ],
                )
                .map_err(|e| LingmiaoError::memory(STORE, format!("insert node: {e}")))?;
                Ok(id)
            }
        }
    }

    /// Fetch a node by id.
    pub fn get_node(&self, id: &str) -> Result<Option<Node>, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row("SELECT * FROM nodes WHERE id=?1", params![id], row_to_node)
            .optional()
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Find a node by its `(kind, name)` upsert key.
    pub fn find_node(&self, kind: &str, name: &str) -> Result<Option<Node>, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT * FROM nodes WHERE kind=?1 AND name=?2",
            params![kind, name],
            row_to_node,
        )
        .optional()
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// Append an observation's content to an existing node's summary / content
    /// (and its keywords), keeping the embedding in step — the MG-evolution
    /// UPDATE path (`mg_evolution`). Returns `Ok(())` when the node is gone.
    pub fn append_to_node(
        &self,
        id: &str,
        obs_content: &str,
        obs_keywords: &str,
    ) -> Result<(), LingmiaoError> {
        let Some(node) = self.get_node(id)? else {
            return Ok(());
        };
        let summary = if node.summary.is_empty() {
            obs_content.to_string()
        } else {
            format!("{}; {}", node.summary, obs_content)
        };
        let keywords = match (node.keywords.is_empty(), obs_keywords.is_empty()) {
            (true, _) => obs_keywords.to_string(),
            (false, true) => node.keywords.clone(),
            (false, false) => format!("{}, {}", node.keywords, obs_keywords),
        };
        let content = format!("{}\n---\n{}", node.content, obs_content);
        let new = NewNode {
            kind: &node.kind,
            name: &node.name,
            summary: &summary,
            content: &content,
            keywords: &keywords,
            topic: &node.topic,
            obs_ids: &node.obs_ids,
        };
        self.upsert_node(&new)?;
        Ok(())
    }

    /// List nodes (optionally by kind), newest-updated first.
    pub fn list_nodes(
        &self,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Node>, LingmiaoError> {
        let conn = lock(&self.conn);
        let (sql, bind_kind) = if kind.is_empty() {
            (
                "SELECT * FROM nodes ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
                "",
            )
        } else {
            (
                "SELECT * FROM nodes WHERE kind=?1 ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3",
                kind,
            )
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = if bind_kind.is_empty() {
            stmt.query_map(params![limit as i64, offset as i64], row_to_node)
        } else {
            stmt.query_map(params![kind, limit as i64, offset as i64], row_to_node)
        }
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        Ok(out)
    }

    /// Distinct node kinds, sorted.
    pub fn all_kinds(&self) -> Result<Vec<String>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare("SELECT DISTINCT kind FROM nodes ORDER BY kind")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        Ok(out)
    }

    /// Add an edge. Edges are append-only (duplicates are allowed, matching the
    /// original which never de-duplicated relations).
    pub fn add_edge(
        &self,
        from_id: &str,
        to_id: &str,
        relation: &str,
        description: &str,
    ) -> Result<i64, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.execute(
            "INSERT INTO edges (from_id, to_id, relation, description, created_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![from_id, to_id, relation, description, now_iso()],
        )
        .map_err(|e| LingmiaoError::memory(STORE, format!("insert edge: {e}")))?;
        Ok(conn.last_insert_rowid())
    }

    /// Edges originating at `id`.
    pub fn edges_from(&self, id: &str) -> Result<Vec<Edge>, LingmiaoError> {
        self.edges_where("from_id", id)
    }

    /// Edges pointing at `id`.
    pub fn edges_to(&self, id: &str) -> Result<Vec<Edge>, LingmiaoError> {
        self.edges_where("to_id", id)
    }

    fn edges_where(&self, column: &str, id: &str) -> Result<Vec<Edge>, LingmiaoError> {
        // `column` is a fixed internal literal, never user input.
        let sql = format!("SELECT * FROM edges WHERE {column}=?1 ORDER BY id");
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map(params![id], row_to_edge)
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
        }
        Ok(out)
    }

    /// Node and edge counts.
    pub fn stats(&self) -> Result<GraphStats, LingmiaoError> {
        let conn = lock(&self.conn);
        let nodes = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get::<_, i64>(0))
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let edges = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get::<_, i64>(0))
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        Ok(GraphStats {
            nodes: nodes.max(0) as u64,
            edges: edges.max(0) as u64,
        })
    }

    /// Cosine semantic search over node embeddings.
    pub fn search_semantic(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(Node, f32)>, LingmiaoError> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let qv = embedder.embed(query);
        let conn = lock(&self.conn);
        let mut stmt = conn
            .prepare("SELECT * FROM nodes WHERE embedding IS NOT NULL")
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                let node = row_to_node(r)?;
                let blob: Option<Vec<u8>> = r.get("embedding")?;
                Ok((node, blob))
            })
            .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
        let mut hits: Vec<(Node, f32)> = Vec::new();
        for row in rows {
            let (node, blob) = row.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let ev = blob.map(|b| decode_blob(&b)).unwrap_or_default();
            hits.push((node, cosine(&ev, &qv)));
        }
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k);
        Ok(hits)
    }

    /// Number of nodes whose `embedding` column is NULL (backfill pending).
    pub fn missing_embeddings(&self) -> Result<u64, LingmiaoError> {
        let conn = lock(&self.conn);
        conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE embedding IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as u64)
        .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))
    }

    /// The backend identifier of this store's embedder (需求⑤ `embed` view).
    pub fn embedder_backend(&self) -> Option<&'static str> {
        self.embedder.as_ref().map(|e| e.backend())
    }

    /// The embedder backend this store **recorded** when its vectors were
    /// written (cross-project space guard), or `None` for a store written before
    /// the metadata table existed. Read from the live DB, so it works on a
    /// read-only connection too.
    pub fn recorded_embedder_backend(&self) -> Option<String> {
        let conn = lock(&self.conn);
        crate::store::read_meta(&conn, crate::store::META_EMBEDDER_BACKEND)
    }

    /// Recompute every node's embedding with the current embedder and refresh the
    /// recorded provenance — the KG half of the ④真语义 vector-space migration.
    /// Returns the number of nodes updated; a no-op (`0`) without an embedder.
    pub fn reembed_all(&self) -> Result<u64, LingmiaoError> {
        let Some(embedder) = &self.embedder else {
            return Ok(0);
        };
        let rows: Vec<(String, String)> = {
            let conn = lock(&self.conn);
            let mut stmt = conn
                .prepare("SELECT id, content FROM nodes")
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| LingmiaoError::memory(STORE, e.to_string()))?);
            }
            out
        };
        let conn = lock(&self.conn);
        let mut n = 0u64;
        for (id, content) in rows {
            let blob = encode_blob(&embedder.embed(&content));
            conn.execute(
                "UPDATE nodes SET embedding=?2 WHERE id=?1",
                params![id, blob],
            )
            .map_err(|e| LingmiaoError::memory(STORE, format!("reembed node: {e}")))?;
            n += 1;
        }
        crate::store::write_meta(
            &conn,
            crate::store::META_EMBEDDER_BACKEND,
            embedder.backend(),
        )?;
        Ok(n)
    }

    /// The runtime schema of this store (需求⑤ `store`/`schema` view): `nodes`
    /// then `edges`, read live from the SQLite catalog.
    pub fn schema(&self) -> Result<Vec<crate::store::TableSchema>, LingmiaoError> {
        let conn = lock(&self.conn);
        let mut out = Vec::new();
        for table in ["nodes", "edges"] {
            if let Some(s) = crate::store::read_table_schema(&conn, table)? {
                out.push(s);
            }
        }
        Ok(out)
    }
}

fn row_to_node(r: &Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get("id")?,
        kind: r.get("kind")?,
        name: r.get("name")?,
        summary: r.get("summary")?,
        content: r.get("content")?,
        keywords: r.get("keywords")?,
        obs_ids: r.get("obs_ids")?,
        topic: r.get("topic")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        version: r.get("version")?,
    })
}

fn row_to_edge(r: &Row<'_>) -> rusqlite::Result<Edge> {
    Ok(Edge {
        id: r.get("id")?,
        from_id: r.get("from_id")?,
        to_id: r.get("to_id")?,
        relation: r.get("relation")?,
        description: r.get("description")?,
        created_at: r.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashingEmbedder;

    fn temp_graph() -> (KnowledgeGraph, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lingmiao-kg-{}", short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let g = KnowledgeGraph::open_in_dir(&dir, Some(Arc::new(HashingEmbedder::new()))).unwrap();
        (g, dir)
    }

    #[test]
    fn upsert_is_idempotent_on_kind_and_name() {
        let (g, dir) = temp_graph();
        let a = g
            .upsert_node(&NewNode::new("project", "lingmiao", "s1", "content one"))
            .unwrap();
        let b = g
            .upsert_node(&NewNode::new("project", "lingmiao", "s2", "content two"))
            .unwrap();
        assert_eq!(a, b, "same (kind,name) must map to one node");
        let node = g.get_node(&a).unwrap().unwrap();
        assert_eq!(node.summary, "s2");
        assert_eq!(node.version, 2);
        assert_eq!(g.stats().unwrap().nodes, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_name_different_kind_are_distinct() {
        let (g, dir) = temp_graph();
        let a = g
            .upsert_node(&NewNode::new("project", "x", "s", "c"))
            .unwrap();
        let b = g
            .upsert_node(&NewNode::new("person", "x", "s", "c"))
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(g.stats().unwrap().nodes, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edges_are_directional_and_appended() {
        let (g, dir) = temp_graph();
        let a = g
            .upsert_node(&NewNode::new("project", "a", "s", "c"))
            .unwrap();
        let b = g
            .upsert_node(&NewNode::new("project", "b", "s", "c"))
            .unwrap();
        g.add_edge(&a, &b, "depends_on", "a→b").unwrap();
        assert_eq!(g.edges_from(&a).unwrap().len(), 1);
        assert_eq!(g.edges_to(&a).unwrap().len(), 0);
        assert_eq!(g.edges_to(&b).unwrap().len(), 1);
        assert_eq!(g.stats().unwrap().edges, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_nodes_filters_by_kind() {
        let (g, dir) = temp_graph();
        g.upsert_node(&NewNode::new("project", "a", "s", "c"))
            .unwrap();
        g.upsert_node(&NewNode::new("decision", "b", "s", "c"))
            .unwrap();
        assert_eq!(g.list_nodes("", 10, 0).unwrap().len(), 2);
        assert_eq!(g.list_nodes("decision", 10, 0).unwrap().len(), 1);
        assert_eq!(g.all_kinds().unwrap(), vec!["decision", "project"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn semantic_search_orders_by_score() {
        let (g, dir) = temp_graph();
        g.upsert_node(&NewNode::new(
            "project",
            "mem",
            "s",
            "sqlite observations store",
        ))
        .unwrap();
        g.upsert_node(&NewNode::new("project", "far", "s", "banana helicopter"))
            .unwrap();
        let hits = g.search_semantic("observations store", 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].1 >= hits[1].1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
