//! `config.json` — the **user configuration file** (cli 2026-09-27).
//!
//! One JSON next to the binary (or in the working directory) carries the
//! user-editable configuration:
//!
//! ```json
//! {
//!   "models": { "default": { "group": "deepseek", "model": "…" }, "groups": [ … ] },
//!   "stages": {
//!     "组织上下文": { "group": "deepseek", "model": "deepseek-v4-flash" },
//!     "工作阶段":      { "group": "deepseek", "model": "deepseek-v4-pro" },
//!     "沉淀阶段":    { "group": "kimi",     "model": "kimi-for-coding" }
//!   }
//! }
//! ```
//!
//! * `stages` routes **each pipeline stage to its own model** — and, because a
//!   group *is* an API source, a stage may talk to a completely different
//!   provider (a Kimi 沉淀阶段 alongside a DeepSeek `工作阶段`).
//! * `models` is the optional model catalogue; when absent the loader falls back
//!   to the `models.json` chain ([`crate::models::ModelRegistry::load`]).
//! * A stage missing from `stages` (or the whole `stages` key) falls back to the
//!   default model — so an old file, or no file at all, keeps the previous
//!   single-model behaviour.
//!
//! ## Loading priority
//! `LINGMIAO_CONFIG` (explicit path) → `<exe dir>/config.json` →
//! `<cwd>/config.json` → none (all defaults). Any candidate that is absent
//! falls through, mirroring [`crate::models::ModelRegistry::load`].

use std::collections::HashMap;
use std::path::PathBuf;

use lingmiao_core::{LingmiaoError, brand};
use serde::Deserialize;
use serde_json::Value;

/// One stage's model selection (`stages.<name>` in `config.json`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StageSel {
    /// API source (a `groups[].id` in the catalogue), e.g. `deepseek` / `kimi`.
    #[serde(default)]
    pub group: String,
    /// Model id within that group. Empty = the group's first model.
    #[serde(default)]
    pub model: String,
}

impl StageSel {
    /// `group/model` as shown in the UI.
    pub fn display(&self) -> String {
        format!("{}/{}", self.group, self.model)
    }
}

/// The raw `config.json` shape.
#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigFile {
    /// Optional model catalogue (`models.json`'s schema, nested under `models`).
    #[serde(default)]
    models: Option<Value>,
    /// Per-stage model routing.
    #[serde(default)]
    stages: HashMap<String, StageSel>,
}

/// The parsed user configuration (empty when no `config.json` exists).
#[derive(Debug, Clone, Default)]
pub struct UserConfig {
    /// The embedded model catalogue, when the file carries one.
    pub models: Option<Value>,
    /// Per-stage model routing (empty = every stage uses the default model).
    pub stages: HashMap<String, StageSel>,
    /// Where the file was read from (`None` = no `config.json`).
    pub source: Option<PathBuf>,
}

impl UserConfig {
    /// The empty configuration (no file): default catalogue, no stage routing.
    pub fn defaults() -> Self {
        Self::default()
    }

    /// Load following the documented priority.
    pub fn load() -> Result<Self, LingmiaoError> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(explicit) = std::env::var(brand::env("CONFIG"))
            && !explicit.trim().is_empty()
        {
            candidates.push(PathBuf::from(explicit));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join("config.json"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("config.json"));
        }
        Self::load_from(&candidates)
    }

    /// Load the first existing candidate; `defaults()` when none exists. Split
    /// out so the priority order is unit-testable without the real exe/cwd.
    pub fn load_from(candidates: &[PathBuf]) -> Result<Self, LingmiaoError> {
        for path in candidates {
            if path.is_file() {
                return Self::from_file(path);
            }
        }
        Ok(Self::defaults())
    }

    /// Parse one `config.json`.
    pub fn from_file(path: &std::path::Path) -> Result<Self, LingmiaoError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LingmiaoError::config(format!("cannot read {}: {e}", path.display())))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LingmiaoError::config(format!("invalid {}: {e}", path.display())))?;
        Self::from_json(&v, Some(path.to_path_buf()))
    }

    /// Parse an already-decoded `config.json` value.
    pub fn from_json(v: &Value, source: Option<PathBuf>) -> Result<Self, LingmiaoError> {
        let file: ConfigFile = serde_json::from_value(v.clone())
            .map_err(|e| LingmiaoError::config(format!("invalid config.json: {e}")))?;
        Ok(Self {
            models: file.models,
            stages: file.stages,
            source,
        })
    }

    /// The model selection for `stage`, if the file routes it.
    ///
    /// The canonical name is tried first, then the pre-2026-09-28 legacy name
    /// ([`lingmiao_core::config::canonical_stage`]) — a user file still carrying
    /// `C-对话` etc. keeps routing instead of silently falling back.
    pub fn stage(&self, stage: &str) -> Option<&StageSel> {
        let canon = lingmiao_core::config::canonical_stage(stage);
        self.stages
            .get(canon)
            .or_else(|| self.stages.get(stage))
            .filter(|s| !s.group.trim().is_empty())
    }

    /// Whether the file routes any stage to its own model.
    pub fn has_stage_routing(&self) -> bool {
        !self.stages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn missing_file_yields_defaults() {
        let uc = UserConfig::load_from(&[PathBuf::from("/nonexistent/config.json")]).unwrap();
        assert!(uc.models.is_none());
        assert!(!uc.has_stage_routing());
        assert!(uc.source.is_none());
    }

    #[test]
    fn parses_models_and_per_stage_routing() {
        let v: Value = serde_json::from_str(
            r#"{
              "models": { "default": {"group":"deepseek","model":"a"},
                          "groups":[{"id":"deepseek","protocol":"openai","base_url":"http://x",
                                     "models":[{"id":"a"}]}] },
              "stages": {
                "组织上下文": { "group": "deepseek", "model": "a" },
                "沉淀阶段":    { "group": "kimi",     "model": "kimi-for-coding" }
              }
            }"#,
        )
        .unwrap();
        let uc = UserConfig::from_json(&v, None).unwrap();
        assert!(uc.models.is_some(), "the catalogue rides along");
        assert!(uc.has_stage_routing());
        assert_eq!(uc.stage("沉淀阶段").unwrap().group, "kimi");
        assert_eq!(uc.stage("组织上下文").unwrap().display(), "deepseek/a");
        // An unrouted stage falls back (no entry).
        assert!(uc.stage("工作阶段").is_none());
    }

    #[test]
    fn empty_stage_group_is_not_routing() {
        let v: Value = serde_json::json!({"stages": {"工作阶段": {"model": "x"}}});
        let uc = UserConfig::from_json(&v, None).unwrap();
        assert!(
            uc.stage("工作阶段").is_none(),
            "a group-less entry cannot route (no API source)"
        );
        assert!(!uc.has_stage_routing() || uc.stage("工作阶段").is_none());
    }

    #[test]
    fn loads_the_first_existing_candidate() {
        let dir = std::env::temp_dir().join(format!("lingmiao-usercfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("config.json");
        let mut f = std::fs::File::create(&a).unwrap();
        write!(
            f,
            r#"{{"stages":{{"工作阶段":{{"group":"kimi","model":"kimi-for-coding"}}}}}}"#
        )
        .unwrap();
        let missing = dir.join("missing.json");
        let uc = UserConfig::load_from(&[missing, a.clone()]).unwrap();
        assert_eq!(uc.source.as_deref(), Some(a.as_path()));
        assert_eq!(uc.stage("工作阶段").unwrap().group, "kimi");
        std::fs::remove_dir_all(&dir).ok();
    }
}
