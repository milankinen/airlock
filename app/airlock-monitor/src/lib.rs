//! TUI monitoring control panel for `airlock start --monitor`.
//!
//! Runs the terminal UI on a dedicated `std::thread`, fully decoupled from
//! the async RPC event loop. Communication happens via channels:
//!
//! - **To TUI:** process output, network events, exit code (`std::sync::mpsc`)
//! - **From TUI:** keystrokes and resize events (`tokio::sync::mpsc`)

mod app;
pub mod input;
mod network_control;
pub mod pty;
mod settings;
mod tabs;
mod ui;

use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, SystemTime};

use app::{App, Tab};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
pub use input::{TuiInputEvent, TuiStdin};
pub use network_control::{NetworkControl, Policy};
use pty::TuiTerminalSink;
use ratatui::DefaultTerminal;
pub use settings::TuiSettings;
pub use ui::TAB_BAR_HEIGHT;

/// Snapshot of guest resource usage, displayed on the Monitor tab.
#[derive(Debug, Clone, Default)]
pub struct StatsSnapshot {
    pub per_core: Vec<u8>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub load_avg: (f32, f32, f32),
}

/// A network event emitted by the host-side proxy for the Monitor tab.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// Raw TCP connect decision (allow/deny at connection time).
    Connect(Arc<ConnectInfo>),
    /// Previously-connected TCP connection closed. The `id` matches the
    /// `ConnectInfo::id` of the `Connect` event that opened it.
    Disconnect(Arc<DisconnectInfo>),
    /// HTTP request observed by the middleware.
    Request(Arc<RequestInfo>),
}

/// TCP-level connect event payload. Wrapped in `Arc` so the broadcast
/// channel only bumps a refcount on recv rather than cloning fields.
#[derive(Debug)]
pub struct ConnectInfo {
    /// Monotonic per-process connection id — used to link a later
    /// `DisconnectInfo` back to its `ConnectInfo`.
    pub id: u64,
    pub timestamp: SystemTime,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
}

/// Event payload for a TCP connection closing.
#[derive(Debug)]
pub struct DisconnectInfo {
    pub id: u64,
    pub timestamp: SystemTime,
}

/// HTTP request event payload. Wrapped in `Arc` on the wire.
#[derive(Debug)]
pub struct RequestInfo {
    pub timestamp: SystemTime,
    pub method: String,
    pub path: String,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    pub headers: Vec<(String, String)>,
}

/// Events sent to the TUI thread.
enum TuiEvent {
    /// Process stdout/stderr output bytes.
    Output(Vec<u8>),
    /// Network connection event for the monitor tab.
    Network(NetworkEvent),
    /// Guest resource snapshot for the monitor tab's CPU/memory widgets.
    Stats(StatsSnapshot),
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

    /// Send a guest stats snapshot to the TUI monitor tab.
    pub fn send_stats(&self, snapshot: StatsSnapshot) {
        let _ = self.tx.send(TuiEvent::Stats(snapshot));
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
/// - `sig_tx`: channel for TUI-initiated signals (e.g. SIGINT when the user
///   presses `q` or Ctrl+D on the monitor tab)
/// - `network`: live handle into host network state (policy + future toggles)
pub fn spawn(
    stdin_tx: tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: tokio::sync::mpsc::Sender<i32>,
    network: Arc<dyn NetworkControl>,
    project_path: String,
) -> TuiHandle {
    let (tx, rx) = std_mpsc::channel();
    let crossterm_tx = tx.clone();

    let join = std::thread::spawn(move || {
        tui_main(rx, crossterm_tx, stdin_tx, sig_tx, network, project_path)
    });

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
    sig_tx: tokio::sync::mpsc::Sender<i32>,
    network: Arc<dyn NetworkControl>,
    project_path: String,
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
    // Bracketed paste: crossterm reports paste as `Event::Paste(String)`
    // instead of dozens of individual key events (which would include the
    // Enter between lines and execute pasted code immediately).
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;

    // Ensure terminal is restored on all exit paths. Explicit `Show` after
    // `ratatui::restore()` is required because ratatui may have issued `Hide`
    // in its last frame (when the network tab was active and no cursor was
    // set) — without this, the host terminal cursor stays hidden after exit.
    let result = run_tui_loop(
        &mut terminal,
        &rx,
        crossterm_tx,
        &stdin_tx,
        &sig_tx,
        network,
        project_path,
        kitty_enabled,
    );

    crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste)?;
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

#[allow(clippy::too_many_arguments)]
fn run_tui_loop(
    terminal: &mut DefaultTerminal,
    rx: &std_mpsc::Receiver<TuiEvent>,
    crossterm_tx: std_mpsc::Sender<TuiEvent>,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: &tokio::sync::mpsc::Sender<i32>,
    network: Arc<dyn NetworkControl>,
    project_path: String,
    kitty_enabled: bool,
) -> anyhow::Result<i32> {
    let mut sink = TuiTerminalSink::new(80, 24);
    let mut app = App::new(network, project_path);
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
                sig_tx,
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
                sig_tx,
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
    sig_tx: &tokio::sync::mpsc::Sender<i32>,
    terminal: &mut DefaultTerminal,
    kitty_enabled: bool,
    mouse_captured: &mut bool,
) -> anyhow::Result<Option<i32>> {
    match event {
        TuiEvent::Output(data) => {
            scan_bracketed_paste_mode(&data, &mut app.guest_bracketed_paste);
            sink.write(&data);
        }
        TuiEvent::Network(ev) => {
            app.monitor.network.push_event(ev, &app.settings);
        }
        TuiEvent::Stats(snapshot) => {
            app.monitor.apply_stats(snapshot);
        }
        TuiEvent::Exit(code) => {
            return Ok(Some(code));
        }
        TuiEvent::Terminal(Event::Key(key)) => {
            if let Some(code) = handle_key(
                key,
                app,
                sink,
                stdin_tx,
                sig_tx,
                kitty_enabled,
                mouse_captured,
            )? {
                return Ok(Some(code));
            }
        }
        TuiEvent::Terminal(Event::Mouse(mouse)) => {
            handle_mouse(mouse, app, sink, terminal, mouse_captured)?;
        }
        TuiEvent::Terminal(Event::Resize(cols, rows)) => {
            let size = ratatui::layout::Rect::new(0, 0, cols, rows);
            let body = ui::body_area(size);
            sink.resize(body.height, body.width);
            let _ = stdin_tx.blocking_send(TuiInputEvent::Resize(body.height, body.width));
        }
        TuiEvent::Terminal(Event::Paste(text)) => {
            // Only forward paste while the sandbox tab is active. Wrap in
            // bracketed paste markers only when the guest shell asked for
            // them (`\e[?2004h`); shells without support (BusyBox ash, dash)
            // mis-parse the markers and silently eat surrounding bytes.
            if app.active_tab == Tab::Sandbox {
                let bytes = if app.guest_bracketed_paste {
                    let mut b = Vec::with_capacity(text.len() + 12);
                    b.extend_from_slice(b"\x1b[200~");
                    b.extend_from_slice(text.as_bytes());
                    b.extend_from_slice(b"\x1b[201~");
                    b
                } else {
                    text.into_bytes()
                };
                let _ = stdin_tx.blocking_send(TuiInputEvent::Data(bytes));
            }
        }
        TuiEvent::Terminal(_) => {}
    }
    Ok(None)
}

/// Scan guest PTY output for the DEC private mode toggles that enable or
/// disable bracketed paste (`\e[?2004h` / `\e[?2004l`). Used to decide
/// whether to wrap host pastes in `\e[200~...\e[201~` before forwarding —
/// shells that don't support it (BusyBox ash) mis-parse the markers and
/// eat surrounding bytes.
///
/// Doesn't try to handle the sequence being split across chunks: the guest
/// re-emits on every prompt redraw, so a single miss resolves itself.
fn scan_bracketed_paste_mode(data: &[u8], enabled: &mut bool) {
    const ENABLE: &[u8] = b"\x1b[?2004h";
    const DISABLE: &[u8] = b"\x1b[?2004l";
    for window in data.windows(ENABLE.len()) {
        if window == ENABLE {
            *enabled = true;
        } else if window == DISABLE {
            *enabled = false;
        }
    }
}

/// Handle a key event. Returns `Some(code)` if the TUI should exit.
#[allow(clippy::too_many_arguments)]
fn handle_key(
    key: KeyEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: &tokio::sync::mpsc::Sender<i32>,
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
            app.active_tab = Tab::Monitor;
            return Ok(None);
        }
        _ => {}
    }

    // Auto-exit selection mode: Esc or Ctrl+C in the sandbox tab re-enables
    // mouse capture so the click-to-select flow is reversible.
    if app.active_tab == Tab::Sandbox
        && !*mouse_captured
        && (key.code == KeyCode::Esc
            || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c')))
    {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
        *mouse_captured = true;
        app.mouse_captured = true;
        return Ok(None);
    }

    match app.active_tab {
        Tab::Sandbox => {
            if let Some(bytes) = key_to_bytes(key, kitty_enabled) {
                // Any key input jumps back to the live view.
                sink.scroll_to_bottom();
                let _ = stdin_tx.blocking_send(TuiInputEvent::Data(bytes));
            }
        }
        Tab::Monitor => {
            if app.monitor.network.dropdown_open() {
                match key.code {
                    KeyCode::Up => app.monitor.network.nudge_policy_highlight(-1),
                    KeyCode::Down => app.monitor.network.nudge_policy_highlight(1),
                    KeyCode::Enter => {
                        if let Some(p) = app.monitor.network.highlighted_policy() {
                            app.network.set_policy(p);
                        }
                        app.monitor.network.close_policy_dropdown();
                    }
                    KeyCode::Esc => app.monitor.network.close_policy_dropdown(),
                    _ => {}
                }
                return Ok(None);
            }
            if app.monitor.network.details_open() {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('x' | 'X') => {
                        app.monitor.network.close_details();
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        app.monitor.network.toggle_sub_tab();
                    }
                    KeyCode::Char('r' | 'R') => {
                        app.monitor
                            .network
                            .select_sub_tab(crate::tabs::monitor::network::NetworkSubTab::Requests);
                    }
                    KeyCode::Char('c' | 'C') => {
                        app.monitor.network.select_sub_tab(
                            crate::tabs::monitor::network::NetworkSubTab::Connections,
                        );
                    }
                    KeyCode::Char('p' | 'P') => {
                        app.monitor
                            .network
                            .open_policy_dropdown(app.network.policy());
                    }
                    KeyCode::Char('q' | 'Q') if key.modifiers.is_empty() => {
                        app.active_tab = Tab::Sandbox;
                    }
                    KeyCode::Char('d' | 'D') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = sig_tx.blocking_send(1);
                        let _ = sig_tx.blocking_send(15);
                    }
                    _ => {}
                }
                return Ok(None);
            }
            match key.code {
                KeyCode::Up => app.monitor.network.select_up(),
                KeyCode::Down => app.monitor.network.select_down(),
                KeyCode::PageUp => app.monitor.network.select_page_up(),
                KeyCode::PageDown => app.monitor.network.select_page_down(),
                KeyCode::Home => app.monitor.network.select_newest(),
                KeyCode::End => app.monitor.network.select_oldest(),
                KeyCode::Enter => app.monitor.network.open_details(),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    app.monitor.network.toggle_sub_tab();
                }
                KeyCode::Char('r' | 'R') => {
                    app.monitor
                        .network
                        .select_sub_tab(crate::tabs::monitor::network::NetworkSubTab::Requests);
                }
                KeyCode::Char('c' | 'C') => {
                    app.monitor
                        .network
                        .select_sub_tab(crate::tabs::monitor::network::NetworkSubTab::Connections);
                }
                KeyCode::Char('p' | 'P') => {
                    app.monitor
                        .network
                        .open_policy_dropdown(app.network.policy());
                }
                // Ctrl+D from the monitor tab asks the sandbox process to
                // exit. SIGHUP first — it's the canonical "controlling
                // terminal went away" signal and interactive shells like
                // bash exit on it (SIGINT/SIGTERM get ignored at an idle
                // prompt). SIGTERM follows as a fallback for anything that
                // doesn't handle HUP. The TUI itself shuts down when the
                // process's exit event arrives on the main channel, so we
                // don't return early.
                KeyCode::Char('q' | 'Q') if key.modifiers.is_empty() => {
                    app.active_tab = Tab::Sandbox;
                }
                KeyCode::Char('d' | 'D') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = sig_tx.blocking_send(1);
                    let _ = sig_tx.blocking_send(15);
                }
                _ => {}
            }
        }
    }

    Ok(None)
}

fn handle_mouse(
    mouse: MouseEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    terminal: &mut DefaultTerminal,
    mouse_captured: &mut bool,
) -> anyhow::Result<()> {
    let size = terminal.size()?;
    let size = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    let tab_rects = ui::tab_header_rects(size);

    match mouse.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            // While the policy dropdown is open, it consumes clicks: pick a
            // row or close on any other click.
            if app.active_tab == Tab::Monitor && app.monitor.network.dropdown_open() {
                if let Some(p) = app.monitor.network.dropdown_row_at(mouse.column, mouse.row) {
                    app.network.set_policy(p);
                }
                app.monitor.network.close_policy_dropdown();
                return Ok(());
            }
            for (tab, rect) in &tab_rects {
                if mouse.row >= rect.y
                    && mouse.row < rect.y + rect.height
                    && mouse.column >= rect.x
                    && mouse.column < rect.x + rect.width
                {
                    app.active_tab = *tab;
                    return Ok(());
                }
            }
            // Policy title anchor click opens the dropdown.
            if app.active_tab == Tab::Monitor
                && app
                    .monitor
                    .network
                    .is_policy_anchor(mouse.column, mouse.row)
            {
                app.monitor
                    .network
                    .open_policy_dropdown(app.network.policy());
                return Ok(());
            }
            // Details sub-tab close button (×). Check before the generic
            // sub-tab hit test so the × inside the details label rect takes
            // precedence over re-selecting the already-active details tab.
            if app.active_tab == Tab::Monitor
                && app
                    .monitor
                    .network
                    .is_details_close(mouse.column, mouse.row)
            {
                app.monitor.network.close_details();
                return Ok(());
            }
            // Sub-tab click inside the monitor tab.
            if app.active_tab == Tab::Monitor
                && let Some(sub) = app.monitor.network.sub_tab_at(mouse.column, mouse.row)
            {
                use crate::tabs::monitor::network::NetworkSubTab;
                match sub {
                    NetworkSubTab::Details => {} // clicking the active details tab is a no-op
                    _ => app.monitor.network.select_sub_tab(sub),
                }
                return Ok(());
            }
            // Click inside the sandbox body: drop mouse capture so the
            // terminal's native selection takes over. The first click is
            // consumed; the user's drag-to-select starts on the next press.
            // Esc / Ctrl+C restores capture (see handle_key).
            if app.active_tab == Tab::Sandbox && *mouse_captured {
                crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;
                *mouse_captured = false;
                app.mouse_captured = false;
            }
        }
        MouseEventKind::ScrollUp => match app.active_tab {
            Tab::Monitor => {
                for _ in 0..3 {
                    app.monitor.network.select_up();
                }
            }
            Tab::Sandbox => sink.scroll_up(3),
        },
        MouseEventKind::ScrollDown => match app.active_tab {
            Tab::Monitor => {
                for _ in 0..3 {
                    app.monitor.network.select_down();
                }
            }
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
