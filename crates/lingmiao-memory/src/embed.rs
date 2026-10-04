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
//! The production model is `intfloat/multilingual-e5-small` via [`FastEmbedder`]
//! (`fastembed-rs` / ONNX runtime). As of ④真语义 it is **mandatory**: the
//! `fastembed` dependency is unconditional and [`crate::default_embedder`] has
//! no lexical fallback — a load failure is a hard error.
//!
//! ## Single-binary distribution (cli 2026-10-01)
//!
//! The model weights are **embedded in this crate** (`assets/embed-model/`,
//! gzip-compressed, ~85 MB) and unpacked to the user cache on first use, so a
//! fresh install never downloads anything. The extract logic mirrors
//! `lingmiao-tools/src/ripgrep.rs`'s embedded `rg`. The unpack is skipped
//! entirely when the cache already holds the model, or when
//! `LINGMIAO_EMBED_MODEL_DIR` points at a complete model directory.
//!
//! The embedded build is the **int8-quantised** e5-small (`model.onnx` 118 MB
//! vs 470 MB fp32) — per-row cosine against fp32 is ≥ 0.9958, i.e. inside the
//! noise, for 1/4 the bytes. It is cached under the `-q` directory so it cannot
//! collide with a full-precision copy a user may already have.
//!
//! The model was switched from `all-MiniLM-L6-v2` (English-only; short Chinese
//! strings collapsed to identical vectors) to multilingual e5-small — both are
//! 384-dim, so the vector space width is unchanged and
//! [`Memory::rebuild_embeddings`](crate::Memory::rebuild_embeddings) migrates
//! existing rows on the first open (the recorded backend id differs).
//!
//! [`HashingEmbedder`] remains only as a *deterministic* embedder for tests
//! (and explicit non-semantic uses): it hashes lowercased tokens into a
//! normalised bag-of-words vector. It gives non-zero cosine similarity for token
//! overlap, which keeps the storage/search plumbing exercisable without loading
//! a 100 MB model, but it must never be the production embedder.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Embedding width — fixed at 384 by ADR A4 (both MiniLM-L6-v2 and the
/// current multilingual-e5-small output 384 dims, so the schema never changed).
pub const EMBEDDING_DIM: usize = 384;

/// A text → vector encoder. Implementations must be thread-safe (stores are
/// shared across the pipeline's parallel stages).
pub trait Embedder: Send + Sync {
    /// The dimensionality of produced vectors. Defaults to [`EMBEDDING_DIM`].
    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    /// Encode `text` into a vector of length [`Embedder::dim`].
    ///
    /// This is the **raw** encoder (no asymmetric prefix). Callers that know
    /// whether the text is a stored document or a search query should prefer
    /// [`Embedder::embed_document`] / [`Embedder::embed_query`] — see those for
    /// why the distinction matters.
    fn embed(&self, text: &str) -> Vec<f32>;

    /// Encode a **stored** document (an observation body, a KG node, a turn, a
    /// help topic). Defaults to [`Embedder::embed`].
    ///
    /// Asymmetric embedding models (intfloat e5) are trained with a
    /// `passage: ` / `query: ` prefix convention: the *same* sentence encodes
    /// differently on each side, and mixing the two spaces degrades recall.
    /// The default no-op keeps symmetric embedders (hashing, tests) correct.
    fn embed_document(&self, text: &str) -> Vec<f32> {
        self.embed(text)
    }

    /// Encode a **search query**. Defaults to [`Embedder::embed`].
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(text)
    }

    /// A short, stable backend identifier surfaced by the meta platform's
    /// `embed` view (需求⑤). Concrete embedders override it (`hashing` /
    /// or `fastembed/multilingual-e5-small-prefixed`); the default keeps
    /// third-party
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

/// Real semantic embedder: `intfloat/multilingual-e5-small` (384-dim) via
/// `fastembed-rs` (ONNX runtime, Q4/A4). Always compiled in as of ④真语义.
///
/// Model resolution order:
/// 1. a local directory from [`LINGMIAO_EMBED_MODEL_DIR`](crate::embed)
///    (`brand::env("EMBED_MODEL_DIR")`);
/// 2. the conventional cache `$HOME/.cache/lingmiao-models/multilingual-e5-small-q-onnx`,
///    **unpacked from the embedded archive** (`assets/embed-model/`) when absent;
/// 3. otherwise `fastembed`'s own HuggingFace download.
///
/// A local directory must contain `model.onnx`, `tokenizer.json`, `config.json`,
/// `special_tokens_map.json` and `tokenizer_config.json`. Loading from local
/// bytes means the embedder works **offline** once the files are present.
pub struct FastEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

impl FastEmbedder {
    /// Load multilingual e5-small, preferring local model files over a network
    /// download (and unpacking the embedded copy before ever going online).
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

    /// Download multilingual e5-small from HuggingFace via fastembed's own
    /// client (only reached when the embedded copy could not be unpacked).
    fn from_hub() -> Result<Self, String> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::MultilingualE5Small)
                .with_show_download_progress(false),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            model: std::sync::Mutex::new(model),
        })
    }
}

/// Resolve a local model directory: env override, then the default cache —
/// **unpacking the embedded archive into that cache** when it is absent.
fn resolve_model_dir() -> Option<std::path::PathBuf> {
    // 需求④: the env override follows the brand prefix (`brand::env(...)`
    // derives `LINGMIAO_EMBED_MODEL_DIR`). An explicit dir wins outright; it is
    // taken as-is when complete, and never overwritten by the embedded copy.
    let override_dir = std::env::var(lingmiao_core::brand::env("EMBED_MODEL_DIR")).ok();
    if let Some(dir) = override_dir {
        let p = std::path::PathBuf::from(dir);
        if p.join("model.onnx").is_file() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let p = std::path::PathBuf::from(&home).join(MODEL_CACHE_REL);
    if p.join("model.onnx").is_file() {
        return Some(p);
    }
    // 单二进制分发: nothing on disk yet — unpack the embedded model and reuse it
    // forever after. A failure here is not fatal: we fall through to `None`, and
    // `FastEmbedder::new` then downloads from the Hub (last resort).
    match unpack_embedded_model(&p) {
        Ok(()) => Some(p),
        Err(e) => {
            tracing::warn!("embed: could not unpack the embedded model: {e}");
            None
        }
    }
}

/// Cache location of the production model, relative to `$HOME`.
///
/// `-q` = the **int8-quantised** e5-small — the build embedded in the binary and
/// redistributed (118 MB vs 470 MB fp32). Its own directory keeps it from
/// colliding with a full-precision copy a user may already have cached.
const MODEL_CACHE_REL: &str = ".cache/lingmiao-models/multilingual-e5-small-q-onnx";

/// Backend identifier recorded in `store_meta` — the vector-space provenance.
///
/// Changing the model **or the encoding convention** requires changing this
/// string, or the first-open staleness check would not notice the vectors are
/// from another space. The `-prefixed` suffix marks the 2026-10-01 switch to
/// e5's `passage: ` / `query: ` convention ([`E5_PASSAGE_PREFIX`]): the model is
/// the same, but every stored vector is encoded differently, so a library
/// written without the prefixes must be re-embedded.
pub const EMBEDDER_BACKEND: &str = "fastembed/multilingual-e5-small-prefixed";

/// intfloat e5's **document-side** prefix.
///
/// e5 is trained with an asymmetric convention — every *stored* text is encoded
/// with `passage: ` and every *query* with `query: `. The model card states the
/// prefixes are required; without them the two sides drift into slightly
/// different regions and recall loses points (measured 2026-10-01 on the real
/// library: top-10 near-duplicate pairs 5 → 0 left in/out of `单二进制分发`).
///
/// Both sides must use the convention **and** the stored vectors must be
/// re-built under it: mixing a prefixed query against unprefixed stored vectors
/// is worse than using neither, so flipping these constants is a vector-space
/// change and [`EMBEDDER_BACKEND`] is versioned along with it (see the `-e5p`
/// suffix note there).
pub const E5_PASSAGE_PREFIX: &str = "passage: ";
/// intfloat e5's **query-side** prefix (see [`E5_PASSAGE_PREFIX`]).
pub const E5_QUERY_PREFIX: &str = "query: ";

/// The embedded model files: name + whether the compiled-in bytes are gzipped.
///
/// `model.onnx` and `tokenizer.json` are gzip-compressed (~81 MB / ~4 MB); the
/// three JSON files are small enough to embed verbatim.
const EMBEDDED_MODEL: &[EmbeddedFile] = &[
    EmbeddedFile {
        gz: include_bytes!("../assets/embed-model/model.onnx.gz"),
        name: "model.onnx",
        compressed: true,
    },
    EmbeddedFile {
        gz: include_bytes!("../assets/embed-model/tokenizer.json.gz"),
        name: "tokenizer.json",
        compressed: true,
    },
    EmbeddedFile {
        gz: include_bytes!("../assets/embed-model/config.json"),
        name: "config.json",
        compressed: false,
    },
    EmbeddedFile {
        gz: include_bytes!("../assets/embed-model/special_tokens_map.json"),
        name: "special_tokens_map.json",
        compressed: false,
    },
    EmbeddedFile {
        gz: include_bytes!("../assets/embed-model/tokenizer_config.json"),
        name: "tokenizer_config.json",
        compressed: false,
    },
];

/// One file carried inside the binary.
struct EmbeddedFile {
    /// Raw bytes as compiled in (`gz` = gzip-compressed when `compressed`).
    gz: &'static [u8],
    /// Name to write under the model directory.
    name: &'static str,
    /// Whether `gz` must be gunzipped before writing.
    compressed: bool,
}

/// Write every embedded file into `dir` (creating it), skipping a file that is
/// already present — so a populated cache costs five `stat`s and nothing else.
/// Each write is atomic (`*.tmp` + rename).
fn unpack_embedded_model(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for f in EMBEDDED_MODEL {
        let dest = dir.join(f.name);
        // A compressed member's inflated size is only known after decompressing,
        // so presence (non-empty) is the reuse signal; an uncompressed member is
        // also size-checked, which catches a truncated write.
        let already = match std::fs::metadata(&dest) {
            Ok(m) if f.compressed => m.len() > 0,
            Ok(m) => m.len() == f.gz.len() as u64,
            _ => false,
        };
        if already {
            continue;
        }
        let bytes = if f.compressed {
            gunzip(f.gz, f.name)?
        } else {
            f.gz.to_vec()
        };
        write_atomic(&dest, &bytes)?;
    }
    tracing::info!("embed: unpacked the embedded model into {}", dir.display());
    Ok(())
}

/// Gunzip a whole buffer in memory.
fn gunzip(data: &[u8], name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("gunzip {name}: {e}"))?;
    Ok(out)
}

/// Write `bytes` to `dest` via a unique temp file + atomic rename, so a crash
/// mid-write cannot leave a half model behind (same discipline as ripgrep.rs).
fn write_atomic(dest: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("install {}: {e}", dest.display())
    })
}

impl Embedder for FastEmbedder {
    fn backend(&self) -> &'static str {
        EMBEDDER_BACKEND
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        self.encode(text)
    }

    /// `passage: ` prefix — the intfloat e5 training convention for the stored
    /// side.
    fn embed_document(&self, text: &str) -> Vec<f32> {
        self.encode(&format!("{E5_PASSAGE_PREFIX}{text}"))
    }

    /// `query: ` prefix — the intfloat e5 training convention for the search
    /// side (measured 2026-10-01: near-duplicate pairs in the top-10 drop from
    /// 5 → 0 on `单二进制分发`).
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.encode(&format!("{E5_QUERY_PREFIX}{text}"))
    }
}

impl FastEmbedder {
    /// Shared raw encoder behind [`Embedder::embed`] and the two prefixed
    /// variants (the mutex + tokenizer path is identical for all three).
    fn encode(&self, text: &str) -> Vec<f32> {
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

    /// The real multilingual embedder must load (when local model files are
    /// present) and rank a semantic neighbour above an unrelated sentence.
    /// Skips cleanly when the model is absent so a model-less checkout still
    /// passes.
    #[test]
    fn fastembed_ranks_semantic_neighbour_when_model_present() {
        let Some(dir) = resolve_model_dir() else {
            eprintln!("skip: no local model dir (set LINGMIAO_EMBED_MODEL_DIR)");
            return;
        };
        let e = FastEmbedder::from_dir(&dir).expect("load FastEmbedder");
        let a = e.embed("the memory layer stores observations");
        let near = e.embed("observations are kept in the memory store");
        let far = e.embed("banana helicopter tuesday");
        assert_eq!(a.len(), EMBEDDING_DIM);
        assert!(cosine(&a, &near) > cosine(&a, &far));
    }

    /// 单二进制分发: the embedded archive must inflate to the exact model files,
    /// and a second call must be a no-op (the reuse path).
    #[test]
    fn embedded_archive_unpacks_and_is_reused() {
        let dir = std::env::temp_dir().join(format!("lingmiao-embed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unpack_embedded_model(&dir).expect("unpack");
        for f in EMBEDDED_MODEL {
            let n = std::fs::metadata(dir.join(f.name))
                .unwrap_or_else(|e| panic!("{} missing: {e}", f.name))
                .len();
            if f.compressed {
                assert!(n > 0, "{} is empty", f.name);
            } else {
                assert_eq!(n, f.gz.len() as u64, "{} size", f.name);
            }
        }
        // The ONNX weights must inflate to their real (large) size, not a stub.
        let onnx = std::fs::metadata(dir.join("model.onnx")).unwrap().len();
        assert!(
            onnx > 100_000_000,
            "model.onnx looks truncated: {onnx} bytes"
        );

        // Reuse: a second call writes nothing (mtimes unchanged).
        let mtime = |n: &str| std::fs::metadata(dir.join(n)).unwrap().modified().unwrap();
        let before = [mtime("model.onnx"), mtime("tokenizer.json")];
        unpack_embedded_model(&dir).expect("re-unpack");
        let after = [mtime("model.onnx"), mtime("tokenizer.json")];
        assert_eq!(before, after, "an existing model must not be rewritten");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The recorded backend id is the vector-space guard: it must name the real
    /// model **and the encoding convention** — a library written before the e5
    /// prefix switch carries the unprefixed id, so the suffix is what triggers
    /// its one-time rebuild.
    #[test]
    fn backend_id_names_the_multilingual_model() {
        assert_eq!(EMBEDDER_BACKEND, "fastembed/multilingual-e5-small-prefixed");
        assert!(MODEL_CACHE_REL.ends_with("multilingual-e5-small-q-onnx"));
    }

    /// The e5 prefixes are asymmetric and applied on the right side: documents
    /// get `passage: `, queries get `query: ` (the model card's requirement).
    /// A symmetric embedder must stay a no-op so nothing else changes.
    #[test]
    fn e5_prefixes_are_asymmetric_and_hashing_ignores_them() {
        assert_eq!(E5_PASSAGE_PREFIX, "passage: ");
        assert_eq!(E5_QUERY_PREFIX, "query: ");
        // HashingEmbedder keeps the trait defaults (no prefix) — its document
        // and query vectors of the same text are identical.
        let h = HashingEmbedder::new();
        assert_eq!(h.embed_document("hello"), h.embed("hello"));
        assert_eq!(h.embed_query("hello"), h.embed("hello"));
    }
}
