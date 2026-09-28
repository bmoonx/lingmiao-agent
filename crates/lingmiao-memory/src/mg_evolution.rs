//! MG evolution — 原版 `core/mg_evolution.py` 的 Rust 落地.
//!
//! Stage I of the original query loop: evolve the memory graph from recent
//! observations, **purely algorithmically** (no LLM calls). Recent observations
//! of the "memory-worthy" kinds are batch-searched against the knowledge graph;
//! the similarities form a weighted bipartite graph, which is partitioned into
//! communities; each community then either **updates** an existing node or
//! **adds** new nodes.
//!
//! ## Deviations from the Python original (documented)
//!
//! The original used `igraph` + `leidenalg` (two heavy native deps). This port
//! keeps the *behaviour* — similarity threshold, batch search, community-wise
//! update-vs-add — but implements community detection with a self-contained
//! **union-find over the threshold-filtered edges** (connected components), the
//! degenerate case Leiden's `ModularityVertexPartition` settles on for the
//! sparse star-shaped graphs this stage produces. No native dependency, same
//! outcome for the graphs in play.
//!
//! Following M6.4「真源不抄」the threshold and kind set are code constants.

use std::collections::{BTreeMap, HashMap};

use lingmiao_core::LingmiaoError;

use crate::knowledge::{KnowledgeGraph, NewNode};
use crate::observations::{Observation, Observations};

/// Cosine similarity above which an observation is considered related to an
/// existing node (Python `MG_SIM_THRESHOLD`).
pub const MG_SIM_THRESHOLD: f32 = 0.35;
/// Observation kinds worth evolving the graph from (Python `MG_RELEVANT_KINDS`).
pub const MG_RELEVANT_KINDS: [&str; 4] = ["fact", "preference", "decision", "constraint"];
/// How many recent observations to consider (Python `recent(30)`).
pub const MG_RECENT_WINDOW: usize = 30;
/// Cap on observations fed into the graph (Python `[:15]`).
pub const MG_MAX_OBS: usize = 15;
/// Neighbours per observation in the batch search (Python `top_k=5`).
pub const MG_TOP_K: usize = 5;

/// The outcome of one MG-evolution pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MgUpdateReport {
    /// Recent relevant observations considered.
    pub observations: usize,
    /// Distinct existing nodes matched above the threshold.
    pub matched_nodes: usize,
    /// Existing nodes updated (one per community that had a match).
    pub nodes_updated: u64,
    /// New nodes added from observations.
    pub nodes_added: u64,
    /// Communities found in the similarity graph.
    pub communities: u64,
}

/// Union-find root with path halving.
fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Sum of edge weights linking `node_v` to the community's observation vertices.
fn edge_weight(edges: &[(usize, usize, f32)], obs_vs: &[usize], node_v: usize) -> f32 {
    edges
        .iter()
        .filter(|(o, n, _)| *n == node_v && obs_vs.contains(o))
        .map(|(_, _, w)| *w)
        .sum()
}

/// Add one observation to the graph as a fresh node (the ADD path).
fn upsert_obs_node(knowledge: &KnowledgeGraph, obs: &Observation) -> Result<String, LingmiaoError> {
    let name = if obs.name.is_empty() {
        obs.topic.clone()
    } else {
        obs.name.clone()
    };
    knowledge.upsert_node(&NewNode {
        kind: &obs.kind,
        name: &name,
        summary: &obs.content,
        content: &obs.content,
        keywords: &obs.keywords,
        topic: &obs.topic,
        obs_ids: "[]",
    })
}

/// Evolve the memory graph from recent observations (no LLM).
///
/// Returns a [`MgUpdateReport`]. A store without an embedder (or with no
/// existing nodes) degrades gracefully: every observation becomes a new node.
pub fn update_mg(
    observations: &Observations,
    knowledge: &KnowledgeGraph,
) -> Result<MgUpdateReport, LingmiaoError> {
    // Step 1: recent memory-worthy observations.
    let mg_obs: Vec<Observation> = observations
        .recent(MG_RECENT_WINDOW)?
        .into_iter()
        .filter(|o| MG_RELEVANT_KINDS.contains(&o.kind.as_str()))
        .take(MG_MAX_OBS)
        .collect();
    let mut report = MgUpdateReport {
        observations: mg_obs.len(),
        ..Default::default()
    };
    if mg_obs.is_empty() {
        return Ok(report);
    }

    // Step 2: batch semantic search — each observation's similar existing nodes.
    let n_obs = mg_obs.len();
    let mut matched_ids: Vec<String> = Vec::new();
    let mut node_index: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<(usize, usize, f32)> = Vec::new();
    for (i, obs) in mg_obs.iter().enumerate() {
        for (node, score) in knowledge.search_semantic(&obs.content, MG_TOP_K)? {
            if score > MG_SIM_THRESHOLD {
                let idx = *node_index.entry(node.id.clone()).or_insert_with(|| {
                    matched_ids.push(node.id.clone());
                    n_obs + matched_ids.len() - 1
                });
                edges.push((i, idx, score));
            }
        }
    }
    report.matched_nodes = matched_ids.len();

    // No existing node is related → the original's `_add_new_observations`
    // fallback: every observation becomes a new node.
    if matched_ids.is_empty() {
        for obs in &mg_obs {
            upsert_obs_node(knowledge, obs)?;
            report.nodes_added += 1;
        }
        return Ok(report);
    }

    // Step 3: community detection (union-find over the thresholded edges).
    let n_total = n_obs + matched_ids.len();
    let mut parent: Vec<usize> = (0..n_total).collect();
    for &(a, b, _) in &edges {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }
    let mut communities: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for v in 0..n_total {
        let r = find(&mut parent, v);
        communities.entry(r).or_default().push(v);
    }
    report.communities = communities.len() as u64;

    // Step 4: per community — UPDATE the best-matched node, or ADD new nodes.
    for vertices in communities.values() {
        let obs_vs: Vec<usize> = vertices.iter().copied().filter(|v| *v < n_obs).collect();
        let mg_vs: Vec<usize> = vertices.iter().copied().filter(|v| *v >= n_obs).collect();
        if obs_vs.is_empty() {
            continue; // community of only existing nodes — nothing to evolve
        }
        if mg_vs.is_empty() {
            for &ov in &obs_vs {
                upsert_obs_node(knowledge, &mg_obs[ov])?;
                report.nodes_added += 1;
            }
        } else {
            let best = mg_vs
                .iter()
                .copied()
                .max_by(|&x, &y| {
                    edge_weight(&edges, &obs_vs, x)
                        .partial_cmp(&edge_weight(&edges, &obs_vs, y))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(mg_vs[0]);
            let best_id = &matched_ids[best - n_obs];
            for &ov in &obs_vs {
                let obs = &mg_obs[ov];
                knowledge.append_to_node(best_id, &obs.content, &obs.keywords)?;
            }
            report.nodes_updated += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashingEmbedder;
    use crate::id::short_id;
    use crate::observations::NewObservation;
    use std::sync::Arc;

    fn temp_stores() -> (Observations, KnowledgeGraph, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lingmiao-mg-{}", short_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let emb: Arc<dyn crate::embed::Embedder> = Arc::new(HashingEmbedder::new());
        let obs = Observations::open_in_dir(&dir, Some(emb.clone())).unwrap();
        let kg = KnowledgeGraph::open_in_dir(&dir, Some(emb)).unwrap();
        (obs, kg, dir)
    }

    #[test]
    fn no_relevant_observations_is_a_noop() {
        let (obs, kg, dir) = temp_stores();
        obs.insert(&NewObservation::new("turn", "chat", "just a chat turn"))
            .unwrap();
        let report = update_mg(&obs, &kg).unwrap();
        assert_eq!(report, MgUpdateReport::default());
        assert_eq!(kg.stats().unwrap().nodes, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adds_new_nodes_when_nothing_matches() {
        let (obs, kg, dir) = temp_stores();
        for (i, body) in ["alpha unique fact", "beta unique fact"].iter().enumerate() {
            obs.insert(&NewObservation::new("fact", &format!("f{i}"), body))
                .unwrap();
        }
        let report = update_mg(&obs, &kg).unwrap();
        assert_eq!(report.observations, 2);
        assert_eq!(report.matched_nodes, 0);
        assert_eq!(report.nodes_added, 2);
        assert_eq!(kg.stats().unwrap().nodes, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn updates_a_matched_node() {
        let (obs, kg, dir) = temp_stores();
        kg.upsert_node(&NewNode::new(
            "technology",
            "rust",
            "systems language",
            "rust is a systems language",
        ))
        .unwrap();
        obs.insert(&NewObservation::new(
            "fact",
            "rust fact",
            "rust is a systems language",
        ))
        .unwrap();
        let report = update_mg(&obs, &kg).unwrap();
        assert!(report.matched_nodes >= 1, "{report:?}");
        assert_eq!(report.nodes_updated, 1, "{report:?}");
        assert_eq!(report.nodes_added, 0, "{report:?}");
        let node = kg.find_node("technology", "rust").unwrap().unwrap();
        assert!(
            node.content.contains("\n---\n"),
            "content not appended: {}",
            node.content
        );
        assert_eq!(kg.stats().unwrap().nodes, 1, "no extra node created");
        std::fs::remove_dir_all(&dir).ok();
    }
}
