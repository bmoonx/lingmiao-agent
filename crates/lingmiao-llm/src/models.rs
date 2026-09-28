//! `models.json` loader — 需求② §3.2/§3.3.
//!
//! The registry flattens the `groups[].models[]` tree of `models.json` into the
//! list of switchable [`ModelSpec`]s the TUI cycles through, and resolves the
//! startup default.
//!
//! ## Loading priority (§3.3)
//! `LINGMIAO_MODELS` (explicit path) → `<exe dir>/models.json` →
//! `<cwd>/models.json` → the embedded default ([`crate::models::ModelRegistry::builtin`],
//! `assets/models.json`). Any candidate that is absent falls through — the same
//! "incompatible/missing → fall back to the embedded default" semantics as
//! [`lingmiao_core::config::Config::discover`].
//!
//! ## Field reference
//! See `docs/api-config.md` §3.2. This loader is a superset: in addition to the
//! documented `api_key` / `api_key_env`, a group may carry `base_url_env` /
//! `model_env` to preserve the legacy per-provider env overrides.

use std::path::{Path, PathBuf};

use lingmiao_core::{LingmiaoError, brand};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{Dialect, ModelSpec};

/// The embedded default (ships with the binary, serves as the fallback).
const DEFAULT_MODELS: &str = include_str!("../assets/models.json");

#[derive(Debug, Clone, Deserialize)]
struct ModelsFile {
    #[serde(default, rename = "default")]
    default_sel: Option<DefaultSel>,
    #[serde(default)]
    groups: Vec<GroupCfg>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DefaultSel {
    #[serde(default)]
    group: String,
    #[serde(default)]
    model: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GroupCfg {
    id: String,
    #[serde(default)]
    label: String,
    protocol: String,
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    api_key_env: String,
    #[serde(default)]
    base_url_env: String,
    #[serde(default)]
    model_env: String,
    #[serde(default)]
    models: Vec<ModelCfg>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelCfg {
    id: String,
    #[serde(default)]
    label: String,
    /// Accepted for **compatibility only** — deliberately ignored.
    ///
    /// cli 2026-09-27: the system must not assume any context-window size (the
    /// provider APIs do not report one reliably and a hard-coded figure is a
    /// guess). The key is still *consumed* here so a user's `models.json` that
    /// carries it does not leak into the request body via `extra`.
    #[serde(default)]
    #[allow(dead_code)]
    context_window: u64,
    /// Whether this model accepts image (multimodal) input. Drives the ⑥ vision
    /// gate in the stage agent: only a `supports_vision` model gets images as
    /// multipart attachments instead of a textual marker.
    #[serde(default)]
    supports_vision: bool,
    /// Every other key is passed through into the request body verbatim
    /// (e.g. DeepSeek `thinking`).
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

/// The parsed model catalogue (one or more API groups, each with models).
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    groups: Vec<GroupCfg>,
    default_sel: Option<DefaultSel>,
    source: Option<PathBuf>,
}

impl ModelRegistry {
    /// The embedded default catalogue.
    pub fn builtin() -> Result<Self, LingmiaoError> {
        let v: Value = serde_json::from_str(DEFAULT_MODELS)
            .map_err(|e| LingmiaoError::config(format!("embedded models.json is invalid: {e}")))?;
        Self::from_json(&v)
    }

    /// Load the catalogue, honouring the user configuration:
    ///
    /// 1. `LINGMIAO_MODELS` (explicit `models.json` path) — wins outright;
    /// 2. the `models` block of `config.json` (exe dir / cwd / `LINGMIAO_CONFIG`),
    ///    when the file carries one;
    /// 3. `<exe dir>/models.json` → `<cwd>/models.json` (legacy);
    /// 4. the embedded default.
    ///
    /// Every candidate that is absent (or, for `config.json`, lacks the `models`
    /// key) falls through, so an old setup keeps working unchanged.
    pub fn load() -> Result<Self, LingmiaoError> {
        if let Ok(explicit) = std::env::var(brand::env("MODELS"))
            && !explicit.trim().is_empty()
        {
            let path = PathBuf::from(explicit);
            if path.is_file() {
                return Self::from_file(&path);
            }
        }
        // config.json's nested catalogue (cli 2026-09-27: one user config file).
        if let Ok(uc) = crate::UserConfig::load()
            && let Some(models) = uc.models
        {
            return Self::from_json_sourced(&models, uc.source);
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join("models.json"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("models.json"));
        }
        Self::load_from(&candidates)
    }

    /// Load the first existing candidate file, else the embedded default. Split
    /// out so the priority order is unit-testable without touching the real
    /// process exe/cwd.
    pub fn load_from(candidates: &[PathBuf]) -> Result<Self, LingmiaoError> {
        for path in candidates {
            if path.is_file() {
                return Self::from_file(path);
            }
        }
        Self::builtin()
    }

    /// Load a specific `models.json` file.
    pub fn from_file(path: &Path) -> Result<Self, LingmiaoError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LingmiaoError::config(format!("cannot read {}: {e}", path.display())))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LingmiaoError::config(format!("invalid {}: {e}", path.display())))?;
        let mut reg = Self::from_json(&v)?;
        reg.source = Some(path.to_path_buf());
        Ok(reg)
    }

    /// Build from an already-parsed JSON value.
    pub fn from_json(v: &Value) -> Result<Self, LingmiaoError> {
        Self::from_json_sourced(v, None)
    }

    /// Build from a JSON value, recording where it came from. Used when the
    /// catalogue arrives nested in `config.json` (see [`crate::UserConfig`]).
    pub fn from_json_sourced(v: &Value, source: Option<PathBuf>) -> Result<Self, LingmiaoError> {
        let file: ModelsFile = serde_json::from_value(v.clone())
            .map_err(|e| LingmiaoError::config(format!("invalid models.json: {e}")))?;
        if file.groups.is_empty() {
            return Err(LingmiaoError::config("models.json defines no groups"));
        }
        Ok(Self {
            groups: file.groups,
            default_sel: file.default_sel,
            source,
        })
    }

    /// Resolve `(group, model)` **checking credentials** — unlike [`Self::resolve`]
    /// (best-effort, for the switch list), this errors when the group names an
    /// `api_key_env` that is not set, so a mis-routed stage fails loudly instead
    /// of 401ing at the first call. `None` when the pair is unknown.
    ///
    /// The legacy per-group `model_env` override (`DEEPSEEK_MODEL=…`) is
    /// **ignored** here: a `config.json` stage route names an exact model, and
    /// letting a legacy env var silently rewrite it would make the route lie
    /// about which model a stage runs on (cli 2026-09-27).
    pub fn resolve_checked(
        &self,
        group_id: &str,
        model_id: &str,
    ) -> Option<Result<ModelSpec, LingmiaoError>> {
        let g = self.groups.iter().find(|g| g.id == group_id)?;
        let m = if model_id.is_empty() {
            g.models.first()?
        } else {
            g.models.iter().find(|m| m.id == model_id)?
        };
        match self.resolve_api_key(g) {
            Ok(key) => Some(Ok(self.build_with_key(g, m, key, false))),
            Err(e) => Some(Err(e)),
        }
    }

    /// Where the catalogue came from (`None` = embedded default).
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Group ids, in file order.
    pub fn group_ids(&self) -> Vec<&str> {
        self.groups.iter().map(|g| g.id.as_str()).collect()
    }

    /// The full switch list: every `group × model` flattened (§3.4). Keys are
    /// resolved best-effort — a missing env key yields an empty `api_key`
    /// rather than erroring, so the TUI can still list and switch.
    pub fn specs(&self) -> Vec<ModelSpec> {
        self.groups
            .iter()
            .flat_map(|g| g.models.iter().map(move |m| self.build(g, m)))
            .collect()
    }

    /// Number of switchable `group × model` entries.
    pub fn len(&self) -> usize {
        self.groups.iter().map(|g| g.models.len()).sum()
    }

    /// Whether the catalogue has any switchable model.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve a `(group, model)` pair. An empty `model` selects the group's
    /// first model.
    pub fn resolve(&self, group_id: &str, model_id: &str) -> Option<ModelSpec> {
        let g = self.groups.iter().find(|g| g.id == group_id)?;
        let m = if model_id.is_empty() {
            g.models.first()?
        } else {
            g.models.iter().find(|m| m.id == model_id)?
        };
        Some(self.build(g, m))
    }

    /// The startup default `(group_id, model_id)`, following the `default` key,
    /// then `LLM_PROVIDER` (legacy), then the first group — without touching
    /// credentials.
    pub fn default_selection(&self) -> Option<(String, String)> {
        let g = self.pick_default_group()?;
        let model = self
            .default_sel
            .as_ref()
            .filter(|d| d.group == g.id && !d.model.is_empty())
            .map(|d| d.model.clone())
            .or_else(|| g.models.first().map(|m| m.id.clone()))?;
        Some((g.id.clone(), model))
    }

    /// Resolve the startup default spec — fatal when its API key is missing.
    pub fn default_spec(&self) -> Result<ModelSpec, LingmiaoError> {
        let (group_id, model_id) = self
            .default_selection()
            .ok_or_else(|| LingmiaoError::config("models.json has no selectable model"))?;
        let g = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .ok_or_else(|| {
                LingmiaoError::config(format!("default group `{group_id}` not found"))
            })?;
        self.resolve_api_key(g)?;
        self.resolve(&group_id, &model_id)
            .ok_or_else(|| LingmiaoError::config(format!("default model `{model_id}` not found")))
    }

    fn pick_default_group(&self) -> Option<&GroupCfg> {
        if let Some(sel) = &self.default_sel
            && let Some(g) = self
                .groups
                .iter()
                .find(|g| g.id == sel.group && !g.models.is_empty())
        {
            return Some(g);
        }
        // Legacy: `LLM_PROVIDER=<group id>` still steers the default group.
        if let Ok(provider) = std::env::var("LLM_PROVIDER")
            && let Some(g) = self
                .groups
                .iter()
                .find(|g| g.id == provider.trim() && !g.models.is_empty())
        {
            return Some(g);
        }
        self.groups.iter().find(|g| !g.models.is_empty())
    }

    fn resolve_api_key(&self, g: &GroupCfg) -> Result<String, LingmiaoError> {
        if !g.api_key.trim().is_empty() {
            return Ok(g.api_key.trim().to_string());
        }
        if !g.api_key_env.trim().is_empty() {
            let env_name = g.api_key_env.trim();
            return std::env::var(env_name)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    LingmiaoError::fatal(format!(
                        "{env_name} not set. Set it in .env or export it."
                    ))
                });
        }
        Ok(String::new())
    }

    fn build(&self, g: &GroupCfg, m: &ModelCfg) -> ModelSpec {
        let api_key = self.resolve_api_key(g).unwrap_or_default();
        self.build_with_key(g, m, api_key, true)
    }

    /// Build one spec. `honour_model_env` gates the legacy per-group
    /// `model_env` override (`DEEPSEEK_MODEL=…`): the interactive catalogue
    /// honours it (a user exporting it expects the switch list to follow), while
    /// an explicit `config.json` stage route does **not** — a route naming
    /// `deepseek-v4-pro` must run on pro even when that env var is set.
    fn build_with_key(
        &self,
        g: &GroupCfg,
        m: &ModelCfg,
        api_key: String,
        honour_model_env: bool,
    ) -> ModelSpec {
        let mut base_url = g.base_url.clone();
        if !g.base_url_env.trim().is_empty()
            && let Ok(v) = std::env::var(g.base_url_env.trim())
            && !v.is_empty()
        {
            base_url = v;
        }
        while base_url.ends_with('/') {
            base_url.pop();
        }
        let mut model = m.id.clone();
        if honour_model_env
            && !g.model_env.trim().is_empty()
            && let Ok(v) = std::env::var(g.model_env.trim())
            && !v.is_empty()
        {
            model = v;
        }
        let dialect = Dialect::parse(&g.protocol).unwrap_or_else(|| {
            tracing::warn!(
                group = %g.id,
                protocol = %g.protocol,
                "unknown protocol in models.json; defaulting to `openai`"
            );
            Dialect::OpenAi
        });
        let group_label = if g.label.is_empty() {
            g.id.clone()
        } else {
            g.label.clone()
        };
        let model_label = if m.label.is_empty() {
            m.id.clone()
        } else {
            m.label.clone()
        };
        ModelSpec {
            group_id: g.id.clone(),
            group_label,
            model_label,
            dialect,
            base_url,
            api_key,
            model,
            supports_vision: m.supports_vision,
            extra: Value::Object(m.extra.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn builtin_catalogue_flattens_group_times_model() {
        let reg = ModelRegistry::builtin().unwrap();
        // deepseek(3) + kimi(1) + claude(1) = 5
        assert_eq!(reg.len(), 5);
        assert_eq!(reg.specs().len(), 5);
        assert_eq!(reg.group_ids(), vec!["deepseek", "kimi", "claude"]);
        // Default selection is deepseek/deepseek-v4-flash-vision-exp (the `default` key).
        assert_eq!(
            reg.default_selection(),
            Some(("deepseek".into(), "deepseek-v4-flash-vision-exp".into()))
        );
    }

    #[test]
    fn builtin_dialects_and_extra() {
        let reg = ModelRegistry::builtin().unwrap();
        let pro = reg.resolve("deepseek", "deepseek-v4-pro").unwrap();
        assert_eq!(pro.dialect, Dialect::OpenAi);
        assert!(!pro.supports_vision);
        // `thinking: true` passed through verbatim as extra.
        assert_eq!(pro.extra.get("thinking"), Some(&Value::Bool(true)));
        let claude = reg.resolve("claude", "claude-fable-5").unwrap();
        assert_eq!(claude.dialect, Dialect::Anthropic);
        assert_eq!(claude.chat_url(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn vision_exp_is_the_default_and_is_flagged() {
        // ⑥ vision: the flash-vision-exp model is the startup default and is the
        // only deepseek model with `supports_vision: true`. The anthropic (claude)
        // group is also flagged (Python parity: `AnthropicClient.supports_vision
        // = True`) — the translation layer converts `image_url` parts into
        // Anthropic `image` blocks, so vision passes the gate end-to-end.
        let reg = ModelRegistry::builtin().unwrap();
        assert_eq!(
            reg.default_selection(),
            Some(("deepseek".into(), "deepseek-v4-flash-vision-exp".into()))
        );
        let vis = reg
            .resolve("deepseek", "deepseek-v4-flash-vision-exp")
            .unwrap();
        assert!(vis.supports_vision);
        let flash = reg.resolve("deepseek", "deepseek-v4-flash").unwrap();
        assert!(!flash.supports_vision);
        let claude = reg.resolve("claude", "claude-fable-5").unwrap();
        assert!(claude.supports_vision, "claude supports vision natively");
    }

    #[test]
    fn resolve_empty_model_picks_first() {
        let reg = ModelRegistry::builtin().unwrap();
        let g = reg.resolve("kimi", "").unwrap();
        assert_eq!(g.model, "kimi-for-coding");
        assert!(reg.resolve("nope", "x").is_none());
    }

    #[test]
    fn load_priority_prefers_first_existing_candidate() {
        let dir = std::env::temp_dir().join(format!("lingmiao-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        let mut fa = std::fs::File::create(&a).unwrap();
        write!(fa, r#"{{"groups":[{{"id":"alpha","protocol":"openai","base_url":"http://a","models":[{{"id":"m1"}}]}}]}}"#).unwrap();
        let mut fb = std::fs::File::create(&b).unwrap();
        write!(fb, r#"{{"groups":[{{"id":"beta","protocol":"openai","base_url":"http://b","models":[{{"id":"m2"}}]}}]}}"#).unwrap();

        // First existing candidate wins.
        let reg = ModelRegistry::load_from(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(reg.group_ids(), vec!["alpha"]);
        assert_eq!(reg.source(), Some(a.as_path()));

        // Missing first candidate falls through to the second.
        let missing = dir.join("missing.json");
        let reg = ModelRegistry::load_from(&[missing, b.clone()]).unwrap();
        assert_eq!(reg.group_ids(), vec!["beta"]);

        // No candidate exists → embedded default.
        let reg = ModelRegistry::load_from(&[dir.join("none.json")]).unwrap();
        assert_eq!(reg.source(), None);
        assert_eq!(reg.len(), 5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_json_rejects_empty_groups() {
        let err = ModelRegistry::from_json(&serde_json::json!({"groups": []})).unwrap_err();
        assert_eq!(err.kind(), lingmiao_core::LingmiaoErrorKind::Config);
    }
}
