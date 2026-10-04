//! 多语言嵌入模型对照探针（cli 2026-10-01「有没有各种语言都可以的嵌入模型」）。
//!
//! 在**同一进程**里并排加载两个 ONNX 模型，对同一批短串做真实编码，输出
//! 两两 cosine —— 用来判定「短中文坍缩」是否只属于 all-MiniLM-L6-v2。
//!
//! 跑法：`cargo run -p lingmiao-tools --example multilingual_probe`
//!
//! 模型目录（本地、离线，均由 hf-mirror 预拉）：
//! - `~/.cache/lingmiao-models/all-MiniLM-L6-v2-onnx`（现役，英文 BERT 词表）
//! - `~/.cache/lingmiao-models/multilingual-e5-small-onnx`（候选，XLM-R 词表）

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use lingmiao_memory::{Embedder, FastEmbedder};

const MINILM_DIR: &str = "/root/.cache/lingmiao-models/all-MiniLM-L6-v2-onnx";
const E5_DIR: &str = "/root/.cache/lingmiao-models/multilingual-e5-small-onnx";

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn encode(e: &dyn Embedder, texts: &[&str]) -> Vec<Vec<f32>> {
    texts.iter().map(|t| e.embed(t)).collect()
}

fn line(c: char) {
    println!("{}", c.to_string().repeat(72));
}

/// 同一批「短中文近义/异义」串：MiniLM 把 OOV 单字全塌到 UNK，应当两两 1.0。
fn report_pairwise(name: &str, e: &dyn Embedder, texts: &[&str]) {
    line('=');
    println!("{name}   backend = {}", e.backend());
    line('-');
    let vs = encode(e, texts);
    println!("  cos 矩阵（乱序配对，越低越有分辨力）:");
    for i in 0..texts.len() {
        let mut row = String::new();
        for j in 0..texts.len() {
            row.push_str(&format!("{:>7.4}", cosine(&vs[i], &vs[j])));
        }
        println!("    {:<10} {row}", texts[i]);
    }
    // 关键对：异义串的 max cos（塌陷 = 1.0）
    let mut worst_off_diag: f32 = 0.0;
    let mut min_off_diag: f32 = 1.0;
    for i in 0..texts.len() {
        for j in 0..texts.len() {
            if i != j {
                let c = cosine(&vs[i], &vs[j]);
                worst_off_diag = worst_off_diag.max(c);
                min_off_diag = min_off_diag.min(c);
            }
        }
    }
    println!(
        "  → 异串 cos：max {worst_off_diag:.4} / min {min_off_diag:.4}   （max 越接近 1.0 越坍缩）"
    );
}

/// 跨语句语义：同义句 vs 异义句 —— 贴近真实检索场景。
fn report_sentence_pairs(name: &str, e: &dyn Embedder, pairs: &[(&str, &str, bool)]) {
    line('-');
    println!("{name}  句对（same = 应当近，diff = 应当远）:");
    let texts: Vec<&str> = pairs.iter().flat_map(|(a, b, _)| [*a, *b]).collect();
    let vs = encode(e, &texts);
    for (k, (a, b, want_same)) in pairs.iter().enumerate() {
        let c = cosine(&vs[2 * k], &vs[2 * k + 1]);
        let tag = if *want_same { "same" } else { "diff" };
        let verdict = if *want_same {
            if c > 0.75 { "OK" } else { "WEAK" }
        } else if c < 0.75 {
            "OK"
        } else {
            "COLLAPSED"
        };
        println!("    [{tag}] {c:.4} {verdict:<9} {a}  |  {b}");
    }
}

/// 同语言内的一致性：中文近义 vs 中英对照（跨语言同义）。
fn report_cross_lingual(name: &str, e: &dyn Embedder, groups: &[(&str, &[&str])]) {
    line('-');
    println!("{name}  跨语言同义（组内应互相靠近）:");
    let mut vs: HashMap<&str, Vec<f32>> = HashMap::new();
    for (_, items) in groups {
        for t in *items {
            vs.insert(t, e.embed(t));
        }
    }
    for (label, items) in groups {
        let mut best = 0.0f32;
        let mut worst = 1.0f32;
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let c = cosine(&vs[items[i]], &vs[items[j]]);
                best = best.max(c);
                worst = worst.min(c);
            }
        }
        println!(
            "    {label:<12} min {worst:.4} / max {best:.4}   组内 {} 串",
            items.len()
        );
    }
}

fn main() {
    let minilm: Arc<dyn Embedder> = match FastEmbedder::from_dir(Path::new(MINILM_DIR)) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("MiniLM 加载失败：{e}");
            std::process::exit(1);
        }
    };
    let e5: Arc<dyn Embedder> = match FastEmbedder::from_dir(Path::new(E5_DIR)) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("e5 加载失败：{e}");
            std::process::exit(1);
        }
    };
    println!("MiniLM backend = {}", minilm.backend());
    println!("e5     backend = {}", e5.backend());

    // ① 短中文串（历史实测：MiniLM 两两 cos = 1.0）
    let short_zh = ["你好", "并发", "编排", "框架", "层配额", "记忆合并"];
    report_pairwise("① MiniLM", minilm.as_ref(), &short_zh);
    report_pairwise("① multilingual-e5-small", e5.as_ref(), &short_zh);

    // ② 跨语句语义
    let pairs: [(&str, &str, bool); 5] = [
        ("怎么修记忆召回", "记忆检索坏了怎么修", true),
        ("怎么修记忆召回", "今天天气不错", false),
        ("并发控制的实现", "并行执行怎么保证安全", true),
        ("并发控制的实现", "蛋糕配方", false),
        ("短中文向量坍缩", "中文 embedding 没有区分度", true),
    ];
    report_sentence_pairs("② MiniLM", minilm.as_ref(), &pairs);
    report_sentence_pairs("② multilingual-e5-small", e5.as_ref(), &pairs);

    // ③ 跨语言同义
    let groups: [(&str, &[&str]); 3] = [
        (
            "中/英/日 同义",
            &["记忆检索", "memory retrieval", "記憶検索"],
        ),
        ("中/英 编程", &["并发控制", "concurrency control"]),
        ("中/英 无关参照", &["蛋糕配方", "xylophone"]),
    ];
    report_cross_lingual("③ MiniLM", minilm.as_ref(), &groups);
    report_cross_lingual("③ multilingual-e5-small", e5.as_ref(), &groups);

    line('=');
    println!(
        "维度：MiniLM {} 维 / e5-small {} 维（应与现役 384 相同 → 无需改 schema）",
        minilm.embed("x").len(),
        e5.embed("x").len()
    );
}
