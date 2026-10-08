//! Offline replay tool for terminal output dumps.
//!
//! Replays a dump of the sandbox terminal output through the TUI terminal
//! emulator and prints the resulting screen. Use it to examine terminal
//! rendering problems without a running sandbox.

use std::io::Write;

/// Replays a PTY dump and prints the resulting screen grid.
///
/// To make a dump, set `AIRLOCK_PTY_DUMP=1`. Airlock then writes the guest
/// output stream to `<sandbox_dir>/pty.dump`.
///
/// Usage: cargo run --example vt100_replay -- <dump-path> [rows] [cols]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| ".airlock/sandbox/pty.dump".into());
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((166, 50));
    let rows: u16 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(term_rows.saturating_sub(2));
    let cols: u16 = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(term_cols);

    let data = std::fs::read(&path).expect("read dump");
    eprintln!("replaying {} bytes at {rows}x{cols}", data.len());

    let mut sink = airlock_monitor::pty::TuiTerminalSink::new(rows, cols, 1000);
    sink.write(&data);
    let screen = sink.screen();
    let (rows, cols) = screen.size();
    println!(
        "grid {rows}x{cols}, alt_screen={}, hide_cursor={}, scrollback={}",
        screen.alternate_screen(),
        screen.hide_cursor(),
        screen.scrollback()
    );
    for r in 0..rows {
        let mut line = String::new();
        for c in 0..cols {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                if s.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(s);
                }
            }
        }
        println!("{r:3}: |{}|", line.trim_end());
    }
    std::io::stdout().flush().unwrap();
}
