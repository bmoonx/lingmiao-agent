//! `meta` tool group — 需求⑤ 元认知平台（M6）.
//!
//! The platform is **a catalogue plus per-mechanism drill-down**, not a
//! document: the model asks a mechanism a question and the tool answers with
//! the *runtime truth* read from this process (live store row counts, the
//! actual embedder backend, the resolved config source, the real stage graph),
//! never a hand-copied value from `docs/*.md`.
//!
//! Three read-only tools, all registered in **every** stage whitelist (like
//! `help`):
//!
//! | tool         | args                          | answer                          |
//! |--------------|-------------------------------|---------------------------------|
//! | `meta_list`  | —                             | the mechanism catalogue (§3)    |
//! | `meta_show`  | `mechanism`, optional `view`  | one mechanism's drilled-down view |
//! | `meta_state` | —                             | live platform snapshot          |
//!
//! Design (see `docs/meta-platform.md`):
//! * **Catalogue + generic `show`** — adding a mechanism is one catalogue row
//!   plus one `render` arm, so the tool count stays constant.
//! * **Runtime source of truth** — `meta_*` reads the running process; the
//!   docs explain *why*, the code is the root.
//! * Reuses the Q6 [`Tool`] trait; no new abstraction.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use lingmiao_core::brand;
use lingmiao_core::config::{Config, STAGE_B_CONTEXT, STAGE_C_DIALOG, STAGE_CONSOLIDATE};
use lingmiao_core::events::EVENT_VARIANTS;
use lingmiao_core::paths::Paths;
use lingmiao_memory::{
    EMBEDDING_DIM, FIELD_WEIGHT_CONTENT, FIELD_WEIGHT_NAME, FIELD_WEIGHT_TOPIC, Memory,
    TableSchema, Zone, read_zone_counts,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};
use crate::{
    business_tools, computer_use, file_tools, help_tool, memory_tools, verify_tools, whiteboard,
};

/// Tools this module contributes.
pub const TOOL_NAMES: [&str; 3] = ["meta_list", "meta_show", "meta_state"];

/// One catalogue entry (需求⑤ §3): the index row every mechanism publishes.
struct Mechanism {
    /// Stable id used by `meta_show`.
    id: &'static str,
    /// Human name.
    name: &'static str,
    /// One-liner: what the mechanism does.
    one_liner: &'static str,
    /// Where the truth lives (code locations).
    location: &'static str,
    /// Drill-down views; the first is the default for `meta_show`.
    views: &'static [&'static str],
}

/// The mechanism catalogue — a code constant kept beside the mechanisms it
/// indexes, reviewed in the same change as any mechanism edit (需求⑤ §3).
const CATALOG: [Mechanism; 9] = [
    Mechanism {
        id: "search",
        name: "记忆检索",
        one_liner: "混合召回 → 排序 → 去重 → top-N",
        location: "lingmiao-tools/src/memory_tools.rs, lingmiao-memory/src/observations.rs",
        views: &["candidates", "ranking", "dedup"],
    },
    Mechanism {
        id: "embed",
        name: "嵌入",
        one_liner: "文本 → 384 维向量，全库共享一个 embedder",
        location: "lingmiao-memory/src/embed.rs, lingmiao-memory/src/lib.rs",
        views: &["backend", "dim", "coverage"],
    },
    Mechanism {
        id: "zones",
        name: "三区隔离",
        one_liner: "chat / main / auditor 三套独立 store",
        location: "lingmiao-memory/src/store.rs",
        views: &["layout", "counts"],
    },
    Mechanism {
        id: "store",
        name: "store schema",
        one_liner: "四库表结构（obs / KG / archive / business）",
        location: "lingmiao-memory/src/{observations,knowledge,archive,business}.rs",
        views: &["schema"],
    },
    Mechanism {
        id: "pipeline",
        name: "管线 stage 图",
        one_liner: "组织上下文 → 工作阶段 → 沉淀阶段",
        location: "lingmiao-engine/src/engine.rs, lingmiao-core/assets/stages.json",
        views: &["graph", "stages"],
    },
    Mechanism {
        id: "whitelist",
        name: "工具白名单",
        one_liner: "每 stage 允许调用的工具集",
        location: "lingmiao-core/assets/stages.json",
        views: &["by_stage"],
    },
    Mechanism {
        id: "config",
        name: "配置解析链",
        one_liner: "内嵌 → 外部覆盖 → 最终值",
        location: "lingmiao-core/src/config.rs, lingmiao-core/src/brand.rs",
        views: &["chain", "source", "values"],
    },
    Mechanism {
        id: "events",
        name: "事件总线",
        one_liner: "单 enum Event + broadcast",
        location: "lingmiao-core/src/events.rs",
        views: &["variants"],
    },
    Mechanism {
        id: "tools",
        name: "工具注册表",
        one_liner: "分组 + 名称 + canonical schema",
        location: "lingmiao-tools/src/lib.rs",
        views: &["groups"],
    },
];

/// Shared state for the three tools: the live memory bundle (chat zone), the
/// resolved config, and the project root.
struct MetaTools {
    memory: Arc<Memory>,
    config: Config,
    root: PathBuf,
}

impl MetaTools {
    fn memory_dir(&self) -> PathBuf {
        Paths::at(&self.root).memory_dir
    }

    /// The union of every statically-known tool name, by group. Used by the
    /// `whitelist` view to flag a whitelisted-but-unregistered ("dangling")
    /// tool. `mcp` tools are dynamic and therefore not enumerable here.
    fn known_tool_names() -> BTreeSet<&'static str> {
        let mut set = BTreeSet::new();
        set.extend(file_tools::TOOL_NAMES);
        set.extend(whiteboard::TOOL_NAMES);
        set.extend(memory_tools::TOOL_NAMES);
        set.extend(business_tools::TOOL_NAMES);
        set.extend(help_tool::TOOL_NAMES);
        set.extend(computer_use::TOOL_NAMES);
        set.extend(verify_tools::TOOL_NAMES);
        set.extend(TOOL_NAMES);
        set
    }

    /// Render one mechanism's view. Returns the JSON/text payload.
    fn render(&self, mechanism: &str, view: &str) -> Result<String, ToolError> {
        let v = match (mechanism, view) {
            ("search", "candidates") => json!({
                "observations_semantic_recall": memory_tools::RECALL_OBS_SEMANTIC,
                "observations_keyword_recall": memory_tools::RECALL_OBS_KEYWORD,
                "knowledge_semantic_recall": memory_tools::RECALL_KNOWLEDGE_SEMANTIC,
                "archive_keyword_recall": memory_tools::RECALL_ARCHIVE_KEYWORD,
                "note": "search_memory 汇总各层召回后合并、去重、按分排序取 top-N",
            }),
            ("search", "ranking") => json!({
                "observations_keyword": {
                    "field_weights": {
                        "name": FIELD_WEIGHT_NAME,
                        "topic": FIELD_WEIGHT_TOPIC,
                        "content": FIELD_WEIGHT_CONTENT,
                    },
                    "recency_bonus": true,
                    "fn": "lingmiao-memory/src/observations.rs::rank_by_relevance",
                },
                "observations_semantic": "cosine over 384-dim vectors",
                "keyword_only_score": memory_tools::KEYWORD_SCORE,
                "note": "语义命中用真实 cosine；关键词命中记固定分；权重与 rank_by_relevance 同源（observations.rs 常量）",
            }),
            ("search", "dedup") => json!({
                "key": "(layer, id)",
                "policy": "重复保留最高分（语义与关键词两路都命中时）",
                "snippet_chars": memory_tools::SNIPPET_CHARS,
            }),
            ("embed", "backend") => json!({
                "backend": self.memory.embedder_backend().unwrap_or("none"),
                "note": "fastembed/all-MiniLM-L6-v2 = 生产真语义嵌入（④真语义起无词法回退）；hashing 仅用于确定性测试",
                "source": "lingmiao-memory/src/lib.rs::default_embedder",
            }),
            ("embed", "dim") => json!({
                "dim": EMBEDDING_DIM,
                "space": "observations 与 knowledge 共享同一向量空间",
                "source": "lingmiao-memory/src/embed.rs::EMBEDDING_DIM (ADR A4)",
            }),
            ("embed", "coverage") => self.embed_coverage()?,
            ("zones", "layout") => {
                let mem = self.memory_dir();
                json!({
                    "chat": mem.display().to_string(),
                    "main": mem.join("main").display().to_string(),
                    "auditor": mem.join("auditor").display().to_string(),
                    "source": "lingmiao-memory/src/store.rs::Zone",
                })
            }
            ("zones", "counts") => self.zone_counts()?,
            ("store", "schema") => self.store_schema()?,
            ("pipeline", "graph") => json!({
                "graph": [STAGE_B_CONTEXT, STAGE_C_DIALOG, STAGE_CONSOLIDATE],
                "source": "lingmiao-engine/src/engine.rs::run_turn（组织上下文→工作阶段→沉淀阶段，共三段；知识图谱更新纯算法，已并入沉淀阶段）",
            }),
            ("pipeline", "stages") => {
                let mut stages = serde_json::Map::new();
                for (name, cfg) in self.config.stages() {
                    stages.insert(
                        name.clone(),
                        json!({"timeout": cfg.timeout, "tool_count": cfg.tools.len()}),
                    );
                }
                json!({"stages": stages, "source": "lingmiao-core/assets/stages.json (Config::stages)"})
            }
            ("whitelist", "by_stage") => self.whitelist(),
            ("config", "chain") => json!({
                "chain": [
                    format!("{}_CONFIG_DIR", brand::ENV_PREFIX),
                    "外部同名 JSON（外部覆盖）",
                    "内嵌 include_str! 默认",
                ],
                "source": "lingmiao-core/src/config.rs::discover/read_json",
            }),
            ("config", "source") => json!({
                "env_var": brand::env("CONFIG_DIR"),
                "override_dir": self
                    .config
                    .override_dir()
                    .map(|p| p.display().to_string()),
                "effective": if self.config.override_dir().is_some() {
                    "external override"
                } else {
                    "embedded defaults"
                },
                "pipeline_complete": self.config.has_pipeline(),
            }),
            ("config", "values") => json!({
                "help_topics": self.config.help_topics().len(),
                "stages": self.config.stages().len(),
                "prompts": self.config.prompts().len(),
                "mcp_servers": self.config.mcp_servers().len(),
                "core_lock_max_len": self.config.core_lock_max_len(),
                "search_strategy_injected": !self.config.search_strategy().is_empty(),
            }),
            ("events", "variants") => {
                let variants: Vec<Value> = EVENT_VARIANTS
                    .iter()
                    .map(|(variant, wire)| json!({"variant": variant, "event_type": wire}))
                    .collect();
                json!({
                    "count": variants.len(),
                    "variants": variants,
                    "source": "lingmiao-core/src/events.rs::Event (Q9 合并后 12 变体)",
                })
            }
            ("tools", "groups") => json!({
                "file_tools": file_tools::TOOL_NAMES,
                "whiteboard": whiteboard::TOOL_NAMES,
                "memory": memory_tools::TOOL_NAMES,
                "business": business_tools::TOOL_NAMES,
                "help": help_tool::TOOL_NAMES,
                "meta": TOOL_NAMES,
                "verify": verify_tools::TOOL_NAMES,
                "computer_use": computer_use::TOOL_NAMES,
                "mcp": "动态：由 mcp.json 声明的每个 server 适配而来",
                "note": "computer_use 组受 LINGMIAO_COMPUTER_USE 门控",
            }),
            _ => {
                return Err(ToolError::invalid(
                    "meta_show",
                    format!("unknown mechanism/view `{mechanism}/{view}`"),
                ));
            }
        };
        Ok(to_pretty(&v))
    }

    /// `embed`/`coverage` — embedding backfill status per store (runtime).
    fn embed_coverage(&self) -> Result<Value, ToolError> {
        let obs_total = self
            .memory
            .observations
            .stats()
            .map_err(|e| tool_mem("meta_show", e))?;
        let obs_missing = self
            .memory
            .observations
            .missing_embeddings()
            .map_err(|e| tool_mem("meta_show", e))?;
        let kg = self
            .memory
            .knowledge
            .stats()
            .map_err(|e| tool_mem("meta_show", e))?;
        let kg_missing = self
            .memory
            .knowledge
            .missing_embeddings()
            .map_err(|e| tool_mem("meta_show", e))?;
        let arc_total = self
            .memory
            .archive
            .count()
            .map_err(|e| tool_mem("meta_show", e))?;
        let arc_missing = self
            .memory
            .archive
            .missing_embeddings()
            .map_err(|e| tool_mem("meta_show", e))?;
        Ok(json!({
            "observations": {"rows": obs_total, "missing_embedding": obs_missing},
            "knowledge_nodes": {"rows": kg.nodes, "missing_embedding": kg_missing},
            "archive_turns": {"rows": arc_total, "missing_embedding": arc_missing},
            "note": "missing_embedding = embedding IS NULL 的行数（待回填）",
        }))
    }

    /// `zones`/`counts` — per-zone row counts. The chat zone is read live from
    /// the open [`Memory`]; the sibling role-loop zones are probed read-only and
    /// reported `null` when they have never run.
    fn zone_counts(&self) -> Result<Value, ToolError> {
        let mem_dir = self.memory_dir();
        let chat = json!({
            "observations": self.memory.observations.stats().map_err(|e| tool_mem("meta_show", e))?,
            "knowledge_nodes": self.memory.knowledge.stats().map_err(|e| tool_mem("meta_show", e))?.nodes,
            "knowledge_edges": self.memory.knowledge.stats().map_err(|e| tool_mem("meta_show", e))?.edges,
            "archive_turns": self.memory.archive.count().map_err(|e| tool_mem("meta_show", e))?,
            "business_tables": self.memory.business.tables().map_err(|e| tool_mem("meta_show", e))?.len(),
        });
        let zone_json = |z: Zone| match read_zone_counts(&mem_dir, z) {
            Some(c) => json!({
                "observations": c.observations,
                "knowledge_nodes": c.knowledge_nodes,
                "knowledge_edges": c.knowledge_edges,
                "archive_turns": c.archive_turns,
                "business_tables": c.business_tables,
            }),
            None => Value::Null,
        };
        Ok(json!({
            "chat": chat,
            "main": zone_json(Zone::Main),
            "auditor": zone_json(Zone::Auditor),
            "source": "chat = live Memory；main/auditor = read_zone_counts 只读探测",
            "note": "main/auditor 为角色循环区，目录不存在时为 null",
        }))
    }

    /// `whitelist`/`by_stage` — each stage's real tool list plus any dangling
    /// (whitelisted-but-unregistered) names.
    fn whitelist(&self) -> Value {
        let known = Self::known_tool_names();
        let mut out = serde_json::Map::new();
        for (stage, cfg) in self.config.stages() {
            let dangling: Vec<&String> = cfg
                .tools
                .iter()
                .filter(|t| !known.contains(t.as_str()))
                .collect();
            out.insert(
                stage.clone(),
                json!({
                    "tools": cfg.tools,
                    "count": cfg.tools.len(),
                    "dangling": dangling,
                }),
            );
        }
        json!({
            "by_stage": out,
            "note": "dangling = 白名单列了但未在静态注册表找到一个匹配名（mcp 动态工具不计）",
        })
    }

    /// `store`/`schema` — each store's schema, read live from the SQLite catalog
    /// (`PRAGMA table_info` + `sqlite_master`), never a hand-copied column list.
    fn store_schema(&self) -> Result<Value, ToolError> {
        let obs = self
            .memory
            .observations
            .schema()
            .map_err(|e| tool_mem("meta_show", e))?;
        let kg = self
            .memory
            .knowledge
            .schema()
            .map_err(|e| tool_mem("meta_show", e))?;
        let arc = self
            .memory
            .archive
            .schema()
            .map_err(|e| tool_mem("meta_show", e))?;
        let biz = self
            .memory
            .business
            .tables()
            .map_err(|e| tool_mem("meta_show", e))?;
        Ok(json!({
            "observations": schema_to_json(obs),
            "knowledge": schema_to_json(kg),
            "archive": schema_to_json(arc),
            "business": {
                "tables": biz,
                "note": "自由 schema：由 agent 通过 business_db_execute 自建表",
            },
            "source": "运行时 PRAGMA table_info / sqlite_master 实读，非文档抄写",
        }))
    }
}

/// Render a list of runtime table schemas as a `{table: {columns, indexes}}` map.
fn schema_to_json(tables: Vec<TableSchema>) -> Value {
    let mut map = serde_json::Map::new();
    for t in tables {
        let cols: Vec<Value> = t
            .columns
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "type": c.ty,
                    "not_null": c.not_null,
                    "pk": c.pk,
                })
            })
            .collect();
        map.insert(t.name, json!({"columns": cols, "indexes": t.indexes}));
    }
    Value::Object(map)
}

/// Pretty-print a JSON value.
fn to_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn tool_mem(tool: &str, e: lingmiao_core::LingmiaoError) -> ToolError {
    ToolError::Other(format!("{tool}: {e}"))
}

// ── tools ──────────────────────────────────────────────────────

/// No-argument tools (`meta_list` / `meta_state`).
#[derive(Deserialize, schemars::JsonSchema)]
struct NoArgs {}

/// `meta_show` arguments.
#[derive(Deserialize, schemars::JsonSchema)]
struct ShowArgs {
    /// Mechanism id (see `meta_list`), e.g. `search` / `embed` / `zones` / `events`.
    mechanism: String,
    /// View within the mechanism; omit for the mechanism's default view.
    #[serde(default)]
    view: String,
}

/// `meta_list` — the mechanism catalogue.
struct MetaListTool {
    m: Arc<MetaTools>,
}

#[async_trait]
impl Tool for MetaListTool {
    fn name(&self) -> &str {
        "meta_list"
    }

    fn description(&self) -> &str {
        "列出灵妙「元认知平台」的机制目录：每个机制（检索/嵌入/三区/schema/管线/白名单/配置/事件/工具表）\
         的 id、名称、一句话、代码位置与可下钻视图。需要了解自身机制时先调 meta_list，再用 meta_show 下钻，\
         不要凭记忆猜测。"
    }

    fn parameters(&self) -> Value {
        json_schema::<NoArgs>()
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        let _ = &self.m; // catalogue is static; the handle keeps registration uniform
        let mut out = format!(
            "灵妙元认知平台 · 机制目录（{}）\n\
             （工具读运行时真源，文档只讲动机；先 list 再 show）\n",
            CATALOG.len()
        );
        for mech in &CATALOG {
            out.push_str(&format!(
                "\n- [{}] {} — {}\n    位置：{}\n    视图：{}",
                mech.id,
                mech.name,
                mech.one_liner,
                mech.location,
                mech.views.join(" / ")
            ));
        }
        out.push_str(
            "\n\n用 meta_show 下钻：mechanism=<id> [view=<视图>]；meta_state 给平台实时快照。",
        );
        Ok(ToolOutput::ok(out))
    }
}

/// `meta_show` — drill into one mechanism.
struct MetaShowTool {
    m: Arc<MetaTools>,
}

#[async_trait]
impl Tool for MetaShowTool {
    fn name(&self) -> &str {
        "meta_show"
    }

    fn description(&self) -> &str {
        "下钻「元认知平台」的某个机制，返回该机制的运行时真值（如真实召回深度、当前嵌入后端、\
         各 store 行数、stage 图、白名单实况、配置来源、事件变体、工具分组）。\
         先 meta_list 拿机制 id；view 省略时给默认视图。"
    }

    fn parameters(&self) -> Value {
        json_schema::<ShowArgs>()
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let a: ShowArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::invalid("meta_show", e.to_string()))?;
        let id = a.mechanism.trim().to_ascii_lowercase();
        let Some(mech) = CATALOG.iter().find(|m| m.id == id) else {
            let ids: Vec<&str> = CATALOG.iter().map(|m| m.id).collect();
            return Ok(ToolOutput::ok(format!(
                "未知机制 `{}`。可用机制：{}。",
                a.mechanism,
                ids.join(" / ")
            )));
        };
        let view = if a.view.trim().is_empty() {
            mech.views.first().copied().unwrap_or("")
        } else {
            a.view.trim()
        };
        if !mech.views.contains(&view) {
            return Err(ToolError::invalid(
                "meta_show",
                format!(
                    "机制 `{}` 无视图 `{view}`；可用视图：{}",
                    mech.id,
                    mech.views.join(" / ")
                ),
            ));
        }
        let body = self.m.render(mech.id, view)?;
        Ok(ToolOutput::ok(format!(
            "[{} · {}] {}\n{}",
            mech.id, view, mech.one_liner, body
        )))
    }
}

/// `meta_state` — live platform snapshot.
struct MetaStateTool {
    m: Arc<MetaTools>,
}

#[async_trait]
impl Tool for MetaStateTool {
    fn name(&self) -> &str {
        "meta_state"
    }

    fn description(&self) -> &str {
        "灵妙元认知平台的实时快照：内存各 store 行数、嵌入后端/维度、配置来源、stage 数、\
         事件变体数、工具分组与已知工具数。需要一眼看清自身当前运行态时调用。"
    }

    fn parameters(&self) -> Value {
        json_schema::<NoArgs>()
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, ToolError> {
        let m = &self.m;
        let mem_dir = m.memory_dir();
        let value = json!({
            "root": m.root.display().to_string(),
            "memory_dir": mem_dir.display().to_string(),
            "config_source": m
                .config
                .override_dir()
                .map(|p| format!("external:{}", p.display()))
                .unwrap_or_else(|| "embedded".to_string()),
            "pipeline_complete": m.config.has_pipeline(),
            "embed": {
                "backend": m.memory.embedder_backend().unwrap_or("none"),
                "dim": EMBEDDING_DIM,
            },
            "stores": {
                "observations": m.memory.observations.stats().map_err(|e| tool_mem("meta_state", e))?,
                "knowledge_nodes": m.memory.knowledge.stats().map_err(|e| tool_mem("meta_state", e))?.nodes,
                "knowledge_edges": m.memory.knowledge.stats().map_err(|e| tool_mem("meta_state", e))?.edges,
                "archive_turns": m.memory.archive.count().map_err(|e| tool_mem("meta_state", e))?,
                "business_tables": m.memory.business.tables().map_err(|e| tool_mem("meta_state", e))?.len(),
            },
            "stages": m.config.stages().len(),
            "prompts": m.config.prompts().len(),
            "event_variants": EVENT_VARIANTS.len(),
            "known_tools": MetaTools::known_tool_names().len(),
        });
        Ok(ToolOutput::ok(to_pretty(&value)))
    }
}

/// Register the `meta` group against one shared [`Memory`] handle, the resolved
/// config and the project root.
pub fn register(registry: &mut ToolRegistry, memory: Arc<Memory>, cfg: &Config, root: &Path) {
    let shared = Arc::new(MetaTools {
        memory,
        config: cfg.clone(),
        root: root.to_path_buf(),
    });
    registry.register(MetaListTool { m: shared.clone() });
    registry.register(MetaShowTool { m: shared.clone() });
    registry.register(MetaStateTool { m: shared });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingmiao_memory::HashingEmbedder;

    fn setup(tag: &str) -> (Arc<MetaTools>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lingmiao-meta-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mem = Memory::open_in_dir(&dir, Zone::Chat, Some(Arc::new(HashingEmbedder::new())))
            .expect("open memory");
        let tools = Arc::new(MetaTools {
            memory: Arc::new(mem),
            config: Config::load_default().expect("config"),
            root: dir.clone(),
        });
        (tools, dir)
    }

    fn registry(tools: Arc<MetaTools>) -> ToolRegistry {
        let reg = ToolRegistry::new();
        reg.register(MetaListTool { m: tools.clone() });
        reg.register(MetaShowTool { m: tools.clone() });
        reg.register(MetaStateTool { m: tools });
        reg
    }

    #[test]
    fn catalog_ids_are_unique_and_have_views() {
        let ids: BTreeSet<&str> = CATALOG.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), CATALOG.len(), "catalogue ids must be unique");
        assert_eq!(CATALOG.len(), 9);
        for m in &CATALOG {
            assert!(!m.views.is_empty(), "{} has no views", m.id);
            assert!(!m.location.is_empty(), "{} has no location", m.id);
        }
    }

    #[tokio::test]
    async fn meta_list_enumerates_every_mechanism() {
        let (tools, dir) = setup("list");
        let reg = registry(tools);
        let out = reg.execute("meta_list", json!({})).await.unwrap();
        assert!(!out.is_error);
        for m in &CATALOG {
            assert!(out.content.contains(m.id), "list missing {}", m.id);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn meta_show_serves_runtime_views() {
        let (tools, dir) = setup("show");
        let reg = registry(tools);
        // Default view of `events` → variants table (static true value).
        let ev = reg
            .execute("meta_show", json!({"mechanism": "events"}))
            .await
            .unwrap();
        assert!(ev.content.contains("summary_reported"), "{}", ev.content);
        // 12 since `context_handoff` joined the enum (the A→B handoff notice,
        // 2026-09-30) — the `source` label was updated with it and this
        // assertion was missed, leaving the workspace red.
        assert!(ev.content.contains("\"count\": 12"), "{}", ev.content);
        // Runtime view: embed backend reflects the opened store's embedder.
        let em = reg
            .execute(
                "meta_show",
                json!({"mechanism": "embed", "view": "backend"}),
            )
            .await
            .unwrap();
        assert!(em.content.contains("hashing"), "{}", em.content);
        // Store counts come from the live memory (empty here).
        let st = reg.execute("meta_state", json!({})).await.unwrap();
        assert!(st.content.contains("\"observations\": 0"), "{}", st.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn meta_show_rejects_unknown_mechanism_and_view() {
        let (tools, dir) = setup("bad");
        let reg = registry(tools);
        let missing = reg
            .execute("meta_show", json!({"mechanism": "nope"}))
            .await
            .unwrap();
        assert!(missing.content.contains("未知机制"));
        let bad_view = reg
            .execute("meta_show", json!({"mechanism": "search", "view": "zzz"}))
            .await
            .unwrap_err();
        assert!(matches!(bad_view, ToolError::InvalidArgs { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn whitelist_view_flags_dangling_names() {
        let (tools, dir) = setup("wl");
        let reg = registry(tools);
        let out = reg
            .execute("meta_show", json!({"mechanism": "whitelist"}))
            .await
            .unwrap();
        // The embedded stages.json is fully bound against the known groups.
        assert!(out.content.contains("组织上下文"), "{}", out.content);
        assert!(out.content.contains("\"dangling\": []"), "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_catalog_view_is_renderable() {
        // A dropped match arm would surface here rather than at runtime.
        let (tools, dir) = setup("render");
        for m in &CATALOG {
            for view in m.views {
                tools
                    .render(m.id, view)
                    .unwrap_or_else(|e| panic!("{}/{} not renderable: {e}", m.id, view));
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pipeline_graph_lists_every_run_turn_stage() {
        // Lockstep: the graph view must name **every** stage
        // `engine.rs::run_turn` actually runs — 组织上下文 → 工作阶段 → 沉淀阶段
        // (2026-09-28 rename; the algorithmic MG update is folded into 沉淀阶段).
        let (tools, dir) = setup("graph");
        let body = tools.render("pipeline", "graph").expect("render graph");
        for stage in [STAGE_B_CONTEXT, STAGE_C_DIALOG, STAGE_CONSOLIDATE] {
            assert!(
                body.contains(stage),
                "graph missing stage `{stage}`: {body}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn store_schema_is_read_at_runtime() {
        let (tools, dir) = setup("schema");
        let reg = registry(tools);
        let out = reg
            .execute("meta_show", json!({"mechanism": "store"}))
            .await
            .unwrap();
        // The view is introspected from the live catalog, so it names the real
        // tables/columns and advertises its runtime source.
        assert!(out.content.contains("\"observations\""), "{}", out.content);
        assert!(out.content.contains("\"nodes\""), "{}", out.content);
        assert!(out.content.contains("\"edges\""), "{}", out.content);
        assert!(out.content.contains("\"turns\""), "{}", out.content);
        assert!(out.content.contains("\"embedding\""), "{}", out.content);
        assert!(out.content.contains("运行时"), "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_ranking_reports_the_ranker_weights() {
        // meta reports the *same* consts `rank_by_relevance` consumes (single
        // source of truth) — a doc-copied number would fail this lockstep.
        let (tools, dir) = setup("ranking");
        let reg = registry(tools);
        let out = reg
            .execute(
                "meta_show",
                json!({"mechanism": "search", "view": "ranking"}),
            )
            .await
            .unwrap();
        for (k, w) in [
            ("name", FIELD_WEIGHT_NAME),
            ("topic", FIELD_WEIGHT_TOPIC),
            ("content", FIELD_WEIGHT_CONTENT),
        ] {
            assert!(
                out.content.contains(&format!("\"{k}\": {w}")),
                "missing weight {k}={w}: {}",
                out.content
            );
        }
        assert!(
            out.content.contains("observations.rs 常量"),
            "source not labelled: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
