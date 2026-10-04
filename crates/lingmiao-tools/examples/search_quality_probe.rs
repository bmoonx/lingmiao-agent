//! 召回质量**数据探针** —— 为「层配额 + 同层近似去重 + 短中文语义退化」
//! 三项改造取真实基线数据（只读副本，绝不触碰活跃 `.memory/`）。
//!
//! 输出四块：
//! 1. **语义分分布**：每个查询 top-15 的 cos 值与名称（退化有多严重）
//! 2. **区分度**：原始 vs 去中心化（mean-centering）后的 max/中位/gap
//! 3. **同层近似重复**：top-30 内两两 cos ≥ 0.95 / 0.99 的对数
//! 4. **层配额现状**：`search_memory` 合并后 top-N 的层分布
//!
//! 跑法：`cargo run -p lingmiao-tools --example search_quality_probe [库目录]`

use std::sync::Arc;

use lingmiao_memory::embed::{cosine, decode_blob};
use lingmiao_memory::{Embedder, FastEmbedder, Memory, Zone};
use lingmiao_tools::{ToolRegistry, memory_tools};

/// Queries used by cli 2026-09-30 when the degradation was first observed.
const QUERIES: [&str; 5] = ["记忆合并", "编排框架", "你好", "并发", "层配额"];

fn line(c: char) {
    println!("{}", c.to_string().repeat(78));
}

fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x - y).collect()
}

struct Row {
    id: String,
    name: String,
    kind: String,
    emb: Vec<f32>,
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".cache/lingmiao/tmp/recall_probe_memory".to_string());

    let embedder: Arc<dyn Embedder> = match FastEmbedder::new() {
        Ok(e) => Arc::new(e),
        Err(err) => {
            eprintln!("fastembed 加载失败：{err}");
            std::process::exit(1);
        }
    };
    println!("库目录 = {dir}");
    println!("embedder = {}", embedder.backend());

    let mem = match Memory::open_in_dir(
        std::path::Path::new(&dir),
        Zone::Chat,
        Some(embedder.clone()),
    ) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            eprintln!("打开库失败：{e}");
            std::process::exit(1);
        }
    };

    // ── 直接用只读 sqlite 连接读全量 embedding（search_* 不返回向量） ──
    let db = std::path::Path::new(&dir).join("observations.db");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open observations.db read-only");
    let all: Vec<Row> = {
        let mut stmt = conn
            .prepare("SELECT id,name,kind,embedding FROM observations WHERE embedding IS NOT NULL")
            .expect("prepare");
        stmt.query_map([], |r| {
            let blob: Vec<u8> = r.get("embedding")?;
            Ok(Row {
                id: r.get::<_, String>("id")?,
                name: r.get::<_, String>("name")?,
                kind: r.get::<_, String>("kind")?,
                emb: decode_blob(&blob),
            })
        })
        .expect("query_map")
        .filter_map(Result::ok)
        .collect()
    };
    println!(
        "带 embedding 的观测 = {}（维度 {}）",
        all.len(),
        all.first().map(|r| r.emb.len()).unwrap_or(0)
    );

    // 语料均值向量（去中心化用）。
    let dim = all.first().map(|r| r.emb.len()).unwrap_or(0);
    let mut mean = vec![0.0f32; dim];
    for r in &all {
        for (i, x) in r.emb.iter().enumerate().take(dim) {
            mean[i] += x;
        }
    }
    let m = all.len().max(1) as f32;
    for v in mean.iter_mut() {
        *v /= m;
    }
    let norm: f32 = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
    println!("语料均值向量 ‖mean‖ = {norm:.4}（MiniLM 各向异性指标，越大越退化成「全都很像」）");

    for q in QUERIES {
        let qv = embedder.embed(q);
        let qv_c = sub(&qv, &mean);

        let mut scored: Vec<(f32, f32, &Row)> = all
            .iter()
            .map(|r| {
                let raw = cosine(&r.emb, &qv);
                let centered = cosine(&sub(&r.emb, &mean), &qv_c);
                (raw, centered, r)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        line('=');
        println!("查询 {q:?}");
        line('=');
        println!("【1】原始 cos top-15");
        for (i, (raw, cen, r)) in scored.iter().take(15).enumerate() {
            println!(
                "  {:>2}. cos={:.4} (cen={:.4})  {:<14} {:<10} {}",
                i + 1,
                raw,
                cen,
                r.id,
                r.kind,
                r.name.chars().take(28).collect::<String>()
            );
        }
        let max = scored.first().map(|s| s.0).unwrap_or(0.0);
        let med = scored.get(scored.len() / 2).map(|s| s.0).unwrap_or(0.0);
        let p10 = scored.get(scored.len() / 10).map(|s| s.0).unwrap_or(0.0);
        println!(
            "  → 原始：max={max:.4} p10={p10:.4} 中位={med:.4} gap(max-p10)={:.4}",
            max - p10
        );

        let mut cs: Vec<f32> = scored.iter().map(|s| s.1).collect();
        cs.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let cmax = cs.first().copied().unwrap_or(0.0);
        let cmed = cs.get(cs.len() / 2).copied().unwrap_or(0.0);
        let cp10 = cs.get(cs.len() / 10).copied().unwrap_or(0.0);
        println!(
            "  → 去中心化：max={cmax:.4} p10={cp10:.4} 中位={cmed:.4} gap(max-p10)={:.4}",
            cmax - cp10
        );

        // 【3】同层近似重复：top-30 两两 cos
        let top: Vec<&Row> = scored.iter().take(30).map(|s| s.2).collect();
        let (mut p95, mut p99) = (0usize, 0usize);
        let mut example = String::new();
        for i in 0..top.len() {
            for j in (i + 1)..top.len() {
                let c = cosine(&top[i].emb, &top[j].emb);
                if c >= 0.95 {
                    p95 += 1;
                    if c >= 0.99 {
                        p99 += 1;
                    }
                    if example.is_empty() {
                        example = format!(
                            "{} ({}) ↔ {} ({}) cos={:.4}",
                            top[i].id, top[i].name, top[j].id, top[j].name, c
                        );
                    }
                }
            }
        }
        println!("【3】top-30 内两两 cos：≥0.95 共 {p95} 对，≥0.99 共 {p99} 对");
        if !example.is_empty() {
            println!("     例：{example}");
        }
    }

    // ── 【4】各层单独候选数（语义 / 关键词） ───────────────────
    line('=');
    println!("【4】各层**单独**召回候选数（合并前）");
    line('=');
    for q in QUERIES {
        let o_sem = mem
            .observations
            .search_semantic(q, 30)
            .map(|v| v.len())
            .unwrap_or(0);
        let o_kw = mem
            .observations
            .search(q, 10, "")
            .map(|v| v.len())
            .unwrap_or(0);
        let k_sem = mem
            .knowledge
            .search_semantic(q, 30)
            .map(|v| v.len())
            .unwrap_or(0);
        let k_kw = mem
            .knowledge
            .search_keyword(q, 10, "")
            .map(|v| v.len())
            .unwrap_or(0);
        let a_sem = mem
            .archive
            .search_semantic(q, 30)
            .map(|v| v.len())
            .unwrap_or(0);
        let a_kw = mem.archive.search(q, 10).map(|v| v.len()).unwrap_or(0);
        println!(
            "  查询 {q:?}: obs {o_sem}sem/{o_kw}kw | kg {k_sem}sem/{k_kw}kw | arc {a_sem}sem/{a_kw}kw"
        );
    }

    // ── 【4】层配额现状 ────────────────────────────────────────
    line('=');
    println!("【4】search_memory 合并后的层分布（无 LLM 精排，纯分数序）");
    line('=');
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut reg = ToolRegistry::new();
    memory_tools::register(&mut reg, mem.clone(), None);
    for q in QUERIES {
        let out = rt
            .block_on(reg.execute(
                "search_memory",
                serde_json::json!({"keyword": q, "limit": 10}),
            ))
            .expect("search_memory");
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap_or_default();
        let mut dist: std::collections::BTreeMap<String, usize> = Default::default();
        if let Some(rows) = v.get("results").and_then(|r| r.as_array()) {
            for r in rows {
                *dist
                    .entry(
                        r.get("layer")
                            .and_then(|x| x.as_str())
                            .unwrap_or("?")
                            .to_string(),
                    )
                    .or_default() += 1;
            }
        }
        // 同层近似重复检查：top-10 内正文前缀是否有重复
        let mut bodies: Vec<String> = Vec::new();
        if let Some(rows) = v.get("results").and_then(|r| r.as_array()) {
            for r in rows {
                bodies.push(
                    r.get("snippet")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(24)
                        .collect(),
                );
            }
        }
        let uniq: std::collections::BTreeSet<&String> = bodies.iter().collect();
        println!(
            "  查询 {q:?} → top-10 层分布 {dist:?} | 正文前缀唯一 {}/{}",
            uniq.len(),
            bodies.len()
        );
        for b in &bodies {
            println!("      · {}", b.replace('\n', " "));
        }
    }
}
