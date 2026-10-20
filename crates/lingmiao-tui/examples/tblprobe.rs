//! Eyeball a markdown **table** inside the real CC-style pane (§12) at a given
//! terminal width: `cargo run -p lingmiao-tui --example tblprobe -- 80 wide`.
//!
//! Used while fixing cli 2026-10-19「表格排版会吞掉字」: it shows exactly what
//! reaches the screen (a `TestBackend` drawn through `App::render`), so a
//! truncated cell / a table torn apart by the outer wrap is visible instead of
//! being argued about.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lingmiao_tui::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn main() {
    let width: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let which = std::env::args().nth(2).unwrap_or_else(|| "long".into());
    let md = match which.as_str() {
        "long" => {
            "| 项目 | 说明 |\n|---|---|\n| 引擎硬拦 | 工具白名单、先读后写、路径沙箱三道物理拦截 |\n| 记忆 | 四层记忆 + 六路召回 |\n"
        }
        "wide" => {
            "| 阶段 | 工具数 | 说明 | 备注 | 来源 | 时间 | 状态 |\n|---|---|---|---|---|---|---|\n| 组织上下文 | 14 | 检索记忆与约束，每轮都跑一遍 | 见设计文档第八节 | 流水线 | 2026-10-19 | 已完成 |\n"
        }
        "path" => {
            "| 文件 | 说明 |\n|---|---|\n| crates/lingmiao-tui/src/markdown.rs | 表格渲染与折行规则 |\n"
        }
        "sell" => {
            "| 卖点名 | 说明 | 原因 |\n|---|---|---|\n| 即插即用 | 无需配置模型与工具链，装完就能上手，常用流程开箱可用 | 降低新用户首次上手成本 |\n| 本地记忆持久化 | 对话与记忆落在本地，关掉重开仍在，跨轮次保持一致 | 避免反复交代背景 |\n| 全链路可审计 | 每一步工具调用与结论都可回看，出错能定位到具体环节 | 减少幻觉与甩锅 |\n| 真实执行闭环 | 不是只给建议，而是真的改文件、跑命令、验证结果 | 只有真跑过才算完成 |\n"
        }
        "many" => {
            "| 一 | 二 | 三 | 四 | 五 | 六 | 七 | 八 | 九 | 十 | 十一 | 十二 |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n| 甲 | 乙 | 丙 | 丁 | 戊 | 己 | 庚 | 辛 | 壬 | 癸 | 子 | 丑 |\n"
        }
        _ => "| a | b |\n|---|---|\n| 1 | 2 |\n",
    };
    let mut app = App::new("deepseek-v4-pro", "deepseek");
    for c in "表格测试".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Enter));
    app.on_event(&lingmiao_core::events::Event::LlmDelta {
        stage: "工作阶段".into(),
        kind: "content".into(),
        text: md.to_string(),
    });
    app.on_turn_done(Ok(Default::default()));

    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let w = buf.area.width as usize;
    for y in 0..buf.area.height as usize {
        let row: String = (0..w).map(|x| buf.content[y * w + x].symbol()).collect();
        println!("{}", row.trim_end());
    }
}
