//! 嵌入模型「体积 vs 质量」对照探针（cli 2026-10-01「好用的模型参数量多大？对二进制影响多大」）。
//!
//! 三个候选并排在同一进程里加载，对同一批串真实编码：
//! - all-MiniLM-L6-v2（现役，英文 BERT 词表，fp32）
//! - multilingual-e5-small（候选，XLM-R 词表，fp32）
//! - multilingual-e5-small 的 int8 量化版（Xenova/model_quantized.onnx）
//!
//! 目的：判定 int8 量化能否把 448MiB 压到 ~113MiB 而不显著掉质量。
//!
//! 跑法：`cargo run -p lingmiao-tools --example embed_size_probe`

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use lingmiao_memory::{Embedder, FastEmbedder};

const MINILM_DIR: &str = "/root/.cache/lingmiao-models/all-MiniLM-L6-v2-onnx";
const E5_FP32_DIR: &str = "/root/.cache/lingmiao-models/multilingual-e5-small-onnx";
const E5_INT8_DIR: &str = "/root/.cache/lingmiao-models/multilingual-e5-small-q-onnx";

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

fn load(name: &str, dir: &str) -> Option<Arc<dyn Embedder>> {
    match FastEmbedder::from_dir(Path::new(dir)) {
        Ok(e) => Some(Arc::new(e)),
        Err(e) => {
            eprintln!("{name} 加载失败（跳过）：{e}");
            None
        }
    }
}

fn short_zh_report(name: &str, e: &dyn Embedder, texts: &[&str]) {
    let vs: Vec<Vec<f32>> = texts.iter().map(|t| e.embed(t)).collect();
    let mut worst: f32 = 0.0;
    let mut best_pair = (String::new(), String::new(), 0.0f32);
    for i in 0..texts.len() {
        for j in (i + 1)..texts.len() {
            let c = cosine(&vs[i], &vs[j]);
            if c > worst {
                worst = c;
                best_pair = (texts[i].to_string(), texts[j].to_string(), c);
            }
        }
    }
    println!(
        "    {name:<26} dim={:<4} 异串 cos max {worst:.4}  （最像的一对：{} / {}）",
        vs[0].len(),
        best_pair.0,
        best_pair.1
    );
}

fn sentence_report(name: &str, e: &dyn Embedder, pairs: &[(&str, &str, bool)]) {
    println!("    {name}:");
    for (a, b, want_same) in pairs {
        let c = cosine(&e.embed(a), &e.embed(b));
        let ok = if *want_same { c > 0.75 } else { c < 0.75 };
        println!(
            "      [{}] {c:.4} {}  {a}  |  {b}",
            if *want_same { "same" } else { "diff" },
            if ok { "OK  " } else { "MISS" }
        );
    }
}

fn cross_lingual_report(name: &str, e: &dyn Embedder, groups: &[(&str, &[&str])]) {
    println!("    {name}:");
    for (label, items) in groups {
        let mut vs: HashMap<&str, Vec<f32>> = HashMap::new();
        for t in *items {
            vs.insert(t, e.embed(t));
        }
        let mut lo = 1.0f32;
        let mut hi = 0.0f32;
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let c = cosine(&vs[items[i]], &vs[items[j]]);
                lo = lo.min(c);
                hi = hi.max(c);
            }
        }
        println!("      {label:<14} min {lo:.4} / max {hi:.4}");
    }
}

fn main() {
    let minilm = load("MiniLM", MINILM_DIR);
    let e5f = load("e5-fp32", E5_FP32_DIR);
    let e5q = load("e5-int8", E5_INT8_DIR);

    let short_zh = ["你好", "并发", "编排", "框架", "层配额", "记忆合并"];
    let pairs: [(&str, &str, bool); 5] = [
        ("怎么修记忆召回", "记忆检索坏了怎么修", true),
        ("怎么修记忆召回", "今天天气不错", false),
        ("并发控制的实现", "并行执行怎么保证安全", true),
        ("并发控制的实现", "蛋糕配方", false),
        ("短中文向量坍缩", "中文 embedding 没有区分度", true),
    ];
    let groups: [(&str, &[&str]); 3] = [
        (
            "中/英/日 同义",
            &["记忆检索", "memory retrieval", "記憶検索"],
        ),
        ("中/英 编程", &["并发控制", "concurrency control"]),
        ("中/英 无关参照", &["蛋糕配方", "xylophone"]),
    ];

    println!("== ① 短中文串分辨力（max 越接近 1.0 越坍缩）==");
    for (n, e) in [("MiniLM", &minilm), ("e5-fp32", &e5f), ("e5-int8", &e5q)] {
        if let Some(e) = e {
            short_zh_report(n, e.as_ref(), &short_zh);
        }
    }

    println!("== ② 句对语义（same 应近 / diff 应远）==");
    for (n, e) in [("MiniLM", &minilm), ("e5-fp32", &e5f), ("e5-int8", &e5q)] {
        if let Some(e) = e {
            sentence_report(n, e.as_ref(), &pairs);
        }
    }

    println!("== ③ 跨语言同义（组内应互相靠近）==");
    for (n, e) in [("MiniLM", &minilm), ("e5-fp32", &e5f), ("e5-int8", &e5q)] {
        if let Some(e) = e {
            cross_lingual_report(n, e.as_ref(), &groups);
        }
    }

    // fp32 vs int8：同一批串逐条比 cos（衡量量化损失）
    if let (Some(a), Some(b)) = (&e5f, &e5q) {
        println!("== ④ int8 量化损失（fp32 与 int8 对同一串的 cos，越接近 1.0 损失越小）==");
        let all: Vec<&str> = short_zh
            .iter()
            .copied()
            .chain(pairs.iter().flat_map(|(a, b, _)| [*a, *b]))
            .chain(groups.iter().flat_map(|(_, xs)| xs.iter().copied()))
            .collect();
        let mut lo = 1.0f32;
        let mut hi = 0.0f32;
        for t in &all {
            let c = cosine(&a.embed(t), &b.embed(t));
            lo = lo.min(c);
            hi = hi.max(c);
        }
        println!(
            "    {all_len} 条串：逐条 cos min {lo:.4} / max {hi:.4}",
            all_len = all.len()
        );
    }
}
