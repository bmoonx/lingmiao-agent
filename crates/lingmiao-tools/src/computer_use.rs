//! Computer-use tools (Q6) — drive the virtual X desktop via `xdotool` / `scrot`.
//!
//! ## Always registered (2026-10-05 决策)
//!
//! These tools move the pointer and type into whatever window is focused on the
//! **Xvfb virtual display** (default `:99`, overridable via
//! `LINGMIAO_COMPUTER_USE_DISPLAY`) — never the host's real desktop. The group
//! is registered **unconditionally**: the previous `LINGMIAO_COMPUTER_USE=1`
//! opt-in gate was removed because this capability is a headline feature of
//! lingmiao. Stage whitelists (`stages.json`) still decide *where* the group is
//! reachable (目前仅「工作阶段」)。

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::tool::{Tool, ToolError, ToolOutput, ToolRegistry, json_schema};

/// Optional environment override for the target X display (需求④: the prefix is
/// `brand::env("COMPUTER_USE_DISPLAY")` = `LINGMIAO_COMPUTER_USE_DISPLAY`).
pub const ENV_DISPLAY: &str = "LINGMIAO_COMPUTER_USE_DISPLAY";
/// Default virtual display (matches the lingmiao Xvfb convention).
pub const DEFAULT_DISPLAY: &str = ":99";

fn display() -> String {
    std::env::var(ENV_DISPLAY).unwrap_or_else(|_| DEFAULT_DISPLAY.to_string())
}

fn have_bin(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_cmd(program: &str, args: &[String], dpy: &str) -> Result<String, ToolError> {
    let output = Command::new(program)
        .args(args)
        .env("DISPLAY", dpy)
        .output()
        .map_err(|e| ToolError::Other(format!("spawn {program}: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(ToolError::Other(format!(
            "{program} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

// ── args ───────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
struct EmptyArgs {}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScreenshotArgs {
    /// Base name for the PNG (saved under `.cache/lingmiao/tmp/computer_use/`).
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MoveArgs {
    /// Absolute X coordinate.
    x: i64,
    /// Absolute Y coordinate.
    y: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ClickArgs {
    /// Absolute X coordinate (omit to click at the current position).
    #[serde(default)]
    x: Option<i64>,
    /// Absolute Y coordinate (omit to click at the current position).
    #[serde(default)]
    y: Option<i64>,
    /// Mouse button: `left` (default) | `middle` | `right`.
    #[serde(default)]
    button: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TypeArgs {
    /// Text to type into the focused window.
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyArgs {
    /// Key or combination (e.g. `Return`, `ctrl+c`, `alt+F4`).
    key: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RunArgs {
    /// Shell command to type into the focused terminal and run.
    command: String,
}

// ── tool ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum CuOp {
    Status,
    Screenshot,
    Mousemove,
    Click,
    Type,
    Key,
    Run,
}

impl CuOp {
    fn name(self) -> &'static str {
        match self {
            CuOp::Status => "computer_use_status",
            CuOp::Screenshot => "computer_use_screenshot",
            CuOp::Mousemove => "computer_use_mousemove",
            CuOp::Click => "computer_use_click",
            CuOp::Type => "computer_use_type",
            CuOp::Key => "computer_use_key",
            CuOp::Run => "computer_use_run",
        }
    }

    fn description(self) -> &'static str {
        match self {
            CuOp::Status => "Report the virtual desktop display + available tooling.",
            CuOp::Screenshot => "Capture the virtual desktop to a PNG (returns its path).",
            CuOp::Mousemove => "Move the pointer to absolute (x, y).",
            CuOp::Click => "Click at (x, y) or the current position with a mouse button.",
            CuOp::Type => "Type text into the focused window.",
            CuOp::Key => "Send a key or key combination to the focused window.",
            CuOp::Run => "Type a shell command into the focused terminal and press Return.",
        }
    }

    fn parameters(self) -> Value {
        match self {
            CuOp::Status => json_schema::<EmptyArgs>(),
            CuOp::Screenshot => json_schema::<ScreenshotArgs>(),
            CuOp::Mousemove => json_schema::<MoveArgs>(),
            CuOp::Click => json_schema::<ClickArgs>(),
            CuOp::Type => json_schema::<TypeArgs>(),
            CuOp::Key => json_schema::<KeyArgs>(),
            CuOp::Run => json_schema::<RunArgs>(),
        }
    }
}

fn parse<T: for<'de> Deserialize<'de>>(tool: &str, args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::invalid(tool, e.to_string()))
}

/// One computer-use tool bound to a screenshot directory.
pub struct ComputerUseTool {
    op: CuOp,
    screenshots_dir: PathBuf,
}

#[async_trait]
impl Tool for ComputerUseTool {
    fn name(&self) -> &str {
        self.op.name()
    }
    fn description(&self) -> &str {
        self.op.description()
    }
    fn parameters(&self) -> Value {
        self.op.parameters()
    }
    fn is_mutating(&self) -> bool {
        !matches!(self.op, CuOp::Status)
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        // No opt-in gate: the group is registered unconditionally. The only
        // thing that can make an individual call fail is a missing backend
        // (Xvfb / xdotool / scrot), which is reported honestly by `status`.
        let dpy = display();
        match self.op {
            CuOp::Status => {
                let xdotool = have_bin("xdotool");
                let scrot = have_bin("scrot");
                let import = have_bin("import");
                let geometry = run_cmd("xdpyinfo", &[], &dpy)
                    .ok()
                    .and_then(|info| {
                        info.lines()
                            .find(|l| l.trim_start().starts_with("dimensions:"))
                            .map(|l| l.trim().to_string())
                    })
                    .unwrap_or_else(|| "(xdpyinfo unavailable)".to_string());
                Ok(ToolOutput::ok(format!(
                    "display={dpy}\n{geometry}\nxdotool={xdotool} scrot={scrot} import={import}"
                )))
            }
            CuOp::Screenshot => {
                let a: ScreenshotArgs = parse(self.op.name(), arguments)?;
                let name = if a.name.trim().is_empty() {
                    format!("shot-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"))
                } else {
                    a.name.trim().trim_end_matches(".png").to_string()
                };
                std::fs::create_dir_all(&self.screenshots_dir)
                    .map_err(|e| ToolError::io(self.screenshots_dir.display().to_string(), e))?;
                let path = self.screenshots_dir.join(format!("{name}.png"));
                let path_str = path.display().to_string();
                if have_bin("scrot") {
                    run_cmd("scrot", std::slice::from_ref(&path_str), &dpy)?;
                } else if have_bin("import") {
                    run_cmd(
                        "import",
                        &["-window".into(), "root".into(), path_str.clone()],
                        &dpy,
                    )?;
                } else {
                    return Err(ToolError::Other(
                        "no screenshot backend found (need scrot or ImageMagick `import`)".into(),
                    ));
                }
                Ok(ToolOutput::ok(path_str))
            }
            CuOp::Mousemove => {
                let a: MoveArgs = parse(self.op.name(), arguments)?;
                run_cmd(
                    "xdotool",
                    &["mousemove".into(), a.x.to_string(), a.y.to_string()],
                    &dpy,
                )?;
                Ok(ToolOutput::ok(format!("moved to ({}, {})", a.x, a.y)))
            }
            CuOp::Click => {
                let a: ClickArgs = parse(self.op.name(), arguments)?;
                let button = match a.button.as_str() {
                    "" | "left" => "1",
                    "middle" => "2",
                    "right" => "3",
                    other => other,
                };
                let mut args: Vec<String> = Vec::new();
                if let (Some(x), Some(y)) = (a.x, a.y) {
                    args.push("mousemove".into());
                    args.push(x.to_string());
                    args.push(y.to_string());
                }
                args.push("click".into());
                args.push(button.into());
                run_cmd("xdotool", &args, &dpy)?;
                Ok(ToolOutput::ok("clicked"))
            }
            CuOp::Type => {
                let a: TypeArgs = parse(self.op.name(), arguments)?;
                run_cmd("xdotool", &["type".into(), "--".into(), a.text], &dpy)?;
                Ok(ToolOutput::ok("typed"))
            }
            CuOp::Key => {
                let a: KeyArgs = parse(self.op.name(), arguments)?;
                run_cmd("xdotool", &["key".into(), a.key], &dpy)?;
                Ok(ToolOutput::ok("sent key"))
            }
            CuOp::Run => {
                let a: RunArgs = parse(self.op.name(), arguments)?;
                run_cmd("xdotool", &["type".into(), "--".into(), a.command], &dpy)?;
                run_cmd("xdotool", &["key".into(), "Return".into()], &dpy)?;
                Ok(ToolOutput::ok("command sent to focused terminal"))
            }
        }
    }
}

/// The seven computer-use tool names.
pub const TOOL_NAMES: [&str; 7] = [
    "computer_use_status",
    "computer_use_screenshot",
    "computer_use_mousemove",
    "computer_use_click",
    "computer_use_type",
    "computer_use_key",
    "computer_use_run",
];

/// Register the computer-use group (unconditional — 2026-10-05 决策 removed the
/// opt-in env gate).
pub fn register_all(registry: &mut ToolRegistry, screenshots_dir: impl AsRef<Path>) {
    let dir = screenshots_dir.as_ref().to_path_buf();
    for op in [
        CuOp::Status,
        CuOp::Screenshot,
        CuOp::Mousemove,
        CuOp::Click,
        CuOp::Type,
        CuOp::Key,
        CuOp::Run,
    ] {
        registry.register(ComputerUseTool {
            op,
            screenshots_dir: dir.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_display_follows_the_brand_prefix() {
        // 需求④: the env name must track `brand::env(...)`, never be stale.
        assert_eq!(
            ENV_DISPLAY,
            lingmiao_core::brand::env("COMPUTER_USE_DISPLAY")
        );
        assert_eq!(DEFAULT_DISPLAY, ":99");
    }

    #[test]
    fn register_all_registers_the_seven_tools_unconditionally() {
        // 2026-10-05 决策: no env gate — the group is always registered.
        let mut reg = ToolRegistry::new();
        register_all(&mut reg, std::env::temp_dir());
        assert_eq!(reg.len(), TOOL_NAMES.len());
        for n in TOOL_NAMES {
            assert!(reg.contains(n), "missing {n}");
        }
    }

    #[tokio::test]
    async fn status_reports_the_display_and_backends() {
        // `status` never needs a live X server: it probes tooling and falls back
        // to "(xdpyinfo unavailable)" when the display is unreachable.
        let mut reg = ToolRegistry::new();
        register_all(&mut reg, std::env::temp_dir());
        let out = reg
            .execute("computer_use_status", serde_json::json!({}))
            .await
            .expect("status must not fail");
        assert!(out.content.contains("display="), "got: {}", out.content);
        assert!(out.content.contains("xdotool="), "got: {}", out.content);
    }
}
