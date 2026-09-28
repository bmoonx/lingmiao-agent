//! Hidden-failure contract probe (cli 2026-09-28 ②「运行中也会有UI突然变乱」).
//!
//! A panic inside a background task used to be the one way the screen could
//! corrupt itself mid-session: `ratatui`'s hook restores the terminal (leaving
//! raw mode / the alternate screen) and the default hook then prints the message
//! to **stderr** — i.e. into the alternate screen, where an incremental repaint
//! never clears it. This example drives the replacement hook with a real
//! background panic and proves two things on a real terminal:
//!
//! 1. the panic message goes to the **log**, not to stderr;
//! 2. the run loop's half works — the message is available exactly once, so it
//!    can be turned into one full repaint + one transcript error block.
//!
//! ```sh
//! cargo run -p lingmiao-tui --example panic_probe
//! ```
//!
//! Prints `PROBE_LOG=…` / `PROBE_TAKEN=…` / `PROBE_TAKEN_AGAIN=…` on stdout. The
//! caller checks that **stderr stayed empty**.

use lingmiao_tui::install_panic_hook;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    install_panic_hook();
    // A background task panics — exactly the shape that used to corrupt the UI.
    let handle = tokio::spawn(async { panic!("probe: background task exploded") });
    let _ = handle.await;
    println!("PROBE_TAKEN={:?}", lingmiao_tui::take_panic());
    println!("PROBE_TAKEN_AGAIN={:?}", lingmiao_tui::take_panic());
    println!("PROBE_DONE");
}
