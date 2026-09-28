//! M2.1 probe: load the real ONNX MiniLM-L6-v2 embedder and show that
//! semantically-related text scores higher than unrelated text.
//!
//! Run with: `cargo run -p lingmiao-memory --example embed_probe`
//! (first run downloads the model weights via fastembed/hf-hub).

use lingmiao_memory::embed::cosine;
use lingmiao_memory::{EMBEDDING_DIM, Embedder, FastEmbedder};

fn main() {
    let e = match FastEmbedder::new() {
        Ok(e) => e,
        Err(err) => {
            eprintln!("fastembed model load FAILED: {err}");
            std::process::exit(1);
        }
    };
    println!("loaded all-MiniLM-L6-v2 (dim={})", e.dim());

    let a = e.embed("the memory layer stores observations");
    let near = e.embed("observations are kept in the memory store");
    let far = e.embed("banana helicopter tuesday");
    assert_eq!(a.len(), EMBEDDING_DIM);

    let s_near = cosine(&a, &near);
    let s_far = cosine(&a, &far);
    println!("cos(query, related)   = {s_near:.4}");
    println!("cos(query, unrelated) = {s_far:.4}");
    assert!(
        s_near > s_far,
        "semantic neighbour should outscore unrelated text"
    );
    println!("OK: real semantic embedding verified");
}
