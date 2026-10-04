//! # lingmiao-tui — terminal UI (需求① · §12 纯 CC 单栏)
//!
//! One `tokio` task runs the input loop, the engine pipeline, and the render
//! loop together, with streamed tokens delivered over the shared [`EventBus`]
//! (decision Q2/Q3 — UI and QL share one event loop, no cross-thread
//! callbacks).
//!
//! Layout is the single-column shape from `docs/ux-design.md §12/§18` (撤侧栏):
//! the CC-style conversation main area (the brand identity is printed once into
//! it at startup, CC-style — there is **no** permanent header, cli 2026-09-24),
//! the **whiteboard band** above the input box (cli 2026-09-24 ③), a multiline
//! input box, and a **2-line footer** (status · session tokens / context
//! composition with the identity at the bottom-right; the shortcut-hint row was
//! dropped — cli 2026-09-24 ①), drawn into a full-screen alternate buffer. Each
//! submitted line
//! runs through the full M4 pipeline ([`Engine::run_turn`]: `组织上下文 →
//! 工作阶段 → 沉淀阶段`); Esc aborts an in-flight turn; the mouse wheel pages the
//! conversation (§11); function navigation is `/help`, `/board`, `/memory`,
//! `/session`, `/tools`, `/model`, `/clear`, `/quit`, and ↑↓ browse submitted-input
//! history (纯键盘，§11).
//!
//! Un-submitted input survives a crash: the queue + input-box draft are
//! snapshotted into the volatile cache dir and read back on startup
//! ([`session_input`], cli 2026-09-28「刚才队列里的提示词还能看到吗？」).

#![forbid(unsafe_code)]

pub mod app;
pub mod clipboard;
pub mod editor;
pub mod markdown;
pub mod motion;
pub mod session_input;
pub mod theme;
pub mod wrap;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyEventKind,
};
use crossterm::execute;
use futures_util::{FutureExt, StreamExt};
use lingmiao_core::{EventBus, LingmiaoError, Paths, brand};
use lingmiao_engine::{AutonomousLoop, Engine, RoundOutcome, SummaryReport};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use app::{Action, App};

/// Set when a panic was intercepted since the last check; the run loop reads it
/// to force one full repaint and show the message in the transcript.
static PANIC_SEEN: AtomicBool = AtomicBool::new(false);
/// The most recent intercepted panic message (set by the hook, taken by the run
/// loop). `std::sync::Mutex` — the hook must not allocate a runtime handle.
static PANIC_MSG: Mutex<Option<String>> = Mutex::new(None);

/// Take the intercepted panic message, if one arrived since the last call.
pub fn take_panic() -> Option<String> {
    if !PANIC_SEEN.swap(false, Ordering::SeqCst) {
        return None;
    }
    PANIC_MSG.lock().ok().and_then(|mut m| m.take())
}

/// Install the TUI's **panic hook** (cli 2026-09-28 ②「运行中也会有UI突然变乱」).
///
/// A panic inside a background task is the one way the screen can corrupt itself
/// mid-session: `ratatui::init` installs a hook whose whole job is
/// `restore()` — leaving raw mode and the alternate screen — which is exactly
/// right when the process is dying, and exactly wrong when a *spawned task*
/// panics while the UI keeps drawing (the TUI would suddenly paint over the
/// terminal's normal screen). The default hook is worse still: it prints the
/// message to **stderr**, i.e. into the alternate screen, where ratatui's
/// incremental repaint never clears it — the same mechanism as the MCP-stderr
/// first-frame bug (§35).
///
/// So 灵妙 replaces both: the panic is logged (`tracing::error!`, into
/// `.cache/lingmiao/logs/`), recorded for the UI, and the terminal is left
/// **untouched** — the run loop notices [`take_panic`] on its next tick, does one
/// `terminal.clear()` (full redraw) and reports it as an error block in the
/// transcript, so a hidden failure is never invisible (cli 2026-09-27
///「即使未来没有发现的错误，也应该在UI上看得出来」). Panics on the UI thread itself
/// still end the session — but through [`run`]'s own teardown, which restores the
/// terminal exactly once.
///
/// Public so the `panic_probe` example can exercise the hidden-failure contract
/// against a real terminal (the hook is otherwise installed by [`run`] alone).
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "（无消息）".to_string(),
            },
        };
        // The panic site (`app.rs:123:4`) — the task name alone is not enough to
        // find a hidden failure.
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let full = if at.is_empty() {
            msg
        } else {
            format!("{msg} @ {at}")
        };
        tracing::error!(panic = %full, "TUI panic intercepted（终端不还原，改由 UI 报告）");
        if let Ok(mut slot) = PANIC_MSG.lock() {
            *slot = Some(full);
        }
        PANIC_SEEN.store(true, Ordering::SeqCst);
    }));
}

/// Run the TUI until the user quits.
pub async fn run(engine: Arc<Engine>, bus: Arc<EventBus>) -> io::Result<()> {
    // The volatile cache `tmp/` dir — where the un-submitted-input snapshot lives
    // ([`session_input`]). Resolved once here and handed to the loop.
    let tmp_dir = Paths::detect().map(|p| p.tmp_dir).unwrap_or_else(|_| {
        std::env::temp_dir()
            .join(brand::CACHE_DIR)
            .join(brand::BIN)
            .join("tmp")
    });
    let mut terminal = ratatui::init();
    install_panic_hook();
    // Mouse capture (cli 2026-09-21 滚轮 · cli 2026-09-27 拖选): without it the
    // terminal turns the wheel into ↑/↓ keys, which the input box swallows
    // ("一滚动就是输入栏，出不去了"). Capturing the mouse also means the terminal's
    // own drag-select never fires, so the app implements text selection itself
    // ([`App::on_mouse`]); the wheel pages the conversation and a left-drag
    // selects/copies (§11 增补).
    let _ = execute!(io::stdout(), EnableMouseCapture);
    // **Bracketed paste** (cli 2026-09-28「默认支持 win 和 linux 的复制粘贴」):
    // ratatui's `init` enables neither, and without this the terminal sends a
    // paste as *raw keystrokes* — so a pasted multi-line block arrives as
    // individual characters and every newline submits a turn. Enabling it makes
    // the terminal wrap a paste in `\x1b[200~ … \x1b[201~` and crossterm deliver
    // one [`TermEvent::Paste`], which [`App::paste`] inserts atomically. (The
    // handler for that event existed since §6 痛点⑤ but the mode was never turned
    // on, so it was dead code.)
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    // A panic on the UI thread itself unwinds through the loop; catch it here so
    // the terminal is still restored below, exactly once (the hook deliberately
    // does not restore — see [`install_panic_hook`]).
    let result = match std::panic::AssertUnwindSafe(run_loop(&mut terminal, engine, bus, tmp_dir))
        .catch_unwind()
        .await
    {
        Ok(r) => r,
        Err(_) => Ok(()),
    };
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Read the whiteboard's current page for the band above the input box (§18 ③,
/// best-effort).
///
/// The whiteboard lives at `<root>/.memory/whiteboard/pages.json` (需求③). The
/// TUI reads it directly — no engine plumbing — and degrades to empty when the
/// file is missing or malformed.
fn whiteboard_lines(root: &Path) -> Vec<String> {
    let path = root
        .join(brand::MEMORY_DIR)
        .join("whiteboard")
        .join("pages.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let pages = v
        .get("pages")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    if pages.is_empty() {
        return Vec::new();
    }
    let cur = v.get("current_page").and_then(|c| c.as_u64()).unwrap_or(1) as usize;
    let idx = cur.saturating_sub(1).min(pages.len() - 1);
    let page = &pages[idx];
    let title = page.get("title").and_then(|t| t.as_str()).unwrap_or("Page");
    let content = page.get("content").and_then(|c| c.as_str()).unwrap_or("");
    let mut lines = vec![format!("[{cur}] {title}")];
    if content.trim().is_empty() {
        lines.push("(空白)".to_string());
    } else {
        for l in content.lines() {
            lines.push(l.to_string());
        }
    }
    lines
}

/// Spawn one turn on its own task and forward the result to `done_tx`.
///
/// Robustness (cli 2026-09-27: 「即使未来没有发现的错误，也应该在UI上看得出来」):
/// a panic anywhere inside `run_turn` would otherwise kill the task silently —
/// `done_tx.send` would never run, the UI would stay "busy" forever and the user
/// would see a frozen screen with no explanation. `catch_unwind` converts the
/// panic into an ordinary [`LingmiaoError`] the run loop can display.
fn spawn_turn(
    engine: Arc<Engine>,
    done_tx: mpsc::Sender<Result<SummaryReport, LingmiaoError>>,
    text: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(engine.run_turn(&text))
            .catch_unwind()
            .await
            .map_err(|_| LingmiaoError::fatal("回合内部 panic（详见日志）"))
            .and_then(std::convert::identity);
        let _ = done_tx.send(result).await;
    })
}

/// Start an **autonomous session** (`/auto <目标>`) on its own task.
///
/// 原版把自动模式跑在 `threading.Thread` 上；Rust 侧是 `tokio::spawn` ——
/// 全栈单事件循环（Q3），所以 Main / Auditor 的每一条进度（`Event::AutoNotice`）
/// 与它们内部的工具卡 / token 流都自动汇入**同一份**事件流，TUI 照常渲染。
///
/// 引擎侧构造一个 [`AutonomousLoop`]（会话结束即丢弃；`autonomous.db` 与
/// 运行中的会话登记表都在它内部），`done_tx` 回传终态会话 id。
/// Esc 停自动模式用的协作句柄（会话 id + 编排器）。
struct AutoCtl {
    loop_: Arc<AutonomousLoop>,
    session_id: String,
}

fn spawn_auto(
    engine: Arc<Engine>,
    bus: Arc<EventBus>,
    done_tx: mpsc::Sender<Result<String, LingmiaoError>>,
    ctl: Arc<Mutex<Option<AutoCtl>>>,
    goal: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let paths = Paths::at(&root);
        let store_path = AutonomousLoop::db_path(&paths);
        let store = match lingmiao_engine::autonomous::store::AutonomousStore::open(store_path) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = done_tx.send(Err(e)).await;
                return;
            }
        };
        // 角色的 LLM 与默认 client 同源（原版按角色覆盖模型，Rust 侧暂未接）。
        let loop_ = AutonomousLoop::new(
            &root,
            engine.config().clone(),
            bus,
            engine.llm().clone(),
            store,
            engine.memory().cloned(),
        );
        let loop_ = Arc::new(loop_);
        let request = lingmiao_engine::autonomous::run::parse_request(&goal);
        let result = async {
            let session = loop_.start(request).await?;
            // 登记协作句柄，Esc 可 `stop()` 它（走正常收尾）。
            if let Ok(mut g) = ctl.lock() {
                *g = Some(AutoCtl {
                    loop_: loop_.clone(),
                    session_id: session.id.clone(),
                });
            }
            let session = if session.status == lingmiao_engine::SessionStatus::Error {
                session
            } else {
                loop_.run_session(session).await?
            };
            Ok::<_, LingmiaoError>(format!("{} · {} 轮", session.id, session.current_iteration))
        }
        .await;
        let _ = done_tx.send(result).await;
    })
}

/// Run one **single-round audit** (`/round <目标>`) on its own task.
fn spawn_round(
    engine: Arc<Engine>,
    bus: Arc<EventBus>,
    done_tx: mpsc::Sender<Result<RoundOutcome, LingmiaoError>>,
    goal: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let paths = Paths::at(&root);
        let store_path = AutonomousLoop::db_path(&paths);
        let store = match lingmiao_engine::autonomous::store::AutonomousStore::open(store_path) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = done_tx.send(Err(e)).await;
                return;
            }
        };
        let loop_ = AutonomousLoop::new(
            &root,
            engine.config().clone(),
            bus,
            engine.llm().clone(),
            store,
            engine.memory().cloned(),
        );
        let _ = done_tx.send(loop_.run_single_round(&goal).await).await;
    })
}

async fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    engine: Arc<Engine>,
    bus: Arc<EventBus>,
    tmp_dir: std::path::PathBuf,
) -> io::Result<()> {
    let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // The identity shows the model that *answers* — the `工作阶段` route when
    // `config.json` declares one, else the default (cli 2026-09-27).
    let chat = engine.llm_for(lingmiao_engine::STAGE_C);
    let mut app = App::new(chat.model().to_string(), chat.group_id().to_string());
    app.set_whiteboard(whiteboard_lines(&root));
    // CC prints its brand block once, at startup, into the conversation (cli
    // 2026-09-24) — 灵妙 does the same instead of carrying a permanent header.
    app.push_startup_banner();
    // When `config.json` routes stages to their own models/APIs, say so once at
    // startup — otherwise a stage's extra provider is invisible (cli 2026-09-27).
    let routes = engine.stage_routes();
    if !routes.is_empty() {
        app.push_notice(&format!("阶段模型路由：{}", routes.join(" · ")));
    }
    // **Crash recovery** (cli 2026-09-28「刚才队列里的提示词还能看到吗？进程突然被杀
    // 死了」): the queue + draft are mirrored into the volatile cache on every change
    // (`persist_input`), so a `kill -9` no longer evaporates what the user had
    // lined up. Restore it here, before the first frame, and **say so** — text
    // appearing in the box out of nowhere is exactly the sort of unexplained
    // state that must never be silent.
    if let Some(snapshot) = session_input::load(&tmp_dir) {
        let queued = snapshot.queue.len();
        app.restore_unsubmitted(&snapshot);
        if queued > 0 {
            app.push_notice(&format!(
                "已恢复上次未提交的输入：{queued} 条排队消息已放回队列"
            ));
        } else {
            app.push_notice("已恢复上次未提交的输入（输入框草稿）");
        }
    }
    // The last snapshot written, so the mirror below only touches the disk when
    // the un-submitted input actually changed (the loop ticks 10×/s).
    let mut last_input = app.unsubmitted();

    let mut term_events = EventStream::new();
    let mut bus_rx = bus.subscribe();
    let (done_tx, mut done_rx) = mpsc::channel::<Result<SummaryReport, LingmiaoError>>(4);
    let mut turn_task: Option<JoinHandle<()>> = None;
    // 自动模式 / 单轮审计各有自己的完成通道与任务句柄（与聊天回合并行）。
    let (auto_tx, mut auto_rx) = mpsc::channel::<Result<String, LingmiaoError>>(4);
    let mut auto_task: Option<JoinHandle<()>> = None;
    // Esc 停自动模式的协作句柄（start() 拿到 session 后登记）。
    let auto_ctl: Arc<Mutex<Option<AutoCtl>>> = Arc::new(Mutex::new(None));
    let (round_tx, mut round_rx) = mpsc::channel::<Result<RoundOutcome, LingmiaoError>>(4);
    let mut round_task: Option<JoinHandle<()>> = None;
    // Repaint ~10×/s: keeps the clock live *and* animates the running-tool
    // braille spinner smoothly (CC/lingmiao use a 0.1s frame).
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // A background task panicked: its message is on the transcript, but the
        // foreign bytes it may have written can only be cleared by a **full**
        // repaint (ratatui's incremental `draw` touches its own dirty cells only
        // — §35's「拖动窗口才变好」). `clear()` resets the back buffer, so the very
        // next `draw` redraws every cell.
        if let Some(msg) = take_panic() {
            terminal.clear()?;
            app.push_error_detail(&format!("后台任务 panic：{msg}"));
        }
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            _ = ticker.tick() => {}

            maybe = term_events.next() => match maybe {
                Some(Ok(TermEvent::Key(k))) if k.kind != KeyEventKind::Release => {
                    match app.on_key(k) {
                        Action::Submit(text) => {
                            // 自动模式在跑时**绝不**起普通回合：两路 stage 流会并发
                            // 写同一份思考 / 正文缓冲，`commit_thinking()` 把对方的
                            // 半截 reasoning 落成 1~2 词碎片（cli 2026-10-04 两张
                            // 截图），而 `begin_turn` 还会清空缓冲 —— 即「像清空了
                            // 继续」。排进队列，等自动模式结束再发（cli 2026-10-04
                            //「插入只能是在不影响原内容的前提下」）。
                            if auto_task.is_some() || round_task.is_some() {
                                app.enqueue_turn(text);
                            } else {
                                turn_task = Some(spawn_turn(engine.clone(), done_tx.clone(), text));
                            }
                        }
                        Action::Cancel => {
                            if let Some(handle) = turn_task.take() {
                                handle.abort();
                            }
                            // 自动模式 / 单轮审计在跑时，Esc 是**立即**停（cli
                            // 2026-10-04「自动模式按 esc 直接退出，不要等待」）。
                            //
                            // v0.12.41 走的是**协作式** `stop()`：置 `AtomicBool`，
                            // 引擎在**轮首**才看到，UI 只提示「当前轮结束后收尾」——
                            // 用户按下 Esc 却要继续等一次 Main+Auditor 往返（可能几
                            // 分钟），屏幕还在刷。那不是「停」，是「排队等停」。
                            //
                            // 现在：先尽力标记引擎停止位（万一恰好落在轮首检查点，
                            // 会话走正常收尾而不是凭空消失），再 **abort** 后台任务，
                            // 屏幕**当场**停下。
                            //
                            // 代价（如实告知用户）：abort 跳过引擎收尾 —— 报告 /
                            // 轨迹 / 会话终态不落盘，`autonomous.db` 里那条会话停在
                            // `running`。这是「不等待」的必然代价：正常收尾要么等当前
                            // 轮，要么就没有。worktree 里的代码改动**已经落盘**（工具
                            // 是同步写文件的），不会因 abort 丢失。
                            if auto_task.is_some() || round_task.is_some() {
                                let stopped = match auto_ctl.lock() {
                                    Ok(g) => g
                                        .as_ref()
                                        .map(|c| {
                                            c.loop_.stop(&c.session_id);
                                            true
                                        })
                                        .unwrap_or(false),
                                    Err(_) => false,
                                };
                                if stopped {
                                    app.push_info(&["· 已停止自动模式（立即退出，会话未走收尾）"]);
                                } else {
                                    app.push_info(&["· 已停止（立即退出，会话未走收尾）"]);
                                }
                                if let Some(handle) = auto_task.take() {
                                    handle.abort();
                                }
                                if let Some(handle) = round_task.take() {
                                    handle.abort();
                                }
                                if let Ok(mut g) = auto_ctl.lock() {
                                    *g = None;
                                }
                                // 收尾这一回合的流式缓冲（**不**打印「已取消进行中
                                // 的回合」—— 用户停的是自动模式，不是一条普通回合）。
                                app.end_auto_turn();
                            } else {
                                app.on_cancel();
                            }
                            // 队列只在**没有任何自动模式任务在跑**时才排空：否则排出来
                            // 的普通回合会与 Auto·Main 并发（碎片 + 标题闪烁），且
                            // `begin_turn` 清空缓冲 —— 正是「清空了继续」。
                            if auto_task.is_none() && round_task.is_none() {
                                if let Some(text) = app.dequeue_turn() {
                                    turn_task =
                                        Some(spawn_turn(engine.clone(), done_tx.clone(), text));
                                }
                            }
                        }
                        // ctrl+x ctrl+s (CC): drop the in-flight turn and send
                        // the front queued message immediately.
                        Action::SendQueuedNow => {
                            if let Some(handle) = turn_task.take() {
                                handle.abort();
                            }
                            // 自动模式在跑时不「立即发送」：那会起一个与 Auto·Main
                            // 并发的普通回合（同一份缓冲 → 碎片），而自动模式本身
                            // 不能被一条插入消息打断（cli 2026-10-04）。
                            if auto_task.is_some() || round_task.is_some() {
                                app.push_info(&["· 自动模式进行中，排队消息将在其结束后发送"]);
                            } else {
                                app.on_interrupt();
                                if let Some(text) = app.dequeue_turn() {
                                    turn_task =
                                        Some(spawn_turn(engine.clone(), done_tx.clone(), text));
                                }
                            }
                        }
                        // `/clear` — 原版对齐: the command wipes the **rendered
                        // panel only** (already done inside `App::slash`); the
                        // engine holds no conversation state to clear and the #1
                        // archive is deliberately left alone.
                        Action::Clear => {}
                        // The explicit copy shortcut (`ctrl+y` / `shift+insert`) or
                        // `ctrl+c` with a live selection, applied to that selection
                        // (cli 2026-09-27 / 2026-09-28).
                        Action::Copy(text) => {
                            let n = text.chars().count();
                            if let Err(e) = clipboard::copy(&text) {
                                app.push_notice(&format!("复制失败：{e}"));
                            } else {
                                app.set_copy_note(format!("已复制 {n} 字符"));
                            }
                        }
                        // Paste from the OS clipboard (cli 2026-09-28「默认支持 win
                        // 和 linux 的复制粘贴」): reading the clipboard is I/O, so it
                        // happens here and the text goes back into the (pure) `App`.
                        // An empty clipboard is silently ignored rather than an
                        // error — nothing to insert is the common case.
                        Action::Paste => match clipboard::paste() {
                            Ok(text) if !text.is_empty() => app.paste(&text),
                            Ok(_) => {}
                            Err(e) => app.push_notice(&format!("粘贴不可用：{e}")),
                        },
                        Action::Stats => match engine.memory() {
                            Some(m) => {
                                let archive = m.archive.count().unwrap_or(0);
                                let observations = m.observations.stats().unwrap_or(0);
                                let (nodes, edges) = m
                                    .knowledge
                                    .stats()
                                    .map(|g| (g.nodes, g.edges))
                                    .unwrap_or((0, 0));
                                app.push_stats(archive, observations, nodes, edges);
                            }
                            None => app.push_notice("记忆层不可用"),
                        },
                        // `/auto <目标>` — 自动模式（Main+Auditor 循环）。
                        Action::Auto(goal) => {
                            if let Some(h) = turn_task.take() {
                                h.abort();
                            }
                            // 重复 `/auto`：旧句柄必须 abort —— `JoinHandle` 的 drop
                            // 是 detach，不是取消，覆盖成新句柄会让旧会话继续跑
                            // （两条 auto 流写同一份缓冲，cli 2026-10-04 的闪烁）。
                            if let Some(h) = auto_task.take() {
                                h.abort();
                            }
                            app.begin_auto_turn(&goal);
                            auto_task = Some(spawn_auto(
                                engine.clone(),
                                bus.clone(),
                                auto_tx.clone(),
                                auto_ctl.clone(),
                                goal,
                            ));
                        }
                        // `/round <目标>` — 单轮审计。
                        Action::Round(goal) => {
                            if let Some(h) = turn_task.take() {
                                h.abort();
                            }
                            if let Some(h) = round_task.take() {
                                h.abort();
                            }
                            app.begin_auto_turn(&goal);
                            round_task = Some(spawn_round(
                                engine.clone(),
                                bus.clone(),
                                round_tx.clone(),
                                goal,
                            ));
                        }
                        Action::Quit | Action::None => {}
                    }
                }
                // A resize is the one moment the pane width changes under the
                // **pre-wrapped** transcript (cli 2026-09-28 ②「运行中也会有UI突然
                // 变乱」). ratatui's `autoresize` → `resize` already resizes both
                // buffers **and clears** them when the geometry actually changed
                // (`terminal.rs::resize` ends in `self.clear()`), so the next frame
                // is a full repaint — no fragments wrapped for the old width can
                // survive. The other half of the narrow-pane garble was ours: the
                // committed cache and the live blocks must be wrapped at the *same*
                // width (`render_conversation`), or one card's halves disagree.
                Some(Ok(TermEvent::Resize(..))) => terminal.autoresize()?,
                // Bracketed paste (§6 痛点⑤): insert the block into the editor.
                Some(Ok(TermEvent::Paste(text))) => app.paste(&text),
                // Mouse (cli 2026-09-21 滚轮 / cli 2026-09-27 拖选): the app owns
                // the meaning (it knows the pane layout + transcript) — paging and
                // selection both stay internal, with **no** outside effect. A
                // drag only *selects* now; the clipboard write happens on the
                // explicit copy shortcut (`ctrl+y`), so a release never overrides
                // the user's selection (cli 2026-09-27). The terminal's own
                // drag-select is unavailable because the TUI captures the mouse
                // (§11 / §16.3).
                Some(Ok(TermEvent::Mouse(m))) => app.on_mouse(m),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },

            received = bus_rx.recv() => match received {
                Ok(event) => {
                    app.on_event(&event);
                    // Drain the whole backlog before redrawing: a C-stage turn
                    // emits thousands of `LlmDelta` chunks (reasoning + content)
                    // far faster than the 100ms repaint tick, so consuming one
                    // event per loop iteration would let the broadcast ring lag
                    // and drop the reasoning stream (T1 修 reasoning 丢包).
                    while let Ok(ev) = bus_rx.try_recv() {
                        app.on_event(&ev);
                    }
                }
                // Reveal dropped events instead of swallowing them silently.
                Err(RecvError::Lagged(n)) => {
                    app.push_notice(&format!("事件积压，已丢弃 {n} 条（不影响最终结果）"));
                }
                Err(RecvError::Closed) => break,
            },

            Some(result) = auto_rx.recv() => {
                auto_task = None;
                if let Ok(mut g) = auto_ctl.lock() {
                    *g = None;
                }
                match result {
                    Ok(summary) => app.push_info(&[format!("· 自动模式结束：{summary}").as_str()]),
                    Err(e) => app.push_error(&format!("自动模式失败：{e}")),
                }
                app.end_auto_turn();
                // 自动模式收尾后才发排队的普通回合：插入的对话追加在自动模式的
                // 内容**之后**，而不是清空它（cli 2026-10-04）。
                if round_task.is_none() {
                    if let Some(text) = app.dequeue_turn() {
                        turn_task = Some(spawn_turn(engine.clone(), done_tx.clone(), text));
                    }
                }
            }

            Some(result) = round_rx.recv() => {
                round_task = None;
                match result {
                    Ok(o) => app.push_info(&[
                        format!(
                            "· 单轮审计结束：{}（{} 次尝试）",
                            if o.passed { "Auditor 通过" } else { "未通过" },
                            o.retries
                        )
                        .as_str(),
                    ]),
                    Err(e) => app.push_error(&format!("单轮审计失败：{e}")),
                }
                app.end_auto_turn();
                if auto_task.is_none() {
                    if let Some(text) = app.dequeue_turn() {
                        turn_task = Some(spawn_turn(engine.clone(), done_tx.clone(), text));
                    }
                }
            }

            Some(result) = done_rx.recv() => {
                turn_task = None;
                app.on_turn_done(result);
                // The agent may have written to the whiteboard — refresh it.
                app.set_whiteboard(whiteboard_lines(&root));
                // Drain one queued message (cli 2026-09-24 ③, CC queue): a
                // mid-turn Enter queues, and the queue sends once the turn ends.
                // **Not while an auto session runs** — the queued turn would race
                // the running Auto·Main over one shared think/answer buffer (cli
                // 2026-10-04); it goes out from the `auto_rx` branch instead.
                if auto_task.is_none() && round_task.is_none() {
                    if let Some(text) = app.dequeue_turn() {
                        turn_task = Some(spawn_turn(engine.clone(), done_tx.clone(), text));
                    }
                }
            }
        }

        // Mirror the un-submitted input (queue + draft) into the volatile cache.
        // Doing it here — at the single point every branch funnels through —
        // means no future edit to a key handler can forget it (cli 2026-09-28
        //「刚才队列里的提示词还能看到吗？进程突然被杀死了」). Best-effort: a failure is
        // logged, never surfaced as a turn error.
        let input = app.unsubmitted();
        if input != last_input {
            if let Err(e) = session_input::save(&tmp_dir, &input) {
                tracing::warn!(error = %e, "未提交输入快照写入失败");
            }
            last_input = input;
        }

        if app.should_quit() {
            break;
        }
    }

    // Abort any still-running turn so it cannot write after the terminal is
    // restored.
    if let Some(handle) = turn_task.take() {
        handle.abort();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panic plumbing the run loop relies on (cli 2026-09-28 ②「运行中也会有 UI
    /// 突然变乱」): a recorded panic is delivered **once**, and the flag is cleared
    /// so the loop does not clear-and-repaint on every subsequent frame.
    #[test]
    fn a_recorded_panic_is_reported_exactly_once() {
        // Nothing recorded yet.
        assert_eq!(take_panic(), None);
        // The hook's two side effects, applied here directly (a real panic would
        // abort the test harness's own stderr plumbing).
        *PANIC_MSG.lock().unwrap() = Some("boom @ src/x.rs:1".to_string());
        PANIC_SEEN.store(true, Ordering::SeqCst);
        assert_eq!(take_panic().as_deref(), Some("boom @ src/x.rs:1"));
        // Taken once — the next tick must not clear the screen again.
        assert_eq!(take_panic(), None);
    }
}
