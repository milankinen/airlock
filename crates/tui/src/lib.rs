//! TUI monitoring control panel for `airlock start --monitor`.
//!
//! Runs the terminal UI on a dedicated `std::thread`, fully decoupled from
//! the async RPC event loop. Communication happens via channels:
//!
//! - **To TUI:** process output, network events, exit code (`std::sync::mpsc`)
//! - **From TUI:** keystrokes and resize events (`tokio::sync::mpsc`)

mod app;
pub mod input;
pub mod pty;
mod tabs;
mod ui;

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use app::{App, Tab};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
pub use input::{TuiInputEvent, TuiStdin};
use pty::TuiTerminalSink;
use ratatui::DefaultTerminal;
pub use ui::TAB_BAR_HEIGHT;

/// A network connection event emitted by the host-side proxy.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    Connect {
        host: String,
        port: u16,
        allowed: bool,
    },
}

/// Events sent to the TUI thread.
enum TuiEvent {
    /// Process stdout/stderr output bytes.
    Output(Vec<u8>),
    /// Network connection event for the monitor tab.
    Network(NetworkEvent),
    /// Process exited with the given code.
    Exit(i32),
    /// Terminal event from crossterm.
    Terminal(Event),
}

/// Sender for feeding events to the TUI thread.
///
/// All methods are non-blocking (unbounded channel).
#[derive(Clone)]
pub struct TuiSender {
    tx: std_mpsc::Sender<TuiEvent>,
}

impl TuiSender {
    /// Send process output (stdout or stderr) to the TUI for display.
    pub fn send_output(&self, data: Vec<u8>) {
        let _ = self.tx.send(TuiEvent::Output(data));
    }

    /// Send a network event to the TUI network tab.
    pub fn send_network(&self, ev: NetworkEvent) {
        let _ = self.tx.send(TuiEvent::Network(ev));
    }

    /// Notify the TUI that the sandbox process has exited.
    pub fn send_exit(&self, code: i32) {
        let _ = self.tx.send(TuiEvent::Exit(code));
    }
}

/// Handle to a running TUI thread.
pub struct TuiHandle {
    /// Sender for pushing events to the TUI.
    pub tx: TuiSender,
    join: Option<std::thread::JoinHandle<anyhow::Result<i32>>>,
}

impl TuiHandle {
    /// Block until the TUI thread finishes and return its exit code.
    pub fn join(mut self) -> anyhow::Result<i32> {
        match self.join.take() {
            Some(h) => h.join().unwrap_or(Ok(1)),
            None => Ok(1),
        }
    }
}

impl Drop for TuiHandle {
    fn drop(&mut self) {
        // If join() was never called, at least wait for the thread.
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Spawn the TUI on a dedicated thread and return a handle for communication.
///
/// - `stdin_tx`: channel for sending keystrokes/resize to the RPC stdin server
/// - `policy`: network policy string displayed in the network tab header
pub fn spawn(stdin_tx: tokio::sync::mpsc::Sender<TuiInputEvent>, policy: String) -> TuiHandle {
    let (tx, rx) = std_mpsc::channel();
    let crossterm_tx = tx.clone();

    let join = std::thread::spawn(move || tui_main(rx, crossterm_tx, stdin_tx, policy));

    TuiHandle {
        tx: TuiSender { tx },
        join: Some(join),
    }
}

/// TUI thread entry point — runs synchronously, never touches the async runtime.
#[allow(clippy::needless_pass_by_value)] // owned values required by thread::spawn move
fn tui_main(
    rx: std_mpsc::Receiver<TuiEvent>,
    crossterm_tx: std_mpsc::Sender<TuiEvent>,
    stdin_tx: tokio::sync::mpsc::Sender<TuiInputEvent>,
    policy: String,
) -> anyhow::Result<i32> {
    // Enter alternate screen, raw mode, mouse capture, and kitty keyboard protocol
    let mut terminal = ratatui::init();
    let kitty_enabled = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if kitty_enabled {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ),
        )?;
    }
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;

    // Ensure terminal is restored on all exit paths. Explicit `Show` after
    // `ratatui::restore()` is required because ratatui may have issued `Hide`
    // in its last frame (when the network tab was active and no cursor was
    // set) — without this, the host terminal cursor stays hidden after exit.
    let result = run_tui_loop(
        &mut terminal,
        &rx,
        crossterm_tx,
        &stdin_tx,
        policy,
        kitty_enabled,
    );

    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;
    if kitty_enabled {
        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags,
        )?;
    }
    ratatui::restore();
    crossterm::execute!(
        std::io::stdout(),
        crossterm::cursor::SetCursorStyle::DefaultUserShape,
        crossterm::cursor::Show,
    )?;

    result
}

fn run_tui_loop(
    terminal: &mut DefaultTerminal,
    rx: &std_mpsc::Receiver<TuiEvent>,
    crossterm_tx: std_mpsc::Sender<TuiEvent>,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    policy: String,
    kitty_enabled: bool,
) -> anyhow::Result<i32> {
    let mut sink = TuiTerminalSink::new(80, 24);
    let mut app = App::new(policy);
    let mut mouse_captured = true;

    // Resize vt100 parser to match terminal body area
    let size = terminal.size()?;
    let size = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    let body = ui::body_area(size);
    sink.resize(body.height, body.width);

    // Crossterm reader thread — sends terminal events into the unified channel
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if crossterm_tx.send(TuiEvent::Terminal(ev)).is_err() {
                break;
            }
        }
    });

    loop {
        // Render frame
        terminal.draw(|f| ui::render(f, &app, &sink))?;

        // Wait for next event (blocks up to 16ms for ~60fps rendering)
        let event = match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(ev) => Some(ev),
            Err(std_mpsc::RecvTimeoutError::Timeout) => None,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(1),
        };

        // Process the event (if any) plus any queued events
        if let Some(ev) = event
            && let Some(code) = handle_event(
                ev,
                &mut app,
                &mut sink,
                stdin_tx,
                terminal,
                kitty_enabled,
                &mut mouse_captured,
            )?
        {
            return Ok(code);
        }
        while let Ok(ev) = rx.try_recv() {
            if let Some(code) = handle_event(
                ev,
                &mut app,
                &mut sink,
                stdin_tx,
                terminal,
                kitty_enabled,
                &mut mouse_captured,
            )? {
                return Ok(code);
            }
        }
    }
}

/// Process a single TUI event. Returns `Some(exit_code)` if the TUI should exit.
#[allow(clippy::too_many_arguments)]
fn handle_event(
    event: TuiEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    terminal: &mut DefaultTerminal,
    kitty_enabled: bool,
    mouse_captured: &mut bool,
) -> anyhow::Result<Option<i32>> {
    match event {
        TuiEvent::Output(data) => {
            sink.write(&data);
        }
        TuiEvent::Network(ev) => {
            app.network.push_event(ev);
        }
        TuiEvent::Exit(code) => {
            return Ok(Some(code));
        }
        TuiEvent::Terminal(Event::Key(key)) => {
            if let Some(code) = handle_key(key, app, sink, stdin_tx, kitty_enabled, mouse_captured)?
            {
                return Ok(Some(code));
            }
        }
        TuiEvent::Terminal(Event::Mouse(mouse)) => {
            handle_mouse(mouse, app, sink, terminal)?;
        }
        TuiEvent::Terminal(Event::Resize(cols, rows)) => {
            let size = ratatui::layout::Rect::new(0, 0, cols, rows);
            let body = ui::body_area(size);
            sink.resize(body.height, body.width);
            let _ = stdin_tx.blocking_send(TuiInputEvent::Resize(body.height, body.width));
        }
        TuiEvent::Terminal(_) => {}
    }
    Ok(None)
}

/// Handle a key event. Returns `Some(code)` if the TUI should exit.
fn handle_key(
    key: KeyEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    kitty_enabled: bool,
    mouse_captured: &mut bool,
) -> anyhow::Result<Option<i32>> {
    // Global shortcuts
    match (key.modifiers, key.code) {
        (_, KeyCode::F(1)) => {
            app.active_tab = Tab::Sandbox;
            return Ok(None);
        }
        (_, KeyCode::F(2)) => {
            app.active_tab = Tab::Network;
            return Ok(None);
        }
        (_, KeyCode::F(12)) => {
            *mouse_captured = !*mouse_captured;
            if *mouse_captured {
                crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
            } else {
                crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;
            }
            app.mouse_captured = *mouse_captured;
            return Ok(None);
        }
        _ => {}
    }

    match app.active_tab {
        Tab::Sandbox => {
            if let Some(bytes) = key_to_bytes(key, kitty_enabled) {
                // Any key input jumps back to the live view.
                sink.scroll_to_bottom();
                let _ = stdin_tx.blocking_send(TuiInputEvent::Data(bytes));
            }
        }
        Tab::Network => match key.code {
            KeyCode::Up => app.network.scroll_up(1),
            KeyCode::Down => app.network.scroll_down(1),
            KeyCode::PageUp => app.network.scroll_up(20),
            KeyCode::PageDown => app.network.scroll_down(20),
            KeyCode::Home => app.network.scroll_to_top(),
            KeyCode::End => app.network.scroll_to_bottom(),
            _ => {}
        },
    }

    Ok(None)
}

fn handle_mouse(
    mouse: MouseEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<()> {
    let size = terminal.size()?;
    let size = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    let tab_rects = ui::tab_header_rects(size);

    match mouse.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            for (tab, rect) in &tab_rects {
                if mouse.row >= rect.y
                    && mouse.row < rect.y + rect.height
                    && mouse.column >= rect.x
                    && mouse.column < rect.x + rect.width
                {
                    app.active_tab = *tab;
                    break;
                }
            }
        }
        MouseEventKind::ScrollUp => match app.active_tab {
            Tab::Network => app.network.scroll_up(3),
            Tab::Sandbox => sink.scroll_up(3),
        },
        MouseEventKind::ScrollDown => match app.active_tab {
            Tab::Network => app.network.scroll_down(3),
            Tab::Sandbox => sink.scroll_down(3),
        },
        _ => {}
    }

    Ok(())
}

/// Convert a crossterm key event into escape sequence bytes for the PTY.
///
/// Uses legacy Xterm encoding by default for maximum guest compatibility.
/// Switches to Kitty CSI-u encoding only for modified special keys (e.g.
/// Shift+Enter) where Xterm encoding would lose the modifier information.
fn key_to_bytes(key: KeyEvent, kitty_enabled: bool) -> Option<Vec<u8>> {
    // Use Kitty encoding only for keys where Xterm would discard modifiers.
    // Xterm can't encode SHIFT on Enter, Backspace, Escape, or Space — they
    // all produce the same byte regardless of Shift. Everything else (Ctrl+key,
    // Alt+key, modified arrows/function keys) encodes fine with Xterm.
    let use_kitty = kitty_enabled
        && key.modifiers.intersects(KeyModifiers::SHIFT)
        && matches!(
            key.code,
            KeyCode::Enter | KeyCode::Backspace | KeyCode::Esc | KeyCode::Char(' ')
        );

    let key = terminput_crossterm::to_terminput_key(key).ok()?;
    let event = terminput::Event::Key(key);
    let encoding = if use_kitty {
        terminput::Encoding::Kitty(terminput::KittyFlags::DISAMBIGUATE_ESCAPE_CODES)
    } else {
        terminput::Encoding::Xterm
    };
    let mut buf = [0u8; 64];
    let n = event.encode(&mut buf, encoding).ok()?;
    if n == 0 {
        None
    } else {
        Some(buf[..n].to_vec())
    }
}
