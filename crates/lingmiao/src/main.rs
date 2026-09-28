//! Single binary — M1 boot + TUI.
//!
//! Decision Q3: no subcommands — a bare `lingmiao` boots the infrastructure, builds
//! the conversation engine, and hands the terminal to the ratatui UI. The whole
//! process runs on one `tokio` runtime so the UI and the QL share a single event
//! loop (Q2), with streamed tokens flowing over the event bus.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use lingmiao_core::events::Event;
use lingmiao_core::{Config, EventBus, Paths, brand};
use lingmiao_engine::Engine;

#[tokio::main]
async fn main() -> ExitCode {
    // 1. Paths — CC-style layout rooted at the working directory (需求③:
    //    `.memory/` for durable assets, `.cache/lingmiao/` for volatile ones).
    let paths = match Paths::detect() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}: cannot resolve working directory: {e}", brand::BIN);
            return ExitCode::FAILURE;
        }
    };

    // 2. Environment — global API keys (`.env` resolution).
    lingmiao_core::envkeys::load_env(Some(&paths.root));

    // 3. Directories + memory whitelist cleanup + empty DBs.
    if let Err(e) = paths.prepare() {
        eprintln!(
            "{}: cannot prepare {}: {e}",
            brand::BIN,
            paths.memory_dir.display()
        );
        return ExitCode::FAILURE;
    }

    // 4. Logging — dual channel (jsonl + log).
    let logs = match lingmiao_core::logging::init(&paths.logs_dir) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}: cannot initialise logging: {e}", brand::BIN);
            return ExitCode::FAILURE;
        }
    };

    // 5. Config — embedded defaults, external override via the config-dir env var.
    let cfg = match Config::discover() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: invalid configuration: {e}", brand::BIN);
            return ExitCode::FAILURE;
        }
    };

    tracing::info!(
        version = lingmiao_core::VERSION,
        root = %paths.root.display(),
        "{} starting",
        brand::SLUG
    );

    // 6. Event bus — 13-variant protocol, log + fan-out (+ broadcast to the TUI).
    let bus = Arc::new(EventBus::default());
    bus.push(Event::StageStarted {
        stage: "boot".to_string(),
        ts: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
    });

    // 7. Engine — needs a provider API key.
    let engine = match Engine::from_env(bus.clone(), &cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}: cannot start engine: {e}", brand::BIN);
            eprintln!(
                "  hint: set DEEPSEEK_API_KEY, or edit models.json / set LLM_PROVIDER + that group's *_API_KEY."
            );
            return ExitCode::FAILURE;
        }
    };

    // 8. TUI — requires a real terminal.
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "{}: interactive TUI requires a terminal (stdout is not a tty)\n\
             \x20     config: {} stages, {} prompts, {} mcp servers; model {}/{}",
            brand::BIN,
            cfg.stages().len(),
            cfg.prompts().len(),
            cfg.mcp_servers().len(),
            engine.llm().group_id(),
            engine.llm().model(),
        );
        return ExitCode::FAILURE;
    }

    // 7b. ⑤ MCP 接上 (**non-blocking**) — start connecting the declared
    //     `mcp.json` servers in the background and hand the terminal straight to
    //     the TUI; their tools register as each server finishes (CC's lazy,
    //     non-blocking connect). A slow or hung server can no longer delay boot.
    let _mcp_attach = engine.spawn_mcp_attach();

    let engine = Arc::new(engine);

    eprintln!(
        "{} (Rust) v{} — {}/{} — logs: {}",
        brand::NAME,
        lingmiao_core::VERSION,
        engine.llm().group_id(),
        engine.llm().model(),
        logs.jsonl.display()
    );
    tracing::info!("{} boot complete", brand::SLUG);

    match lingmiao_tui::run(engine, bus).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: TUI error: {e}", brand::BIN);
            ExitCode::FAILURE
        }
    }
}
