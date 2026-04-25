//! Replay a dumped byte stream (from AIRLOCK_PTY_DUMP=1, which writes to
//! `<sandbox_dir>/pty.dump`) through vt100 and print the resulting grid,
//! so we can diagnose terminal rendering issues offline.
//!
//! Usage: cargo run --example vt100_replay -- <dump-path> [rows] [cols]

use std::io::Write;

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
