//! # lingmiao-engine — QL orchestrator
//!
//! M1 ships the single-turn streaming engine ([`Engine::turn`]); M4 grows it
//! into the full pipeline.
//!
//! Decision Q7: each stage is an `async fn` over a strongly-typed [`TurnContext`],
//! with the consolidation stage run under `Result` error boundaries; the Python
//! `StageAgent` is ported as a reusable module ([`stage_agent`]).
//! Q9 (2026-09-16): 对话观测/要点记录/上下文审计/内容质检/任务追踪 merged into the
//! single **沉淀阶段** stage — the pipeline is `组织上下文 → 工作阶段 → 沉淀阶段`
//! ([`Engine::run_turn`]), and the four 1:1 result events collapse into
//! `summary_reported`.

#![forbid(unsafe_code)]

pub mod context;
pub mod engine;
pub mod rerank;
pub mod stage_agent;

pub use context::{SummaryReport, TurnContext};
pub use engine::{Engine, STAGE_B, STAGE_C, STAGE_SUMMARY, empty_usage, fill_prompt};
pub use rerank::LlmReranker;
pub use stage_agent::{StageAgent, StageOutcome, extract_json};
