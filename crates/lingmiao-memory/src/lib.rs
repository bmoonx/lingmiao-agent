//! # lingmiao-memory — the memory layer (M2)
//!
//! Four SQLite stores on one shared 384-dim embedding space (decisions Q4/A4):
//!
//! | store          | module              | role                                   |
//! |----------------|---------------------|----------------------------------------|
//! | #1 archive     | [`archive`]         | full per-turn transcript (`turns`)     |
//! | #2 observations| [`observations`]    | facts / decisions / constraints / …    |
//! | #3 knowledge   | [`knowledge`]       | concept graph (`nodes` + `edges`)      |
//! | #4 business    | [`business`]        | free-form project data (agent schema)  |
//!
//! Each store exists in **three isolated zones** (ADR A1) — the root chat zone
//! and one per role loop (`main/`, `auditor/`). [`Memory`] bundles the four
//! stores for a single zone; [`Memory::open_all_zones`] opens all three.
//!
//! ## Embeddings
//!
//! All stores share one [`Embedder`] so vectors are comparable across layers.
//! The production embedder is [`FastEmbedder`] (ONNX multilingual-e5-small,
//! 384-dim; weights embedded in the binary), loaded by [`default_embedder`]; as
//! of ④真语义 it is mandatory and has no lexical fallback. [`HashingEmbedder`]
//! survives only for deterministic tests. When an existing store's vectors were
//! written by a *different* backend, [`open_default_zone`] performs a one-time
//! [`Memory::rebuild_embeddings`].

#![forbid(unsafe_code)]

pub mod archive;
pub mod business;
pub mod embed;
pub mod id;
pub mod knowledge;
pub mod mg_evolution;
pub mod observations;
pub mod store;

pub use archive::{Archive, ScoredTurn, Turn};
pub use business::Business;
pub use embed::FastEmbedder;
pub use embed::{EMBEDDER_BACKEND, EMBEDDING_DIM, Embedder, HashingEmbedder};
pub use id::{now_iso, short_id};
pub use knowledge::{Edge, GraphStats, KnowledgeGraph, LINK_LAYERS, Link, NewNode, Node};
pub use mg_evolution::{MG_RELEVANT_KINDS, MG_SIM_THRESHOLD, MgUpdateReport, update_mg};
pub use observations::{
    FIELD_WEIGHT_CONTENT, FIELD_WEIGHT_NAME, FIELD_WEIGHT_TOPIC, NewObservation, Observation,
    Observations, ScoredObservation,
};
pub use store::Zone;
pub use store::{ColumnInfo, TableSchema, ZoneCounts, read_table_schema, read_zone_counts};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lingmiao_core::{LingmiaoError, Paths};

/// All four stores for one zone, sharing a single embedder.
///
/// Open with [`Memory::open`] (from resolved [`Paths`]) or
/// [`Memory::open_in_dir`] (from an explicit directory, e.g. in tests).
pub struct Memory {
    /// Which zone this bundle belongs to.
    pub zone: Zone,
    /// Directory holding the four DB files.
    pub dir: PathBuf,
    /// #2 observations store.
    pub observations: Observations,
    /// #3 knowledge graph.
    pub knowledge: KnowledgeGraph,
    /// #1 turn archive.
    pub archive: Archive,
    /// #4 business database.
    pub business: Business,
}

impl Memory {
    /// Open all four stores rooted at an explicit directory.
    pub fn open_in_dir(
        dir: &Path,
        zone: Zone,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        let shared = embedder;
        Ok(Self {
            zone,
            dir: dir.to_path_buf(),
            observations: Observations::open_in_dir(dir, shared.clone())?,
            knowledge: KnowledgeGraph::open_in_dir(dir, shared.clone())?,
            archive: Archive::open_in_dir(dir, shared)?,
            business: Business::open_in_dir(dir)?,
        })
    }

    /// Open one zone's stores under the project's `memory/` directory.
    pub fn open(
        paths: &Paths,
        zone: Zone,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Self, LingmiaoError> {
        Self::open_in_dir(&zone.dir(&paths.memory_dir), zone, embedder)
    }

    /// Open the chat / main / auditor zones, in [`Zone::ALL`] order.
    pub fn open_all_zones(
        paths: &Paths,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<[Memory; 3], LingmiaoError> {
        let mut out: Vec<Memory> = Vec::with_capacity(3);
        for zone in Zone::ALL {
            out.push(Self::open(paths, zone, embedder.clone())?);
        }
        Ok([out.remove(0), out.remove(0), out.remove(0)])
    }

    /// The backend identifier of the shared embedder (需求⑤ `embed` view).
    /// `None` when the bundle was opened without an embedder.
    pub fn embedder_backend(&self) -> Option<&'static str> {
        self.observations.embedder_backend()
    }

    /// The shared embedder handle, if any. Lets a cross-project search reuse the
    /// running process's vector space (same embedding space across projects).
    pub fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.observations.embedder()
    }

    /// Evolve the memory graph from recent observations (原版 stage I / MG
    /// evolution) — purely algorithmic, no LLM. See [`mg_evolution`].
    pub fn evolve_memory_graph(&self) -> Result<MgUpdateReport, LingmiaoError> {
        update_mg(&self.observations, &self.knowledge)
    }

    /// True when this zone's stored vectors were written by a **different**
    /// embedder backend than the one currently open (a stale vector space, e.g.
    /// lexical vectors from before the semantic model was made mandatory).
    ///
    /// `recorded_embedder_backend` is written first-wins, so this stays true
    /// until [`Memory::rebuild_embeddings`] refreshes the provenance.
    pub fn embedder_backend_stale(&self) -> bool {
        match (
            self.observations.recorded_embedder_backend(),
            self.observations.embedder_backend(),
        ) {
            (Some(recorded), Some(current)) => recorded != current,
            _ => false,
        }
    }

    /// Recompute every stored embedding (observations, KG nodes, archive turns)
    /// with the current embedder and refresh the recorded provenance — the
    /// one-time vector-space migration after switching embedder backends
    /// (④真语义).
    pub fn rebuild_embeddings(&self) -> Result<RebuildReport, LingmiaoError> {
        Ok(RebuildReport {
            observations: self.observations.reembed_all()?,
            knowledge_nodes: self.knowledge.reembed_all()?,
            archive_turns: self.archive.reembed_all()?,
        })
    }
}

/// Outcome of a [`Memory::rebuild_embeddings`] run (④真语义).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Observations re-embedded.
    pub observations: u64,
    /// Knowledge-graph nodes re-embedded.
    pub knowledge_nodes: u64,
    /// Archive turns re-embedded.
    pub archive_turns: u64,
}

impl RebuildReport {
    /// Total rows re-embedded across the three embedding stores.
    pub fn total(&self) -> u64 {
        self.observations + self.knowledge_nodes + self.archive_turns
    }
}

/// Load the production semantic embedder: [`FastEmbedder`]
/// (multilingual-e5-small, 384-dim; weights embedded in the binary).
///
/// As of ④真语义 there is **no lexical fallback**: a load failure is returned as
/// an error so the caller can decide to run without memory, rather than
/// silently substituting non-semantic vectors.
pub fn default_embedder() -> Result<Arc<dyn Embedder>, LingmiaoError> {
    match embed::FastEmbedder::new() {
        Ok(e) => {
            tracing::info!("memory: using {} (384d)", embed::EMBEDDER_BACKEND);
            Ok(Arc::new(e))
        }
        Err(err) => Err(LingmiaoError::memory(
            store::STORE,
            format!("load {}: {err}", embed::EMBEDDER_BACKEND),
        )),
    }
}

/// Open one zone with the production semantic embedder.
///
/// When the zone's vectors were written by a different backend (e.g. the old
/// lexical fallback), a one-time [`Memory::rebuild_embeddings`] re-embeds them
/// into the current vector space (④真语义 vector-space migration).
pub fn open_default_zone(paths: &Paths, zone: Zone) -> Result<Memory, LingmiaoError> {
    let mem = Memory::open(paths, zone, Some(default_embedder()?))?;
    // NOTE (④真语义): only the observations / knowledge stores persist a
    // `store_meta` backend marker — the #1 archive (`context_record.db`) has
    // none, so its vectors have no independent staleness signal. They ride the
    // observations store: a stale detection here re-embeds *all three* stores
    // via `rebuild_embeddings`, migrating the archive in lock-step.
    if mem.embedder_backend_stale() {
        let started = std::time::Instant::now();
        let report = mem.rebuild_embeddings()?;
        tracing::info!(
            "memory: rebuilt stale embeddings ({} observations, {} nodes, {} turns) in {:.1}s",
            report.observations,
            report.knowledge_nodes,
            report.archive_turns,
            started.elapsed().as_secs_f64()
        );
    }
    Ok(mem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_core::Paths;

    /// A test embedder producing the same lexical vectors as [`HashingEmbedder`]
    /// but reporting a *different* backend id — used to simulate a store written
    /// in another vector space (④真语义 stale-detection). The id it claims is the
    /// **real retired backend** (`all-MiniLM-L6-v2`) — exactly what an existing
    /// zone recorded before the 2026-10-01 model switch, so this test doubles as
    /// the migration path's regression guard.
    struct OtherBackendEmbedder;

    impl Embedder for OtherBackendEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            HashingEmbedder::new().embed(text)
        }
        fn backend(&self) -> &'static str {
            "fastembed/all-MiniLM-L6-v2"
        }
    }

    #[test]
    fn stale_vector_space_triggers_rebuild_and_refreshes_provenance() {
        let dir = std::env::temp_dir().join(format!("lingmiao-mem-rebuild-{}", short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        // First writer records the "semantic" backend.
        {
            let m = Memory::open_in_dir(&dir, Zone::Chat, Some(Arc::new(OtherBackendEmbedder)))
                .unwrap();
            m.observations
                .insert(&NewObservation::new("fact", "a", "hello world"))
                .unwrap();
            m.knowledge
                .upsert_node(&NewNode::new("project", "p", "s", "content"))
                .unwrap();
            m.archive.save(&Turn::new("hi", "yo")).unwrap();
        }
        // Reopening with a different backend sees a stale vector space.
        let m =
            Memory::open_in_dir(&dir, Zone::Chat, Some(Arc::new(HashingEmbedder::new()))).unwrap();
        assert!(m.embedder_backend_stale(), "recorded backend must persist");
        let rep = m.rebuild_embeddings().unwrap();
        assert_eq!(rep.observations, 1);
        assert_eq!(rep.knowledge_nodes, 1);
        assert_eq!(rep.archive_turns, 1);
        assert_eq!(rep.total(), 3);
        assert!(
            !m.embedder_backend_stale(),
            "provenance must be refreshed after rebuild (one-time)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opens_four_stores_in_one_zone() {
        let dir = std::env::temp_dir().join(format!("lingmiao-mem-zone-{}", short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let mem = Memory::open_in_dir(&dir, Zone::Chat, Some(Arc::new(HashingEmbedder::new())))
            .expect("open");
        let id = mem
            .observations
            .insert(&NewObservation::new("fact", "x", "hello world"))
            .unwrap();
        assert!(mem.observations.get(&id).unwrap().is_some());
        mem.knowledge
            .upsert_node(&NewNode::new("project", "lingmiao", "s", "c"))
            .unwrap();
        mem.archive.save(&Turn::new("hi", "yo")).unwrap();
        mem.business.execute("CREATE TABLE t (a TEXT)").unwrap();
        assert_eq!(mem.observations.stats().unwrap(), 1);
        assert_eq!(mem.knowledge.stats().unwrap().nodes, 1);
        assert_eq!(mem.archive.count().unwrap(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn three_zones_are_isolated() {
        let root = std::env::temp_dir().join(format!("lingmiao-mem-zones-{}", short_id("t")));
        let paths = Paths::at(&root);
        paths.ensure_dirs().unwrap();
        let zones = Memory::open_all_zones(&paths, None).expect("open all");
        zones[0] // chat
            .observations
            .insert(&NewObservation::new("fact", "a", "chat only"))
            .unwrap();
        assert_eq!(zones[0].observations.stats().unwrap(), 1);
        assert_eq!(zones[1].observations.stats().unwrap(), 0); // main
        assert_eq!(zones[2].observations.stats().unwrap(), 0); // auditor
        // The role zones must live in sub-directories.
        assert!(paths.memory_dir.join("main/observations.db").exists());
        assert!(paths.memory_dir.join("auditor/observations.db").exists());
        assert!(
            Zone::Chat
                .db_path(&paths.memory_dir, "observations.db")
                .exists()
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
