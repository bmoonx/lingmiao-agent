//! Embedding support — the 384-dim vector space shared by observations and the
//! knowledge graph (ADR A4).
//!
//! ## Design
//!
//! Embeddings are stored as little-endian `f32` blobs (384 × 4 = 1536 bytes),
//! exactly like the Python original (`np.asarray(vec, dtype=np.float32).tobytes()`).
//! Every store shares **one** [`Embedder`] instance so observations and KG nodes
//! live in the same vector space and can be compared across layers.
//!
//! ## Model status
//!
//! The production model is `all-MiniLM-L6-v2` via [`FastEmbedder`]
//! (`fastembed-rs` / ONNX runtime). As of ④真语义 it is **mandatory**: the
//! `fastembed` dependency is unconditional and [`crate::default_embedder`] has
//! no lexical fallback — a load failure is a hard error.
//!
//! [`HashingEmbedder`] remains only as a *deterministic* embedder for tests
//! (and explicit non-semantic uses): it hashes lowercased tokens into a
//! normalised bag-of-words vector. It gives non-zero cosine similarity for token
//! overlap, which keeps the storage/search plumbing exercisable without loading
//! a 90 MB model, but it must never be the production embedder.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Embedding width — fixed at 384 by ADR A4 (MiniLM-L6-v2 output size).
pub const EMBEDDING_DIM: usize = 384;

/// A text → vector encoder. Implementations must be thread-safe (stores are
/// shared across the pipeline's parallel stages).
pub trait Embedder: Send + Sync {
    /// The dimensionality of produced vectors. Defaults to [`EMBEDDING_DIM`].
    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    /// Encode `text` into a vector of length [`Embedder::dim`].
    fn embed(&self, text: &str) -> Vec<f32>;

    /// A short, stable backend identifier surfaced by the meta platform's
    /// `embed` view (需求⑤). Concrete embedders override it (`hashing` /
    /// `fastembed/all-MiniLM-L6-v2`); the default keeps third-party
    /// implementations compiling.
    fn backend(&self) -> &'static str {
        "custom"
    }
}

/// Serialise a vector to the little-endian `f32` blob used in SQLite.
pub fn encode_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode a little-endian `f32` blob back into a vector.
///
/// A trailing partial word (if any) is ignored; an empty blob yields an empty
/// vector. Callers comparing vectors should guard for length mismatch.
pub fn decode_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity of two vectors (0.0 if either is empty or all-zero).
///
/// Vectors shorter than the full width are compared over their common prefix,
/// so a zero-length decoded blob degrades gracefully instead of panicking.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Tokenise text for the lexical embedder.
///
/// ASCII alphanumeric runs become one token; each CJK/other non-ASCII
/// alphanumeric character becomes its own token; everything else is a
/// separator. Consecutive tokens are additionally emitted as bigrams to give
/// neighbouring CJK characters a shared signal.
fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut atoms: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            cur.push(ch);
        } else if ch.is_alphanumeric() {
            if !cur.is_empty() {
                atoms.push(std::mem::take(&mut cur));
            }
            atoms.push(ch.to_string());
        } else if !cur.is_empty() {
            atoms.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        atoms.push(cur);
    }

    let mut tokens = atoms.clone();
    for pair in atoms.windows(2) {
        tokens.push(format!("{}{}", pair[0], pair[1]));
    }
    tokens
}

fn hash_token(tok: &str) -> u64 {
    let mut h = DefaultHasher::new();
    tok.hash(&mut h);
    h.finish()
}

/// Deterministic lexical embedder (see the module docs for its limits).
///
/// Use [`HashingEmbedder::new`] for the standard 384-dim width.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    /// Create a 384-dim lexical embedder.
    pub fn new() -> Self {
        Self { dim: EMBEDDING_DIM }
    }

    /// Create a lexical embedder with a custom width (tests / ablation).
    pub fn with_dim(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn backend(&self) -> &'static str {
        "hashing"
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for tok in tokenize(text) {
            let h = hash_token(&tok);
            let idx = (h % self.dim as u64) as usize;
            // Signed hashing keeps collisions from always reinforcing positively.
            let sign = if (h >> 63) & 1 == 1 { -1.0 } else { 1.0 };
            v[idx] += sign;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

/// Real semantic embedder: `all-MiniLM-L6-v2` (384-dim) via `fastembed-rs`
/// (ONNX runtime, Q4/A4). Always compiled in as of ④真语义.
///
/// Model resolution order:
/// 1. a local directory from [`LINGMIAO_EMBED_MODEL_DIR`](crate::embed)
///    (`brand::env("EMBED_MODEL_DIR")`);
/// 2. the conventional cache `$HOME/.cache/lingmiao-models/all-MiniLM-L6-v2-onnx`;
/// 3. otherwise `fastembed`'s own HuggingFace download.
///
/// A local directory must contain `model.onnx`, `tokenizer.json`, `config.json`,
/// `special_tokens_map.json` and `tokenizer_config.json`. Loading from local
/// bytes means the embedder works **offline** once the files are present.
pub struct FastEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

impl FastEmbedder {
    /// Load MiniLM-L6-v2, preferring local model files over a network download.
    ///
    /// Returns an error string (surfaced as an [`LingmiaoError`] by
    /// [`crate::default_embedder`]); there is no lexical fallback.
    pub fn new() -> Result<Self, String> {
        match resolve_model_dir() {
            Some(dir) => Self::from_dir(&dir),
            None => Self::from_hub(),
        }
    }

    /// Load from an explicit local model directory (offline).
    pub fn from_dir(dir: &std::path::Path) -> Result<Self, String> {
        use fastembed::{
            InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles,
            UserDefinedEmbeddingModel,
        };
        let read = |name: &str| {
            std::fs::read(dir.join(name))
                .map_err(|e| format!("read {name} in {}: {e}", dir.display()))
        };
        let files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let model =
            UserDefinedEmbeddingModel::new(read("model.onnx")?, files).with_pooling(Pooling::Mean);
        let te = TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::default())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            model: std::sync::Mutex::new(te),
        })
    }

    /// Download MiniLM-L6-v2 from HuggingFace via fastembed's own client.
    fn from_hub() -> Result<Self, String> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(false),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            model: std::sync::Mutex::new(model),
        })
    }
}

/// Resolve a local MiniLM model directory (env override, then default cache).
fn resolve_model_dir() -> Option<std::path::PathBuf> {
    // 需求④: the env override follows the brand prefix (`brand::env(...)`
    // derives `LINGMIAO_EMBED_MODEL_DIR`).
    let override_dir = std::env::var(lingmiao_core::brand::env("EMBED_MODEL_DIR")).ok();
    if let Some(dir) = override_dir {
        let p = std::path::PathBuf::from(dir);
        if p.join("model.onnx").is_file() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let p = std::path::PathBuf::from(&home).join(".cache/lingmiao-models/all-MiniLM-L6-v2-onnx");
    if p.join("model.onnx").is_file() {
        return Some(p);
    }
    None
}

impl Embedder for FastEmbedder {
    fn backend(&self) -> &'static str {
        "fastembed/all-MiniLM-L6-v2"
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut model = match self.model.lock() {
            Ok(m) => m,
            Err(_) => return vec![0.0; EMBEDDING_DIM],
        };
        match model.embed(vec![text.to_string()], None) {
            Ok(mut vecs) if !vecs.is_empty() => {
                let mut v = vecs.remove(0);
                v.resize(EMBEDDING_DIM, 0.0);
                v
            }
            _ => vec![0.0; EMBEDDING_DIM],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_preserves_values() {
        let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| i as f32 * 0.5).collect();
        let blob = encode_blob(&v);
        assert_eq!(blob.len(), EMBEDDING_DIM * 4);
        assert_eq!(decode_blob(&blob), v);
    }

    #[test]
    fn cosine_is_one_for_identical_unit_vectors() {
        let e = HashingEmbedder::new();
        let a = e.embed("the quick brown fox");
        let b = e.embed("the quick brown fox");
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn lexical_overlap_scores_higher_than_unrelated() {
        let e = HashingEmbedder::new();
        let a = e.embed("memory layer sqlite observations");
        let near = e.embed("sqlite observations store");
        let far = e.embed("banana helicopter tuesday");
        assert!(cosine(&a, &near) > cosine(&a, &far));
    }

    #[test]
    fn vectors_are_normalised_and_sized() {
        let e = HashingEmbedder::new();
        let v = e.embed("hello 世界 memory");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
    }

    #[test]
    fn cosine_with_empty_is_zero() {
        assert_eq!(cosine(&[], &[1.0, 2.0]), 0.0);
    }

    /// The real MiniLM embedder must load (when local model files are present)
    /// and rank a semantic neighbour above an unrelated sentence. Skips cleanly
    /// when the model is absent so a model-less checkout still passes.
    #[test]
    fn fastembed_ranks_semantic_neighbour_when_model_present() {
        let Some(dir) = resolve_model_dir() else {
            eprintln!("skip: no local MiniLM model dir (set LINGMIAO_EMBED_MODEL_DIR)");
            return;
        };
        let e = FastEmbedder::from_dir(&dir).expect("load FastEmbedder");
        let a = e.embed("the memory layer stores observations");
        let near = e.embed("observations are kept in the memory store");
        let far = e.embed("banana helicopter tuesday");
        assert_eq!(a.len(), EMBEDDING_DIM);
        assert!(cosine(&a, &near) > cosine(&a, &far));
    }
}
