//! Control for [`panic_probe`](panic_probe.rs) — the **pre-fix** behaviour.
//!
//! It reproduces the exact pre-2026-09-28 sequence: the terminal is put into the
//! alternate screen by `ratatui::init()` (whose built-in hook only *restores* on
//! panic), then a background task panics with the **default** hook still in
//! place. Two things go wrong, and this example exists so both are measurable
//! rather than asserted:
//!
//! * the default hook prints the message to **stderr** — i.e. into the alternate
//!   screen, where ratatui's incremental repaint never clears it (§35's
//!   「拖动窗口才变好」 mechanism);
//! * `restore()` (leave raw mode / leave alternate screen) fires for a task
//!   panic that did **not** end the process — so the still-running UI then paints
//!   over the shell's normal screen.
//!
//! Deliberately **not** a production path. Run it under a PTY and diff against
//! [`panic_probe`](panic_probe.rs): here stderr carries the panic text, there
//! stderr stays empty.
//!
//! ```sh
//! cargo run -p lingmiao-tui --example panic_control
//! ```

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _terminal = ratatui::init();
    let handle = tokio::spawn(async { panic!("control: background task exploded") });
    let _ = handle.await;
    println!("CONTROL_DONE");
}
