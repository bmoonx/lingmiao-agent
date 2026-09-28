//! # lingmiao-tools — the tool layer (M3)
//!
//! Decision Q6: a single [`Tool`] abstraction (name + description +
//! `schemars`-derived JSON Schema + async execute) behind a [`ToolRegistry`],
//! plus the builtin groups and an MCP client:
//!
//! | module            | group          | tools                                          |
//! |-------------------|----------------|------------------------------------------------|
//! | [`file_tools`]    | `file_tools`   | read_file / write_file / edit / glob / …       |
//! | [`file_guard`]    | *(guard)*      | path sandbox + read-before-write + stale check |
//! | [`diff`]          | *(view)*       | unified diff of a file mutation (CC `structuredPatch`) |
//! | [`whiteboard`]    | `whiteboard`   | new_page / read / append / update / clear / …  |
//! | [`memory_tools`]  | `memory`       | search_* / memory_stats / update_memory / …    |
//! | [`business_tools`]| `business`     | business_db_query / execute / schema           |
//! | [`meta_tools`]    | `meta`         | meta_list / meta_show / meta_state *(需求⑤)*   |
//! | [`computer_use`]  | `computer_use` | screenshot / click / type / key / … *(gated)*  |
//! | [`mcp`]           | `mcp`          | remote MCP servers adapted into local tools    |
//!
//! [`ToolRegistry::schemas`] emits the **OpenAI canonical function-calling
//! tools array** so the LLM layer (Q5) can consume the registry directly, and
//! [`ToolRegistry::schemas_for`] narrows it to a stage whitelist.
//!
//! The `parliament` group is gone with the mode layer (Q2).

#![forbid(unsafe_code)]

pub mod business_tools;
pub mod computer_use;
pub mod diff;
pub mod file_guard;
pub mod file_tools;
pub mod help_tool;
pub mod mcp;
pub mod memory_tools;
pub mod meta_tools;
pub mod ripgrep;
pub mod tool;
pub mod verify_tools;
pub mod whiteboard;

use std::path::PathBuf;
use std::sync::Arc;

use lingmiao_core::brand;
use lingmiao_core::config::Config;
use lingmiao_memory::Memory;

pub use business_tools::{BusinessExecuteTool, BusinessQueryTool, BusinessSchemaTool};
pub use computer_use::{ComputerUseTool, ENV_SWITCH as COMPUTER_USE_ENV};
pub use file_guard::{FileGuard, GuardedWrite};
pub use file_tools::FileTools;
pub use help_tool::HelpTool;
pub use mcp::{McpSource, McpToolSpec, RemoteTool};
pub use memory_tools::{
    ListArchiveTool, ListObservationsTool, MemoryKindsTool, MemoryStatsTool, RerankCandidate,
    Reranker, SearchArchiveTool, SearchExternalMemoryTool, SearchKnowledgeTool, SearchMemoryTool,
    SearchObservationsTool, UpdateKnowledgeTool, UpdateMemoryTool,
};
pub use tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};
pub use verify_tools::{Assertion, VerificationReport, parse_acceptance, run_assertions};
pub use whiteboard::{WhiteboardStore, WhiteboardTool};

/// Build the default tool registry for a project rooted at `root`.
///
/// Registers the `file_tools` and `whiteboard` groups always; the
/// `computer_use` group only when [`computer_use::enabled`] is true (the
/// `LINGMIAO_COMPUTER_USE=1` switch), so no pointer/keyboard action can happen
/// by accident. Memory + business tools need a [`Memory`] handle — see
/// [`full_registry`].
pub fn default_registry(root: impl Into<PathBuf>) -> ToolRegistry {
    let root = root.into();
    let mut registry = ToolRegistry::new();
    let guard = Arc::new(FileGuard::new(&root));

    FileTools::new(guard).register_all(&mut registry);
    whiteboard::register(&mut registry, &root);
    // Lexical-only here; `full_registry` re-registers it with the process
    // embedder to enable help RAG (see `help_tool`).
    help_tool::register(&mut registry, None);
    computer_use::register_if_enabled(
        &mut registry,
        root.join(brand::CACHE_DIR)
            .join(brand::BIN)
            .join("tmp")
            .join("computer_use"),
    );

    registry
}

/// Build the full registry: [`default_registry`] plus the memory, business and
/// meta (需求⑤) tool groups (M4 — closes the `stages.json` binding gap).
///
/// The `meta` group needs the resolved [`Config`] and project root so its
/// drill-down views report the *running* config (source, stages, overrides)
/// rather than re-discovering defaults. `reranker` is the optional LLM
/// re-ranker wired into `search_memory` (原版对齐) — `None` disables it.
pub fn full_registry(
    root: impl Into<PathBuf>,
    memory: Arc<Memory>,
    cfg: &Config,
    reranker: Option<Arc<dyn Reranker>>,
) -> ToolRegistry {
    let root = root.into();
    let mut registry = default_registry(&root);
    // Help RAG: upgrade the `help` slot registered by `default_registry` (lexical
    // only) to the process embedder so unknown/natural-language queries fall back
    // to vector retrieval over the topic bodies.
    help_tool::register(&mut registry, memory.embedder());
    memory_tools::register(&mut registry, memory.clone(), reranker);
    business_tools::register(&mut registry, memory.clone());
    meta_tools::register(&mut registry, memory, cfg, &root);
    // `verify` group (原版 verifier.py): the pure-code acceptance oracle. Needs
    // only the project root — no memory handle — but rides the full registry so
    // it is present wherever the memory/business groups are.
    verify_tools::register(&mut registry, &root);
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lingmiao-tools-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_registry_has_file_and_whiteboard_groups_only() {
        let reg = default_registry(temp_root("default"));
        // file group
        for n in file_tools::TOOL_NAMES {
            assert!(reg.contains(n), "missing file tool {n}");
        }
        // whiteboard group
        for n in whiteboard::TOOL_NAMES {
            assert!(reg.contains(n), "missing whiteboard tool {n}");
        }
        // computer-use group must be absent by default (env switch off)
        for n in computer_use::TOOL_NAMES {
            assert!(
                !reg.contains(n),
                "computer-use tool {n} registered by default"
            );
        }
        // help is always present (需求② §6.2 — available in every stage).
        assert!(reg.contains("help"), "help tool must always be registered");
        assert_eq!(
            reg.len(),
            file_tools::TOOL_NAMES.len()
                + whiteboard::TOOL_NAMES.len()
                + help_tool::TOOL_NAMES.len()
        );
    }

    #[test]
    fn schemas_name_all_four_groups() {
        // Force computer-use on for this assertion by registering the group
        // directly (the env gate is exercised separately).
        let root = temp_root("schemas");
        let mut reg = default_registry(&root);
        computer_use::register_all(&mut reg, root.join("shots"));

        let names: Vec<String> = reg
            .schemas()
            .as_array()
            .expect("canonical array")
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();

        assert!(names.contains(&"read_file".to_string()), "file_tools group");
        assert!(
            names.iter().any(|n| n.starts_with("whiteboard_")),
            "whiteboard group"
        );
        assert!(
            names.iter().any(|n| n.starts_with("computer_use_")),
            "computer_use group"
        );
        // Every entry has the canonical shape.
        for entry in reg.schemas().as_array().unwrap() {
            assert_eq!(entry["type"], "function");
            assert!(
                entry["function"]["parameters"].is_object(),
                "schema is an object"
            );
        }
    }

    #[test]
    fn stages_whitelist_is_fully_bound() {
        use lingmiao_core::config::Config;
        // Every tool named in stages.json must resolve in the full registry —
        // the binding gap that previously left `copy_file`/`bash`/memory-*/
        // business_db_* dangling. The computer-use group is gated off by default
        // (`LINGMIAO_COMPUTER_USE`), so bind against the *enabled* registry: its
        // names must still be whitelisted so they resolve once the switch is on.
        let dir = temp_root("binding");
        let mem = Arc::new(
            Memory::open_in_dir(&dir, lingmiao_memory::Zone::Chat, None).expect("open memory"),
        );
        let cfg = Config::load_default().expect("config");
        let mut reg = full_registry(&dir, mem, &cfg, None);
        computer_use::register_all(&mut reg, dir.join("shots"));
        assert!(!cfg.stages().is_empty());
        for (stage, sc) in cfg.stages() {
            for tool in &sc.tools {
                // `mcp:*` is a wildcard for dynamically-named remote tools, not a
                // concrete registry entry — skip it here (its resolution is
                // covered by `schemas_for_mcp_wildcard_matches_remote_tools_only`).
                if tool.contains('*') {
                    continue;
                }
                assert!(
                    reg.contains(tool),
                    "stage `{stage}` whitelists unregistered tool `{tool}`"
                );
            }
        }
        // 需求② §6.2: the `help` tool is whitelisted in every stage and
        // resolves in the registry.
        assert!(reg.contains("help"), "help tool not registered");
        for (stage, sc) in cfg.stages() {
            assert!(
                sc.tools.contains(&"help".to_string()),
                "stage `{stage}` whitelist is missing `help`"
            );
        }
        // The new groups are wired in.
        for n in memory_tools::TOOL_NAMES {
            assert!(reg.contains(n), "missing memory tool {n}");
        }
        for n in business_tools::TOOL_NAMES {
            assert!(reg.contains(n), "missing business tool {n}");
        }
        // 需求⑤: the `meta` group is registered and, like `help`, reachable in
        // *every* stage (默认在场).
        for n in meta_tools::TOOL_NAMES {
            assert!(reg.contains(n), "missing meta tool {n}");
            for (stage, sc) in cfg.stages() {
                assert!(
                    sc.tools.contains(&n.to_string()),
                    "stage `{stage}` whitelist is missing meta tool `{n}`"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
