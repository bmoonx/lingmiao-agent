//! 探针：验证 `store::open_db` 施加的 `PRAGMA mmap_size` 在**真实库**上的收益。
//!
//! 只读打开一份副本 `context_record.db`（绝不触碰活跃 `.memory/`），对
//! archive 的语义召回底层查询计时两遍：一遍走生产 opener（已开 mmap），一遍
//! 显式把 mmap 关掉复刻旧行为。
//!
//! 跑法：`cargo run --release -p lingmiao-memory --example mmap_probe <副本目录>`

use std::time::Instant;

use lingmiao_memory::store::{MMAP_SIZE_BYTES, open_db};

const QUERY: &str = "SELECT * FROM turns WHERE embedding IS NOT NULL";

fn bench(conn: &rusqlite::Connection, label: &str) -> f64 {
    let mut best = f64::MAX;
    let mut rows = 0usize;
    for _ in 0..3 {
        let t = Instant::now();
        let mut stmt = conn.prepare(QUERY).expect("prepare");
        rows = stmt.query_map([], |_| Ok(())).expect("query").count();
        best = best.min(t.elapsed().as_secs_f64());
    }
    let map: i64 = conn
        .query_row("PRAGMA mmap_size", [], |r| r.get(0))
        .expect("pragma");
    println!("{label}: {best:.3}s  (rows={rows}, mmap_size={map})");
    best
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".cache/lingmiao/tmp/b_exp_c".to_string());
    let path = std::path::Path::new(&dir).join("context_record.db");
    println!("db = {}", path.display());

    // 生产 opener：mmap 已开。
    let conn = open_db(&path).expect("open via production opener");
    let with_mmap = bench(&conn, "生产 opener（mmap 开）");

    // 旧行为复刻：显式关掉 mmap。
    conn.execute_batch("PRAGMA mmap_size=0;").expect("pragma");
    let without = bench(&conn, "mmap 关（旧行为）");

    println!(
        "→ mmap 加速 {:.1}x（MMAP_SIZE_BYTES={}）",
        without / with_mmap,
        MMAP_SIZE_BYTES
    );
}
