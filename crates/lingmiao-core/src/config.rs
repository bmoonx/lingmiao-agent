//! Configuration — ported from `core/config.py`.
//!
//! Q8 decision: the four JSON descriptors (`prompts` / `stages` / `locks` / `mcp`)
//! are **embedded** into the binary via [`include_str!`] and deserialized into
//! strongly-typed structs, so a malformed config fails at *startup* rather than
//! at first use. A user may still override any file wholesale by placing a
//! same-named JSON in the config-dir env var (see [`crate::brand::env`], or an
//! explicitly supplied directory).
//!
//! The original Python exposed attribute/dict access over loosely typed JSON
//! (`cfg.stages["沉淀阶段"].max_turns`); here those become real fields checked
//! by the compiler.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::ConfigError;

const DEFAULT_PROMPTS: &str = include_str!("../assets/prompts.json");
const DEFAULT_STAGES: &str = include_str!("../assets/stages.json");
const DEFAULT_LOCKS: &str = include_str!("../assets/locks.json");
const DEFAULT_MCP: &str = include_str!("../assets/mcp.json");
const DEFAULT_HELP: &str = include_str!("../assets/help.json");

/// Canonical pipeline stage name: context selection.
pub const STAGE_B_CONTEXT: &str = "组织上下文";
/// Canonical pipeline stage name: the conversation stage.
pub const STAGE_C_DIALOG: &str = "工作阶段";
/// Canonical pipeline stage name: the consolidated stage (Q9). As of the
/// 2026-09-28 rename the algorithmic memory-graph update (原版 `I-知识图谱更新` /
/// MG evolution) is **folded into this stage** — it runs right after the LLM
/// call and reports the same name, so the pipeline is exactly three stages.
pub const STAGE_CONSOLIDATE: &str = "沉淀阶段";

/// Legacy (pre-2026-09-28) stage names → canonical names.
///
/// The 2026-09-28 rename replaced the Python-era `字母-中文` stage names with
/// plain Chinese ones and folded the algorithmic MG update (`I-知识图谱更新`)
/// into `沉淀阶段`. An external override dir (`<PREFIX>_CONFIG_DIR`) or user
/// `config.json` still carrying the old keys would otherwise **silently miss**
/// every lookup — so every stage-name read resolves through this table first.
pub const LEGACY_STAGE_ALIASES: &[(&str, &str)] = &[
    ("B-上下文选择", STAGE_B_CONTEXT),
    ("C-对话", STAGE_C_DIALOG),
    ("总结流程", STAGE_CONSOLIDATE),
    ("I-知识图谱更新", STAGE_CONSOLIDATE),
];

/// Resolve a possibly-legacy stage name to its canonical (current) name.
pub fn canonical_stage(stage: &str) -> &str {
    LEGACY_STAGE_ALIASES
        .iter()
        .find(|(old, _)| *old == stage)
        .map(|(_, new)| *new)
        .unwrap_or(stage)
}

/// `stages.json` uses the full stage names, but `prompts.json` keys the two
/// pipeline stages by short aliases (`B` / `C`) — a quirk inherited from the
/// Python original, whose call sites read `cfg.prompt("B")` / `cfg.prompt("C")`
/// (`stages/b.py`, `core/context.py`). The consolidated stage is keyed by its
/// full name. This table is the single place that maps one to the other.
pub const STAGE_PROMPT_KEYS: &[(&str, &str)] = &[
    (STAGE_B_CONTEXT, "B"),
    (STAGE_C_DIALOG, "C"),
    (STAGE_CONSOLIDATE, STAGE_CONSOLIDATE),
];

/// The `prompts.json` key for a stage (falls back to the stage name itself).
pub fn stage_prompt_key(stage: &str) -> &str {
    STAGE_PROMPT_KEYS
        .iter()
        .find(|(s, _)| *s == stage)
        .map(|(_, k)| *k)
        .unwrap_or(stage)
}

/// Per-stage tool whitelist + timeout (`stages.json`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StageCfg {
    /// Tool names the stage may call.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Stage time budget in seconds.
    #[serde(default)]
    pub timeout: u64,
}

/// One stage's prompt bundle (`prompts.json` value object).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PromptEntry {
    /// System prompt.
    #[serde(default)]
    pub system: String,
    /// Optional skill block.
    #[serde(default)]
    pub skill: String,
    /// Optional JSON-output rule.
    #[serde(default, rename = "json_rule")]
    pub json_rule: String,
}

/// One built-in help topic (`help.json`) — 需求② §6.1.
///
/// The help document ships with the binary (`include_str!`) and is queried by
/// the [`help` tool](lingmiao_tools) / `/help`; the same material is human-readable
/// as `docs/*.md`. Two audiences, two carriers.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HelpTopic {
    /// Human-facing title.
    #[serde(default)]
    pub title: String,
    /// Retrieval keywords for fuzzy topic matching.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Markdown body.
    #[serde(default)]
    pub body: String,
}

/// One MCP server declaration (`mcp.json`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpServer {
    /// Executable to spawn.
    #[serde(default)]
    pub command: String,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Which prompt field to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptField {
    /// `system`
    System,
    /// `skill`
    Skill,
    /// `json_rule`
    JsonRule,
}

impl PromptField {
    /// The JSON field name this selector maps to.
    pub fn key(self) -> &'static str {
        match self {
            PromptField::System => "system",
            PromptField::Skill => "skill",
            PromptField::JsonRule => "json_rule",
        }
    }
}

/// Resolved, typed configuration.
#[derive(Debug, Clone)]
pub struct Config {
    prompts: HashMap<String, PromptEntry>,
    base_prompt: String,
    env_prompt: String,
    search_strategy: String,
    stages: HashMap<String, StageCfg>,
    core_locks: HashMap<String, String>,
    base_core_lock: String,
    core_lock_max_len: usize,
    mcp_servers: HashMap<String, McpServer>,
    help_topics: HashMap<String, HelpTopic>,
    override_dir: Option<PathBuf>,
}

impl Config {
    /// Load with the embedded defaults (no external override).
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load(None)
    }

    /// Load, optionally overriding whole files from `override_dir`.
    pub fn load(override_dir: Option<PathBuf>) -> Result<Self, ConfigError> {
        if let Some(dir) = &override_dir
            && !dir.is_dir()
        {
            return Err(ConfigError::DirNotFound(dir.clone()));
        }
        let dir = override_dir.as_deref();

        let prompts = parse_prompts(&read_json(dir, "prompts.json", DEFAULT_PROMPTS)?)?;
        let stages = parse_stages(&read_json(dir, "stages.json", DEFAULT_STAGES)?)?;
        let (core_locks, base_core_lock, core_lock_max_len) =
            parse_locks(&read_json(dir, "locks.json", DEFAULT_LOCKS)?)?;
        let mcp_servers = parse_mcp(&read_json(dir, "mcp.json", DEFAULT_MCP)?)?;
        let help_topics = parse_help(&read_json(dir, "help.json", DEFAULT_HELP)?)?;

        Ok(Self {
            prompts: prompts.entries,
            base_prompt: prompts.base,
            env_prompt: prompts.env,
            search_strategy: prompts.search_strategy,
            stages,
            core_locks,
            base_core_lock,
            core_lock_max_len,
            mcp_servers,
            help_topics,
            override_dir: override_dir.clone(),
        })
    }

    /// Load honouring the config-dir environment variable (`<PREFIX>_CONFIG_DIR`).
    ///
    /// An external override must be *compatible with the Rust pipeline*: it has
    /// to define every stage in [`STAGE_PROMPT_KEYS`] (and their prompts). A dir
    /// that is unset, absent, or simply carries a foreign/stale config — e.g.
    /// the original Python implementation's pre-Q9 `config/` picked up via a
    /// stale externally exported config-dir variable — is rejected with a
    /// warning and the embedded
    /// defaults are used instead, rather than silently running `B`/`C` with no
    /// system prompt and no `沉淀阶段` stage.
    pub fn discover() -> Result<Self, ConfigError> {
        let dir = std::env::var_os(crate::brand::env("CONFIG_DIR"))
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let Some(dir) = dir else {
            return Self::load(None);
        };
        if !dir.is_dir() {
            tracing::warn!(
                dir = %dir.display(),
                "config override dir does not exist; using embedded defaults"
            );
            return Self::load(None);
        }
        let cfg = Self::load(Some(dir.clone()))?;
        if cfg.has_pipeline() {
            return Ok(cfg);
        }
        tracing::warn!(
            dir = %dir.display(),
            "config override is missing the `{STAGE_CONSOLIDATE}` pipeline; using embedded defaults"
        );
        Self::load(None)
    }

    /// Whether this config defines the full pipeline: every stage in
    /// [`STAGE_PROMPT_KEYS`] exists and carries a non-empty system prompt.
    /// Legacy stage names in an override resolve through [`Self::stage`].
    pub fn has_pipeline(&self) -> bool {
        STAGE_PROMPT_KEYS.iter().all(|(stage, key)| {
            self.stage(stage).is_some() && !self.prompt(key, PromptField::System).is_empty()
        })
    }

    /// The override directory in use, if any.
    pub fn override_dir(&self) -> Option<&Path> {
        self.override_dir.as_deref()
    }

    /// Read a prompt field for a stage (empty string when absent).
    ///
    /// The lookup resolves legacy stage names first ([`canonical_stage`]) and
    /// then the `prompts.json` key for that stage ([`stage_prompt_key`]) — so
    /// both the canonical name (`工作阶段`) and the pre-rename one (`C-对话`,
    /// which maps to the `C` prompt entry) resolve.
    pub fn prompt(&self, stage: &str, field: PromptField) -> &str {
        let canon = canonical_stage(stage);
        if let Some(e) = self.prompts.get(canon) {
            return prompt_field(e, field);
        }
        if let Some(e) = self.prompts.get(stage_prompt_key(canon)) {
            return prompt_field(e, field);
        }
        ""
    }

    /// The shared base prompt (`_base`).
    pub fn base_prompt(&self) -> &str {
        &self.base_prompt
    }

    /// The shared environment block (`_env`).
    pub fn env_prompt(&self) -> &str {
        &self.env_prompt
    }

    /// The shared memory search strategy block (`_search_strategy`).
    pub fn search_strategy(&self) -> &str {
        &self.search_strategy
    }

    /// All stage prompts.
    pub fn prompts(&self) -> &HashMap<String, PromptEntry> {
        &self.prompts
    }

    /// Stage params for a stage, if configured.
    ///
    /// Legacy names resolve to their canonical row ([`canonical_stage`]), so an
    /// override `stages.json` with the pre-rename keys keeps working.
    pub fn stage(&self, name: &str) -> Option<&StageCfg> {
        let canon = canonical_stage(name);
        self.stages.get(canon).or_else(|| self.stages.get(name))
    }

    /// All stage param rows.
    pub fn stages(&self) -> &HashMap<String, StageCfg> {
        &self.stages
    }

    /// Raw per-stage core lock text (`locks.json`/`core_locks`). Legacy stage
    /// names resolve to their canonical key ([`canonical_stage`]).
    pub fn core_lock(&self, stage: &str) -> &str {
        let canon = canonical_stage(stage);
        self.core_locks
            .get(canon)
            .or_else(|| self.core_locks.get(stage))
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The shared base core lock (`_base_core_lock`).
    pub fn base_core_lock(&self) -> &str {
        &self.base_core_lock
    }

    /// Maximum length of a composed core lock.
    pub fn core_lock_max_len(&self) -> usize {
        self.core_lock_max_len
    }

    /// Compose `base + stage` core lock, truncated to `core_lock_max_len`.
    pub fn full_core_lock(&self, stage: &str) -> String {
        let mut s = self.base_core_lock.clone();
        s.push_str(self.core_lock(stage));
        truncate_chars(&mut s, self.core_lock_max_len);
        s
    }

    /// Declared MCP servers.
    pub fn mcp_servers(&self) -> &HashMap<String, McpServer> {
        &self.mcp_servers
    }

    /// Built-in help topics (`help.json`), keyed by topic id.
    pub fn help_topics(&self) -> &HashMap<String, HelpTopic> {
        &self.help_topics
    }

    /// One help topic by id.
    pub fn help_topic(&self, id: &str) -> Option<&HelpTopic> {
        self.help_topics.get(id)
    }
}

/// The embedded help topics, parsed once.
///
/// The [`help` tool](lingmiao_tools) reads this static copy (always the shipped,
/// embedded document), independent of any `LINGMIAO_CONFIG_DIR` override.
pub fn embedded_help_topics() -> &'static HashMap<String, HelpTopic> {
    static EMBEDDED_HELP: OnceLock<HashMap<String, HelpTopic>> = OnceLock::new();
    EMBEDDED_HELP.get_or_init(|| {
        let v: Value = serde_json::from_str(DEFAULT_HELP)
            .expect("embedded help.json is valid JSON (checked by tests)");
        parse_help(&v).expect("embedded help.json parses (checked by tests)")
    })
}

/// Truncate a string to at most `max` Unicode scalar values.
fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() <= max {
        return;
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    s.truncate(end);
}

/// Read one field of a [`PromptEntry`].
fn prompt_field(e: &PromptEntry, field: PromptField) -> &str {
    match field {
        PromptField::System => e.system.as_str(),
        PromptField::Skill => e.skill.as_str(),
        PromptField::JsonRule => e.json_rule.as_str(),
    }
}

fn read_json(dir: Option<&Path>, file: &str, embedded: &str) -> Result<Value, ConfigError> {
    if let Some(dir) = dir {
        let path = dir.join(file);
        if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
                path: path.clone(),
                source,
            })?;
            return serde_json::from_str(&text).map_err(|e| ConfigError::Invalid {
                file: file.to_string(),
                detail: e.to_string(),
            });
        }
    }
    serde_json::from_str(embedded).map_err(|e| ConfigError::Invalid {
        file: file.to_string(),
        detail: e.to_string(),
    })
}

struct PromptBundle {
    entries: HashMap<String, PromptEntry>,
    base: String,
    env: String,
    search_strategy: String,
}

fn parse_prompts(v: &Value) -> Result<PromptBundle, ConfigError> {
    let obj = v.as_object().ok_or_else(|| ConfigError::Invalid {
        file: "prompts.json".into(),
        detail: "expected a JSON object".into(),
    })?;
    let mut entries = HashMap::new();
    let mut base = String::new();
    let mut env = String::new();
    let mut search_strategy = String::new();
    for (key, val) in obj {
        match key.as_str() {
            "_base" => base = val.as_str().unwrap_or_default().to_string(),
            "_env" => env = val.as_str().unwrap_or_default().to_string(),
            "_search_strategy" => search_strategy = val.as_str().unwrap_or_default().to_string(),
            _ if key.starts_with('_') => {}
            _ => {
                let entry: PromptEntry =
                    serde_json::from_value(val.clone()).map_err(|e| ConfigError::Invalid {
                        file: "prompts.json".into(),
                        detail: format!("prompt `{key}`: {e}"),
                    })?;
                entries.insert(key.clone(), entry);
            }
        }
    }
    Ok(PromptBundle {
        entries,
        base,
        env,
        search_strategy,
    })
}

fn parse_stages(v: &Value) -> Result<HashMap<String, StageCfg>, ConfigError> {
    let obj = v.as_object().ok_or_else(|| ConfigError::Invalid {
        file: "stages.json".into(),
        detail: "expected a JSON object".into(),
    })?;
    let mut stages = HashMap::new();
    for (key, val) in obj {
        if key.starts_with('_') {
            continue;
        }
        let cfg: StageCfg =
            serde_json::from_value(val.clone()).map_err(|e| ConfigError::Invalid {
                file: "stages.json".into(),
                detail: format!("stage `{key}`: {e}"),
            })?;
        stages.insert(key.clone(), cfg);
    }
    Ok(stages)
}

fn parse_locks(v: &Value) -> Result<(HashMap<String, String>, String, usize), ConfigError> {
    #[derive(Deserialize)]
    struct LocksRaw {
        #[serde(default, rename = "_base_core_lock")]
        base_core_lock: String,
        #[serde(default)]
        core_lock_max_len: usize,
        #[serde(default)]
        core_locks: HashMap<String, String>,
    }
    let raw: LocksRaw = serde_json::from_value(v.clone()).map_err(|e| ConfigError::Invalid {
        file: "locks.json".into(),
        detail: e.to_string(),
    })?;
    let max_len = if raw.core_lock_max_len == 0 {
        500
    } else {
        raw.core_lock_max_len
    };
    Ok((raw.core_locks, raw.base_core_lock, max_len))
}

fn parse_mcp(v: &Value) -> Result<HashMap<String, McpServer>, ConfigError> {
    let obj = v.as_object().ok_or_else(|| ConfigError::Invalid {
        file: "mcp.json".into(),
        detail: "expected a JSON object".into(),
    })?;
    let servers_val = obj.get("mcpServers").ok_or_else(|| ConfigError::Invalid {
        file: "mcp.json".into(),
        detail: "missing `mcpServers`".into(),
    })?;
    let servers_obj = servers_val
        .as_object()
        .ok_or_else(|| ConfigError::Invalid {
            file: "mcp.json".into(),
            detail: "`mcpServers` must be an object".into(),
        })?;
    let mut servers = HashMap::new();
    for (key, val) in servers_obj {
        if key.starts_with('_') {
            continue;
        }
        let server: McpServer =
            serde_json::from_value(val.clone()).map_err(|e| ConfigError::Invalid {
                file: "mcp.json".into(),
                detail: format!("server `{key}`: {e}"),
            })?;
        servers.insert(key.clone(), server);
    }
    Ok(servers)
}

fn parse_help(v: &Value) -> Result<HashMap<String, HelpTopic>, ConfigError> {
    let obj = v.as_object().ok_or_else(|| ConfigError::Invalid {
        file: "help.json".into(),
        detail: "expected a JSON object".into(),
    })?;
    let topics_val = obj.get("topics").ok_or_else(|| ConfigError::Invalid {
        file: "help.json".into(),
        detail: "missing `topics`".into(),
    })?;
    let topics_obj = topics_val.as_object().ok_or_else(|| ConfigError::Invalid {
        file: "help.json".into(),
        detail: "`topics` must be an object".into(),
    })?;
    let mut topics = HashMap::new();
    for (key, val) in topics_obj {
        if key.starts_with('_') {
            continue;
        }
        let topic: HelpTopic =
            serde_json::from_value(val.clone()).map_err(|e| ConfigError::Invalid {
                file: "help.json".into(),
                detail: format!("topic `{key}`: {e}"),
            })?;
        topics.insert(key.clone(), topic);
    }
    Ok(topics)
}

static GLOBAL: OnceLock<Config> = OnceLock::new();

/// Process-wide configuration, discovered once.
///
/// # Panics
/// Panics if the on-disk override directory contains an invalid config — an
/// invalid configuration is fatal at startup by design (Q8).
pub fn global() -> &'static Config {
    GLOBAL.get_or_init(|| {
        Config::discover()
            .unwrap_or_else(|e| panic!("failed to load {} config: {e}", crate::brand::NAME))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_defaults_load_and_are_typed() {
        let cfg = Config::load_default().expect("defaults must parse");
        // stages.json: 沉淀阶段 declares a timeout of 600
        let d = cfg.stage("沉淀阶段").expect("stage exists");
        assert_eq!(d.timeout, 600);
        assert!(d.tools.contains(&"search_observations".to_string()));
        // prompts.json exposes the C-stage system prompt
        assert!(!cfg.prompt("C", PromptField::System).is_empty());
        assert!(!cfg.base_prompt().is_empty());
    }

    #[test]
    fn c_stage_whitelists_computer_use_group() {
        // The `C`-stage system prompt advertises the computer-use tools; the
        // `stages.json` whitelist must include them or `schemas_for` drops them
        // and the model can never call them. The group is registered
        // unconditionally (2026-10-05 决策 removed the env gate), so the
        // whitelist is the only thing that could block them.
        let cfg = Config::load_default().unwrap();
        let c = cfg.stage(STAGE_C_DIALOG).expect("C stage exists");
        for name in [
            "computer_use_status",
            "computer_use_screenshot",
            "computer_use_mousemove",
            "computer_use_click",
            "computer_use_type",
            "computer_use_key",
            "computer_use_run",
        ] {
            assert!(
                c.tools.contains(&name.to_string()),
                "工作阶段 whitelist is missing `{name}` (the prompt advertises it)"
            );
        }
    }

    #[test]
    fn embedded_help_has_full_topic_set() {
        let cfg = Config::load_default().expect("defaults must parse");
        // 需求② §6.1: the initial help document covers all topics. Two added
        // after the first release document the 需求⑤ meta platform and the
        // verifier (原版 verifier.py) so the AI can discover them via `help`.
        assert_eq!(cfg.help_topics().len(), 19, "expected 19 help topics");
        for id in [
            "overview",
            "install",
            "layout",
            "models",
            "config",
            "env",
            "commands",
            "whiteboard",
            "session",
            "pipeline",
            "memory",
            "tools",
            "meta",
            "verify",
            "mcp",
            "logs",
            "troubleshoot",
            "errors",
            "for-ai",
        ] {
            let t = cfg
                .help_topic(id)
                .unwrap_or_else(|| panic!("missing topic {id}"));
            assert!(!t.title.is_empty(), "topic {id} has no title");
            assert!(!t.body.is_empty(), "topic {id} has no body");
        }
        // The static accessor (used by the help tool) agrees with the config.
        assert_eq!(embedded_help_topics().len(), 19);
        assert!(embedded_help_topics().contains_key("models"));
        assert!(embedded_help_topics().contains_key("meta"));
        assert!(embedded_help_topics().contains_key("verify"));
    }

    #[test]
    fn stage_prompt_keys_map_short_aliases() {
        // `stages.json` full names → `prompts.json` keys (Python parity).
        assert_eq!(stage_prompt_key(STAGE_B_CONTEXT), "B");
        assert_eq!(stage_prompt_key(STAGE_C_DIALOG), "C");
        assert_eq!(stage_prompt_key(STAGE_CONSOLIDATE), "沉淀阶段");
        // Unknown stages fall back to their own name.
        assert_eq!(stage_prompt_key("A-验收"), "A-验收");
        // The embedded defaults are a complete pipeline.
        assert!(Config::load_default().unwrap().has_pipeline());
    }

    #[test]
    fn legacy_stage_names_resolve_to_canonical() {
        // 2026-09-28 rename compatibility: an override carrying the old names
        // must still resolve — silently missing would flip a user's per-stage
        // model route back to the default with no error.
        assert_eq!(canonical_stage("B-上下文选择"), STAGE_B_CONTEXT);
        assert_eq!(canonical_stage("C-对话"), STAGE_C_DIALOG);
        assert_eq!(canonical_stage("总结流程"), STAGE_CONSOLIDATE);
        assert_eq!(canonical_stage("I-知识图谱更新"), STAGE_CONSOLIDATE);
        // A canonical (or unknown) name is returned unchanged.
        assert_eq!(canonical_stage(STAGE_C_DIALOG), STAGE_C_DIALOG);
        assert_eq!(canonical_stage("unknown"), "unknown");

        // Config lookups go through the alias table.
        let cfg = Config::load_default().unwrap();
        assert_eq!(cfg.stage("C-对话").map(|s| s.timeout), Some(0));
        assert!(!cfg.full_core_lock("C-对话").is_empty());
        assert!(!cfg.prompt("C-对话", PromptField::System).is_empty());
    }

    #[test]
    fn core_lock_composition_respects_max_len() {
        let cfg = Config::load_default().unwrap();
        let lock = cfg.full_core_lock("工作阶段");
        assert!(lock.starts_with(cfg.base_core_lock()));
        assert!(lock.chars().count() <= cfg.core_lock_max_len());
    }

    #[test]
    fn missing_override_dir_is_an_error() {
        let err = Config::load(Some(PathBuf::from("/no/such/dir"))).unwrap_err();
        assert!(matches!(err, ConfigError::DirNotFound(_)));
    }
}
