//! 召回算法对照探针 —— 回答「除了 cos 相似度还有没有更好的办法」的真数据。
//!
//! 只用**只读副本库**（`cp .memory/*.db` 而来），绝不触碰活跃 `.memory/`。
//! 同一批语料（observations 层）、同一批查询，横向对照 5 种召回/排序策略：
//!
//! 1. **纯 cos**（现状语义路：`observations.search_semantic`）
//! 2. **mean-centered cos**（去中心化，治嵌入各向异性 —— MiniLM/e5 的通病）
//! 3. **BM25**（真关键词打分，替代现状 `LIKE` + 字段权重固定分）
//! 4. **RRF**（语义排名 ⊕ 关键词排名 融合，k=60；现状是「分数拼接」）
//! 5. **MMR**（最大边际相关，多样性重排；现状是正文指纹去重）
//!
//! 外加小样本测 **e5 前缀**（`query:`/`passage:`）增益。
//!
//! 跑法：`cargo run -p lingmiao-tools --example recall_algo_probe [库目录]`

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lingmiao_memory::embed::{cosine, decode_blob};
use lingmiao_memory::{Embedder, FastEmbedder};

const QUERIES: [&str; 6] = [
    "记忆合并",
    "编排框架",
    "层配额",
    "单二进制分发",
    "召回算法",
    "并发",
];

fn line(c: char) {
    println!("{}", c.to_string().repeat(78));
}

// ── 分词：ASCII 连续串 + CJK 单字 + 相邻 bigram（中文检索的关键） ──
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
    let mut out = atoms.clone();
    for p in atoms.windows(2) {
        out.push(format!("{}{}", p[0], p[1]));
    }
    out
}

// ── BM25（Okapi，k1=1.2 b=0.75） ─────────────────────────────
struct Bm25 {
    df: HashMap<String, usize>,
    tf: Vec<HashMap<String, usize>>,
    dl: Vec<usize>,
    avgdl: f32,
    n: usize,
}

impl Bm25 {
    fn new(docs: &[String]) -> Self {
        let n = docs.len();
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut tf = Vec::with_capacity(n);
        let mut dl = Vec::with_capacity(n);
        for d in docs {
            let toks = tokenize(d);
            dl.push(toks.len());
            let mut m: HashMap<String, usize> = HashMap::new();
            for t in toks {
                *m.entry(t).or_insert(0) += 1;
            }
            for t in m.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            tf.push(m);
        }
        let avgdl = if n == 0 {
            0.0
        } else {
            dl.iter().sum::<usize>() as f32 / n as f32
        };
        Self {
            df,
            tf,
            dl,
            avgdl,
            n,
        }
    }

    fn score(&self, q: &str, i: usize) -> f32 {
        const K1: f32 = 1.2;
        const B: f32 = 0.75;
        let qset: HashSet<String> = tokenize(q).into_iter().collect();
        let dl = self.dl[i] as f32;
        let mut s = 0.0f32;
        for t in &qset {
            let df = *self.df.get(t).unwrap_or(&0) as f32;
            if df == 0.0 {
                continue;
            }
            let idf = (1.0 + (self.n as f32 - df + 0.5) / (df + 0.5)).ln();
            let f = *self.tf[i].get(t).unwrap_or(&0) as f32;
            if f == 0.0 {
                continue;
            }
            s += idf * (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * dl / self.avgdl));
        }
        s
    }

    fn rank(&self, q: &str, top: usize) -> Vec<usize> {
        let mut v: Vec<(usize, f32)> = (0..self.n).map(|i| (i, self.score(q, i))).collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter()
            .filter(|x| x.1 > 0.0)
            .take(top)
            .map(|x| x.0)
            .collect()
    }
}

// ── RRF（Reciprocal Rank Fusion，Cormack 2009，k=60） ─────────
fn rrf_fuse(lists: &[&[usize]], k: f32) -> Vec<(usize, f32)> {
    let mut acc: HashMap<usize, f32> = HashMap::new();
    for l in lists {
        for (rank, &idx) in l.iter().enumerate() {
            *acc.entry(idx).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut v: Vec<(usize, f32)> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

// ── MMR（Maximal Marginal Relevance，Carbonell & Goldstein 1998） ──
fn mmr(qv: &[f32], embs: &[Vec<f32>], cands: &[usize], lambda: f32, k: usize) -> Vec<usize> {
    let mut selected: Vec<usize> = Vec::new();
    let mut remaining: Vec<usize> = cands.to_vec();
    while selected.len() < k && !remaining.is_empty() {
        let mut best = remaining[0];
        let mut best_v = f32::MIN;
        for &c in &remaining {
            let rel = cosine(&embs[c], qv);
            let div = if selected.is_empty() {
                0.0
            } else {
                selected
                    .iter()
                    .map(|&s| cosine(&embs[c], &embs[s]))
                    .fold(f32::MIN, f32::max)
            };
            let v = lambda * rel - (1.0 - lambda) * div;
            if v > best_v {
                best_v = v;
                best = c;
            }
        }
        selected.push(best);
        remaining.retain(|&x| x != best);
    }
    selected
}

struct Doc {
    #[allow(dead_code)]
    id: String,
    kind: String,
    name: String,
    text: String,
    emb: Vec<f32>,
}

fn load_obs(dir: &Path) -> Vec<Doc> {
    let db = dir.join("observations.db");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open observations.db read-only");
    let mut stmt = conn
        .prepare(
            "SELECT id,kind,name,content,embedding FROM observations WHERE embedding IS NOT NULL",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| {
            let blob: Vec<u8> = r.get("embedding")?;
            let name: String = r.get("name")?;
            let content: String = r.get("content")?;
            Ok(Doc {
                id: r.get("id")?,
                kind: r.get("kind")?,
                text: format!("{name} {content}"),
                name,
                emb: decode_blob(&blob),
            })
        })
        .expect("query_map");
    rows.filter_map(Result::ok).collect()
}

fn top_cos(embs: &[Vec<f32>], qv: &[f32], top: usize) -> Vec<usize> {
    let mut v: Vec<(usize, f32)> = embs
        .iter()
        .enumerate()
        .map(|(i, e)| (i, cosine(e, qv)))
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v.into_iter().take(top).map(|x| x.0).collect()
}

fn sub(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).map(|(x, y)| x - y).collect()
}

fn names(docs: &[Doc], idx: &[usize], k: usize) -> String {
    idx.iter()
        .take(k)
        .enumerate()
        .map(|(i, &d)| {
            format!(
                "{}.{}[{}]",
                i + 1,
                docs[d].name.chars().take(20).collect::<String>(),
                docs[d].kind
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".cache/lingmiao/tmp/recall_algo".to_string());
    let dir = PathBuf::from(dir);

    // 库里的向量是 all-MiniLM 编的；为保持同向量空间，主测用 MiniLM 重编查询。
    let home = std::env::var("HOME").unwrap_or_default();
    let minilm_dir = PathBuf::from(&home).join(".cache/lingmiao-models/all-MiniLM-L6-v2-onnx");
    let embedder = match FastEmbedder::from_dir(&minilm_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("加载 MiniLM 失败：{err}");
            std::process::exit(1);
        }
    };

    let docs = load_obs(&dir);
    let embs: Vec<Vec<f32>> = docs.iter().map(|d| d.emb.clone()).collect();
    let texts: Vec<String> = docs.iter().map(|d| d.text.clone()).collect();
    let bm25 = Bm25::new(&texts);

    line('=');
    println!(
        "语料 = observations {} 条（库 {}）",
        docs.len(),
        dir.display()
    );
    println!("查询 embedder = MiniLM（与库内向量同空间）");
    line('=');

    // 语料均值向量（去中心化用）
    let dim = embs.first().map(|v| v.len()).unwrap_or(0);
    let mut mean = vec![0.0f32; dim];
    for e in &embs {
        for (i, x) in e.iter().enumerate() {
            mean[i] += x;
        }
    }
    let n = embs.len().max(1) as f32;
    for m in mean.iter_mut() {
        *m /= n;
    }
    let mean_norm: f32 = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
    println!(
        "语料均值向量 ‖mean‖ = {mean_norm:.4}（各向异性指标：越接近平均向量长度，区分度越差）\n"
    );

    for q in QUERIES {
        let qv = embedder.embed(q);
        let qv_c = sub(&qv, &mean);
        let embs_c: Vec<Vec<f32>> = embs.iter().map(|e| sub(e, &mean)).collect();

        // 各算法排名
        let cos_rank = top_cos(&embs, &qv, 10);
        let cen_rank = top_cos(&embs_c, &qv_c, 10);
        let bm_rank = bm25.rank(q, 10);
        let fused = rrf_fuse(&[&cos_rank, &bm_rank], 60.0);
        let fused_idx: Vec<usize> = fused.iter().take(10).map(|x| x.0).collect();
        // MMR 在 cos top-30 池上做多样性重排
        let pool = top_cos(&embs, &qv, 30);
        let mmr_idx = mmr(&qv, &embs, &pool, 0.7, 10);

        // 区分度：top-1 与 top-10 的 cos 差
        let c1 = cosine(&embs[cos_rank[0]], &qv);
        let c10 = cosine(&embs[*cos_rank.get(9).unwrap_or(&cos_rank[0])], &qv);
        let cen1 = cosine(&embs_c[cen_rank[0]], &qv_c);
        let cen10 = cosine(&embs_c[*cen_rank.get(9).unwrap_or(&cen_rank[0])], &qv_c);

        line('=');
        println!("查询 {q:?}");
        println!(
            "  cos 区分度 gap(top1-top10) = {:.4}   |   去中心化 gap = {:.4}",
            c1 - c10,
            cen1 - cen10
        );
        println!("  【纯 cos】   {}", names(&docs, &cos_rank, 6));
        println!("  【去中心化】 {}", names(&docs, &cen_rank, 6));
        println!("  【BM25】     {}", names(&docs, &bm_rank, 6));
        println!("  【RRF融合】  {}", names(&docs, &fused_idx, 6));
        println!("  【MMR λ=.7】 {}", names(&docs, &mmr_idx, 6));

        // 多样性：top-10 内两两 cos ≥0.95 的对数（越少越好）
        let dup = |idx: &[usize]| -> usize {
            let mut c = 0;
            for i in 0..idx.len() {
                for j in (i + 1)..idx.len() {
                    if cosine(&embs[idx[i]], &embs[idx[j]]) >= 0.95 {
                        c += 1;
                    }
                }
            }
            c
        };
        println!(
            "  近似重复对(≥0.95, top10)：cos {} | 去中心化 {} | RRF {} | MMR {}",
            dup(&cos_rank),
            dup(&cen_rank),
            dup(&fused_idx),
            dup(&mmr_idx)
        );
        println!();
    }

    // ── e5 版对照（现场重编全量语料，看「换模型后 cos 是否还退化」） ──
    if std::env::args().any(|a| a == "--e5") {
        run_e5_section(&docs, &texts);
    }
}

/// 用生产模型（multilingual-e5-small-q）现场重编全量语料，重复同一套对照。
///
/// 关键问题：MiniLM 的短中文坍缩（`记忆合并`/`并发` 的 cos 全相同）在 e5 下是否
/// 消失？若消失，则「纯 cos 够不够」的答案取决于模型；若仍在，则融合/多样性重排
/// 才是结构性解药。
fn run_e5_section(docs: &[Doc], texts: &[String]) {
    line('=');
    println!(
        "【e5 版对照】生产模型 multilingual-e5-small-q 现场重编全量 {} 条",
        docs.len()
    );
    line('=');
    let home = std::env::var("HOME").unwrap_or_default();
    let e5_dir = PathBuf::from(&home).join(".cache/lingmiao-models/multilingual-e5-small-q-onnx");
    let e5 = match FastEmbedder::from_dir(&e5_dir) {
        Ok(e) => e,
        Err(e) => {
            println!("  跳过（加载失败）：{e}");
            return;
        }
    };
    let t0 = std::time::Instant::now();
    let plain: Vec<Vec<f32>> = texts.iter().map(|t| e5.embed(t)).collect();
    let pref: Vec<Vec<f32>> = texts
        .iter()
        .map(|t| e5.embed(&format!("passage: {t}")))
        .collect();
    println!(
        "  重编耗时 {:.1}s（plain + passage: 两套）\n",
        t0.elapsed().as_secs_f32()
    );

    let bm25 = Bm25::new(texts);
    for q in QUERIES {
        let qp = e5.embed(q);
        let qq = e5.embed(&format!("query: {q}"));
        let cp = top_cos(&plain, &qp, 10);
        let cw = top_cos(&pref, &qq, 10);
        let bm = bm25.rank(q, 10);
        let fused = rrf_fuse(&[&cw, &bm], 60.0);
        let fi: Vec<usize> = fused.iter().take(10).map(|x| x.0).collect();

        let gp = cosine(&plain[cp[0]], &qp) - cosine(&plain[*cp.last().unwrap()], &qp);
        let gw = cosine(&pref[cw[0]], &qq) - cosine(&pref[*cw.last().unwrap()], &qq);
        let dup = |embs: &[Vec<f32>], idx: &[usize]| -> usize {
            let mut c = 0;
            for i in 0..idx.len() {
                for j in (i + 1)..idx.len() {
                    if cosine(&embs[idx[i]], &embs[idx[j]]) >= 0.95 {
                        c += 1;
                    }
                }
            }
            c
        };
        println!("查询 {q:?}");
        println!(
            "  【e5 cos】   gap={gp:.4}  重复对={}  {}",
            dup(&plain, &cp),
            names(docs, &cp, 5)
        );
        println!(
            "  【e5 前缀】  gap={gw:.4}  重复对={}  {}",
            dup(&pref, &cw),
            names(docs, &cw, 5)
        );
        println!("  【BM25】     {}", names(docs, &bm, 5));
        println!(
            "  【RRF 融合 e5前缀+BM25】 重复对={}  {}",
            dup(&pref, &fi),
            names(docs, &fi, 5)
        );
        println!();
    }
}
