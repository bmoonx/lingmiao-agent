//! Render a scripted turn into a `TestBackend` and print it as text.
//!
//! A quick way to eyeball the CC-style conversation (§4: `❯` user, `∴`
//! reasoning, `●` reply, `● name(args)` + `⎿ result` tool cards, `◜` tail)
//! without a real terminal / API key:
//!
//! ```sh
//! cargo run -p lingmiao-tui --example preview
//! ```
//!
//! **This is a scripted sample, not real pipeline output.** The *shapes* it
//! feeds mirror the real producers so the demo doesn't lie about the contract:
//! the six context segments below are exactly what `lingmiao-engine`'s
//! `context_sections()` (§8.5) emits for a `工作阶段` turn; a real run fills in
//! the measured `chars`/tokens.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lingmiao_core::events::Event;
use lingmiao_tui::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn main() {
    let mut app = App::new("deepseek-v4-pro", "deepseek");
    app.set_whiteboard(vec![
        "需求：CC 式对话区".into(),
        "- ✅ 思考 ∴".into(),
        "- ✅ 工具 ● ⎿".into(),
        "- ⬜ 实拍验证".into(),
    ]);

    // User turn.
    for c in "帮我看下 src/main.rs".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Enter));

    // Reasoning (∴).
    app.on_event(&Event::LlmDelta {
        stage: "工作阶段".into(),
        kind: "reasoning".into(),
        text: "用户想看入口文件，先读一下 src/main.rs 再说。".into(),
    });

    // Tool call: running (⠋) then finished (●/⎿).
    app.on_event(&Event::ToolStarted {
        stage: "工作阶段".into(),
        tool: "read_file".into(),
        args: json!({"file_path": "src/main.rs"}),
        call_id: "c1".into(),
    });
    app.on_event(&Event::ToolCalled {
        stage: "工作阶段".into(),
        tool: "read_file".into(),
        args: json!({"file_path": "src/main.rs"}),
        result_preview: "fn main() {\n    println!(\"你好\");\n}".into(),
        ms: 4.3,
        error: false,
        call_id: "c1".into(),
        // Non-file tool: no patch.
        diff: Value::Array(vec![]),
    });

    // A file mutation shows its red/green unified diff (cli 2026-09-28
    //「参照 CC 实现代码改动时的红绿对比格式显示样式」) — the same `kind`/`text`
    // rows `lingmiao_tools::diff::unified_diff` puts on `Event::ToolCalled.diff`.
    app.on_event(&Event::ToolStarted {
        stage: "工作阶段".into(),
        tool: "edit".into(),
        args: json!({"file_path": "src/main.rs"}),
        call_id: "c2".into(),
    });
    app.on_event(&Event::ToolCalled {
        stage: "工作阶段".into(),
        tool: "edit".into(),
        args: json!({"file_path": "src/main.rs"}),
        result_preview: "Edited src/main.rs (1 replacement)".into(),
        ms: 3.1,
        error: false,
        call_id: "c2".into(),
        diff: json!([
            {"kind": "hunk", "text": "@@ -1,4 +1,4 @@"},
            {"kind": "context", "text": "fn main() {"},
            {"kind": "remove", "text": "    println!(\"hello\");"},
            {"kind": "add", "text": "    println!(\"你好\");"},
            {"kind": "context", "text": "}"},
        ]),
    });

    // Answer (●).
    for t in [
        "入口文件很干净：只有一个 `main` 函数。",
        "它向终端打印一句问候。",
        "要我继续看别的模块吗？",
    ] {
        app.on_event(&Event::LlmDelta {
            stage: "工作阶段".into(),
            kind: "content".into(),
            text: t.into(),
        });
    }

    // §8.5 context composition — same six segments (name + order) as the real
    // `engine::context_sections`; sample `chars` + provider-measured tokens.
    app.on_event(&Event::ContextUsage {
        stage: "工作阶段".into(),
        sections: json!([
            {"name": "规则底座", "chars": 8000},
            {"name": "知识记忆", "chars": 5100},
            {"name": "历史观测", "chars": 9000},
            {"name": "最近对话", "chars": 22400},
            {"name": "工具能力", "chars": 4800},
            {"name": "当前提问", "chars": 2500},
        ]),
        total_chars: 51_800,
    });
    app.on_event(&Event::StageResultReported {
        stage: "工作阶段".into(),
        ok: true,
        data: Value::Null,
        fault_type: String::new(),
        fault_detail: String::new(),
        tokens: json!({"input_tokens": 20480, "output_tokens": 120}),
        tool_calls: 1,
        elapsed_ms: 1800.0,
    });
    app.on_turn_done(Ok(Default::default()));

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let w = buf.area.width as usize;
    for y in 0..buf.area.height as usize {
        let row: String = (0..w).map(|x| buf.content[y * w + x].symbol()).collect();
        println!("{}", row.trim_end());
    }
}
