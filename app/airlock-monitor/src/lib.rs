//! TUI monitoring control panel for `airlock start --monitor`.
//!
//! The TUI shows the sandbox terminal and a Monitor tab with resource usage and
//! network activity. The user can also change the network policy from the TUI.
//!
//! The TUI runs on its own thread, separately from the host event loop. The
//! host sends process output, network events, resource statistics and the exit
//! code to the TUI. The TUI sends keystrokes, terminal size changes and process
//! signals to the sandbox.

mod app;
pub mod input;
pub mod keys;
mod mouse;
mod network_control;
pub mod pty;
mod settings;
mod tabs;
mod terminal;
mod ui;

use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, SystemTime};

use app::{App, Tab};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
pub use input::{TuiInputEvent, TuiStdin};
pub use keys::{Action, KeyBindings};
pub use network_control::{NetworkControl, Policy};
use pty::{MouseProtocolMode, TuiTerminalSink};
use ratatui::backend::Backend;
use ratatui::{DefaultTerminal, Terminal};
pub use settings::TuiSettings;
pub use ui::TAB_BAR_HEIGHT;

/// Snapshot of guest resource usage, displayed on the Monitor tab.
#[derive(Debug, Clone, Default)]
pub struct StatsSnapshot {
    /// CPU usage of each core, in percent.
    pub per_core: Vec<u8>,
    /// Total guest memory in bytes.
    pub total_bytes: u64,
    /// Used guest memory in bytes.
    pub used_bytes: u64,
    /// Guest load average for 1, 5 and 15 minutes.
    pub load_avg: (f32, f32, f32),
}

/// Network event from the host-side proxy for the Monitor tab.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// TCP connect decision (allow or deny when the connection opens).
    Connect(Arc<ConnectInfo>),
    /// A connected TCP connection closed. The `id` is the same as the
    /// [`ConnectInfo::id`] of the `Connect` event that opened it.
    Disconnect(Arc<DisconnectInfo>),
    /// New byte counters for a live connection. The `id` is the same as the
    /// [`ConnectInfo::id`] of the `Connect` event that opened it.
    Traffic(Arc<TrafficInfo>),
    /// HTTP request that the middleware saw.
    Request(Arc<RequestInfo>),
    /// Response to a request from an earlier `Request` event. The `id` is
    /// the same as the [`RequestInfo::id`] of that request.
    Response(Arc<ResponseInfo>),
}

/// Payload of a TCP connect event.
///
/// The payload is in an `Arc`, so a receive from the broadcast channel only
/// increments a reference count and does not clone the fields.
#[derive(Debug)]
pub struct ConnectInfo {
    /// Connection ID. It increases monotonically in the process. A later
    /// [`DisconnectInfo`] uses it to refer to this connection.
    pub id: u64,
    /// Time of the connect attempt.
    pub timestamp: SystemTime,
    /// Target host.
    pub host: String,
    /// Target port.
    pub port: u16,
    /// True if the network policy allowed the connection.
    pub allowed: bool,
}

/// Payload of a TCP disconnect event.
#[derive(Debug)]
pub struct DisconnectInfo {
    /// ID of the closed connection, from [`ConnectInfo::id`].
    pub id: u64,
    /// Time when the connection closed.
    pub timestamp: SystemTime,
}

/// Cumulative byte counters for one connection. The values are totals, not
/// deltas.
///
/// The proxy counts the bytes on the raw stream, below the TLS layer that it
/// terminates. Thus the counts are wire bytes, which include encrypted
/// records and the handshake.
#[derive(Debug)]
pub struct TrafficInfo {
    /// Connection ID, from [`ConnectInfo::id`].
    pub id: u64,
    /// Total bytes from guest to server.
    pub up: u64,
    /// Total bytes from server to guest.
    pub down: u64,
}

/// Payload of an HTTP request event. The sender wraps it in an `Arc`.
#[derive(Debug)]
pub struct RequestInfo {
    /// Request ID. It increases monotonically in the process. The matching
    /// [`ResponseInfo`] uses it to refer to this request.
    pub id: u64,
    /// Time of the request.
    pub timestamp: SystemTime,
    /// HTTP method.
    pub method: String,
    /// Request path.
    pub path: String,
    /// Target host.
    pub host: String,
    /// Target port.
    pub port: u16,
    /// True if the network policy allowed the request.
    pub allowed: bool,
    /// Request headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,
}

/// Payload of an HTTP response event. The `id` field links it to a
/// [`RequestInfo`].
#[derive(Debug)]
pub struct ResponseInfo {
    /// Request ID, from [`RequestInfo::id`].
    pub id: u64,
    /// HTTP status code.
    pub status: u16,
    /// Response headers as (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// True if a middleware script denied the request (`req:deny()`). Then
    /// the response is the 403 from the proxy, not a reply from upstream.
    ///
    /// The middleware runs after the proxy sends the [`RequestInfo`]. Thus
    /// that event said `allowed`, and this field overrides it. Always
    /// `false` for a request that the policy denied, because its
    /// `RequestInfo` already shows the denial.
    pub denied: bool,
}

/// Events sent to the TUI thread.
enum TuiEvent {
    /// Process stdout/stderr output bytes.
    Output(Vec<u8>),
    /// Network event for the Monitor tab.
    Network(NetworkEvent),
    /// Guest resource snapshot for the CPU and memory widgets.
    Stats(StatsSnapshot),
    /// Process exited with the given code.
    Exit(i32),
    /// Terminal event from crossterm.
    Terminal(Event),
}

/// Sender of events to the TUI thread.
///
/// All methods are non-blocking, because the channel is unbounded.
#[derive(Clone)]
pub struct TuiSender {
    tx: std_mpsc::Sender<TuiEvent>,
}

impl TuiSender {
    /// Send process output (stdout or stderr) to the TUI for display.
    pub fn send_output(&self, data: Vec<u8>) {
        let _ = self.tx.send(TuiEvent::Output(data));
    }

    /// Send a network event to the Monitor tab.
    pub fn send_network(&self, ev: NetworkEvent) {
        let _ = self.tx.send(TuiEvent::Network(ev));
    }

    /// Send a guest stats snapshot to the Monitor tab.
    pub fn send_stats(&self, snapshot: StatsSnapshot) {
        let _ = self.tx.send(TuiEvent::Stats(snapshot));
    }

    /// Tell the TUI that the sandbox process exited with `code`.
    pub fn send_exit(&self, code: i32) {
        let _ = self.tx.send(TuiEvent::Exit(code));
    }
}

/// Handle to a running TUI thread.
pub struct TuiHandle {
    /// Sender of events to the TUI.
    pub tx: TuiSender,
    join: Option<std::thread::JoinHandle<anyhow::Result<i32>>>,
}

impl TuiHandle {
    /// Block until the TUI thread stops.
    /// Returns:
    ///   Exit code of the sandbox process, or 1 if the TUI stopped for a
    ///   different reason.
    pub fn join(mut self) -> anyhow::Result<i32> {
        match self.join.take() {
            Some(h) => h.join().unwrap_or(Ok(1)),
            None => Ok(1),
        }
    }
}

impl Drop for TuiHandle {
    fn drop(&mut self) {
        // If nobody called join(), wait for the thread here.
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Start the TUI on its own thread.
/// Args:
///  - `stdin_tx`: Channel for keystrokes and resize events to the RPC stdin
///    server
///  - `sig_tx`: Channel for signals that the TUI sends to the sandbox
///    process (for example when the user presses Ctrl+D on the Monitor tab)
///  - `network`: Handle to the live host network state
///  - `project_path`: Project path to show on the Monitor tab
///  - `version`: Airlock version to show on the Monitor tab
///  - `settings`: Runtime settings from the user config.
///
/// Returns:
///   Handle to send events to the TUI and to wait for it to stop.
pub fn spawn(
    stdin_tx: tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: tokio::sync::mpsc::Sender<i32>,
    network: Arc<dyn NetworkControl>,
    project_path: String,
    version: String,
    settings: TuiSettings,
) -> TuiHandle {
    let (tx, rx) = std_mpsc::channel();
    let crossterm_tx = tx.clone();

    let join = std::thread::spawn(move || {
        tui_main(
            rx,
            crossterm_tx,
            stdin_tx,
            sig_tx,
            network,
            project_path,
            version,
            settings,
        )
    });

    TuiHandle {
        tx: TuiSender { tx },
        join: Some(join),
    }
}

/// Entry point of the TUI thread. Runs synchronously and does not use the
/// async runtime.
#[allow(clippy::needless_pass_by_value)] // owned values required by thread::spawn move
#[allow(clippy::too_many_arguments)]
fn tui_main(
    rx: std_mpsc::Receiver<TuiEvent>,
    crossterm_tx: std_mpsc::Sender<TuiEvent>,
    stdin_tx: tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: tokio::sync::mpsc::Sender<i32>,
    network: Arc<dyn NetworkControl>,
    project_path: String,
    version: String,
    settings: TuiSettings,
) -> anyhow::Result<i32> {
    // Enable the alternate screen, raw mode, mouse capture and the kitty
    // keyboard protocol.
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
    // With bracketed paste, crossterm reports a paste as one
    // `Event::Paste(String)`. Without it, a paste becomes many key events.
    // These include the Enter between lines, which runs the pasted code
    // immediately.
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;

    // Restore the terminal on all exit paths. The explicit `Show` after
    // `ratatui::restore()` is necessary. Ratatui may have sent `Hide` in its
    // last frame (when the Monitor tab was active and no cursor was set).
    // Without `Show`, the host terminal cursor stays hidden after exit.
    let result = run_tui_loop(
        &mut terminal,
        &rx,
        crossterm_tx,
        &stdin_tx,
        &sig_tx,
        network,
        project_path,
        version,
        settings,
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
    version: String,
    settings: TuiSettings,
    kitty_enabled: bool,
) -> anyhow::Result<i32> {
    let mut sink = TuiTerminalSink::new(80, 24, settings.scrollback);
    let mut app = App::new(network, project_path, version, settings);

    // Make the vt100 parser the same size as the terminal body area. Skip a
    // zero-sized body. A terminal that is not taller than the tab bar gives
    // a 0x0 body. A vt100 grid with zero rows causes an underflow (panic in
    // debug builds, corrupt data in release builds). The sink keeps its
    // default size until the terminal is large enough.
    let size = terminal.size()?;
    let size = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    let body = ui::body_area(size);
    if body.height > 0 && body.width > 0 {
        sink.resize(body.height, body.width);
    }

    // Crossterm reader thread. It sends terminal events into the same channel
    // as all other TUI events.
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if crossterm_tx.send(TuiEvent::Terminal(ev)).is_err() {
                break;
            }
        }
    });

    loop {
        terminal.draw(|f| ui::render(f, &app, &sink))?;

        // Wait for the next event. Block for 16ms at most, to render at
        // approximately 60 fps.
        let event = match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(ev) => Some(ev),
            Err(std_mpsc::RecvTimeoutError::Timeout) => None,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Ok(1),
        };

        // Process the event (if there is one) and all queued events.
        if let Some(ev) = event
            && let Some(code) = handle_event(
                ev,
                &mut app,
                &mut sink,
                stdin_tx,
                sig_tx,
                terminal,
                kitty_enabled,
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
            )? {
                return Ok(code);
            }
        }
    }
}

/// Process one TUI event.
/// Returns:
///   `Some(exit_code)` if the TUI must stop, else `None`.
#[allow(clippy::too_many_arguments)]
fn handle_event<B: Backend>(
    event: TuiEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: &tokio::sync::mpsc::Sender<i32>,
    terminal: &mut Terminal<B>,
    kitty_enabled: bool,
) -> anyhow::Result<Option<i32>>
where
    B::Error: Send + Sync + 'static,
{
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
            if let Some(code) = handle_key(key, app, sink, stdin_tx, sig_tx, kitty_enabled) {
                return Ok(Some(code));
            }
        }
        TuiEvent::Terminal(Event::Mouse(mouse)) => {
            handle_mouse(mouse, app, sink, stdin_tx, terminal)?;
        }
        TuiEvent::Terminal(Event::Resize(cols, rows)) => {
            let size = ratatui::layout::Rect::new(0, 0, cols, rows);
            let body = ui::body_area(size);
            // Ignore a zero-sized body. A terminal with 2 rows or fewer gives a
            // 0x0 body. A vt100 grid (or guest PTY) with zero size causes a
            // panic or underflow. Keep the last valid size and let the render
            // clip the output.
            if body.height > 0 && body.width > 0 {
                sink.resize(body.height, body.width);
                let _ = stdin_tx.blocking_send(TuiInputEvent::Resize(body.height, body.width));
            }
        }
        TuiEvent::Terminal(Event::Paste(text)) => {
            // Forward a paste only when the Sandbox tab is active. Add the
            // bracketed paste markers only when the guest shell enabled them
            // (`\e[?2004h`). Shells without support (BusyBox ash, dash) parse
            // the markers incorrectly and silently discard the bytes near them.
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

/// Update the guest bracketed paste state from guest PTY output.
///
/// Looks for the DEC private mode sequences that enable or disable bracketed
/// paste (`\e[?2004h` / `\e[?2004l`). The state tells if host pastes get the
/// `\e[200~...\e[201~` markers. Shells without support (BusyBox ash) parse
/// the markers incorrectly and discard the bytes near them.
/// Args:
///  - `data`: Chunk of guest PTY output
///  - `enabled`: Current state. The function changes it when the chunk
///    contains one of the sequences.
fn scan_bracketed_paste_mode(data: &[u8], enabled: &mut bool) {
    // A sequence that is split across two chunks is not found. This is
    // acceptable: the guest sends the sequence again on each prompt redraw,
    // so one miss corrects itself.
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

/// Handle a key event.
/// Returns:
///   `Some(code)` if the TUI must stop, else `None`.
fn handle_key(
    key: KeyEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    sig_tx: &tokio::sync::mpsc::Sender<i32>,
    kitty_enabled: bool,
) -> Option<i32> {
    // Mouse capture is always on, so no key here writes an escape sequence
    // to the terminal. Thus this function has no I/O error to return.
    let action = app.settings.keys.lookup(&key);

    // Global shortcuts.
    match action {
        Some(Action::SwitchSandbox) => {
            app.active_tab = Tab::Sandbox;
            return None;
        }
        Some(Action::SwitchMonitor) => {
            app.active_tab = Tab::Monitor;
            return None;
        }
        _ => {}
    }

    match app.active_tab {
        Tab::Sandbox => {
            // The Sandbox tab forwards all keys except the global shortcuts,
            // so the user cannot rebind other keys here. Send the raw
            // keystroke to the guest PTY.
            if let Some(bytes) = key_to_bytes(key, kitty_enabled) {
                // A key press always goes back to the live view.
                sink.scroll_to_bottom();
                let _ = stdin_tx.blocking_send(TuiInputEvent::Data(bytes));
            }
        }
        Tab::Monitor => {
            handle_monitor_action(action, app, sig_tx);
        }
    }

    None
}

/// Apply an [`Action`] on the Monitor tab.
///
/// The open view (dropdown, details or list) sets the meaning of each
/// action. A key without an action binding (`None`) has no effect.
fn handle_monitor_action(
    action: Option<Action>,
    app: &mut App,
    sig_tx: &tokio::sync::mpsc::Sender<i32>,
) {
    use crate::tabs::monitor::network::NetworkSubTab;

    let Some(action) = action else {
        return;
    };

    if app.monitor.network.dropdown_open() {
        match action {
            Action::SelectUp => app.monitor.network.nudge_policy_highlight(-1),
            Action::SelectDown => app.monitor.network.nudge_policy_highlight(1),
            Action::Confirm => {
                if let Some(p) = app.monitor.network.highlighted_policy() {
                    app.network.set_policy(p);
                }
                app.monitor.network.close_policy_dropdown();
            }
            Action::Cancel | Action::Back => app.monitor.network.close_policy_dropdown(),
            _ => {}
        }
        return;
    }

    if app.monitor.network.details_open() {
        match action {
            // In the details pane, the selection keys scroll the body. There
            // are no rows to select, and long header lists do not fit on one
            // screen.
            Action::SelectUp => app.monitor.network.scroll_details(-1),
            Action::SelectDown => app.monitor.network.scroll_details(1),
            Action::SelectPageUp => app.monitor.network.scroll_details(-20),
            Action::SelectPageDown => app.monitor.network.scroll_details(20),
            Action::SelectNewest => app.monitor.network.scroll_details_to_top(),
            Action::SelectOldest => app.monitor.network.scroll_details_to_bottom(),
            // `Back` and `Cancel` both close the details pane first, and the
            // Monitor tab stays open. A second `Back` from the list view then
            // opens the Sandbox tab (below).
            Action::Cancel | Action::Back => app.monitor.network.close_details(),
            Action::ToggleSubTab => app.monitor.network.toggle_sub_tab(),
            Action::SelectRequests => app.monitor.network.select_sub_tab(NetworkSubTab::Requests),
            Action::SelectConnections => {
                app.monitor
                    .network
                    .select_sub_tab(NetworkSubTab::Connections);
            }
            Action::OpenPolicy => app
                .monitor
                .network
                .open_policy_dropdown(app.network.policy()),
            Action::KillSandbox => {
                let _ = sig_tx.blocking_send(1);
                let _ = sig_tx.blocking_send(15);
            }
            _ => {}
        }
        return;
    }

    match action {
        Action::SelectUp => app.monitor.network.select_up(),
        Action::SelectDown => app.monitor.network.select_down(),
        Action::SelectPageUp => app.monitor.network.select_page_up(),
        Action::SelectPageDown => app.monitor.network.select_page_down(),
        Action::SelectNewest => app.monitor.network.select_newest(),
        Action::SelectOldest => app.monitor.network.select_oldest(),
        Action::Confirm => app.monitor.network.open_details(),
        Action::ToggleSubTab => app.monitor.network.toggle_sub_tab(),
        Action::SelectRequests => app.monitor.network.select_sub_tab(NetworkSubTab::Requests),
        Action::SelectConnections => {
            app.monitor
                .network
                .select_sub_tab(NetworkSubTab::Connections);
        }
        Action::OpenPolicy => app
            .monitor
            .network
            .open_policy_dropdown(app.network.policy()),
        Action::Back => app.active_tab = Tab::Sandbox,
        // Ctrl+D on the Monitor tab tells the sandbox process to exit.
        // Send SIGHUP first. It is the standard "controlling terminal is
        // gone" signal, and interactive shells such as bash exit on it.
        // (They ignore SIGINT and SIGTERM at an idle prompt.) Then send
        // SIGTERM for processes that do not handle SIGHUP. The TUI stops
        // when the exit event of the process comes on the main channel, so
        // do not return early.
        Action::KillSandbox => {
            let _ = sig_tx.blocking_send(1);
            let _ = sig_tx.blocking_send(15);
        }
        _ => {}
    }
}

/// True if mouse events go to the sandboxed program and not to the TUI.
fn guest_owns_mouse(app: &App, sink: &TuiTerminalSink) -> bool {
    // The value comes from the active tab and the parser state each time.
    // Thus there is no field on `App` to keep in sync.
    //
    // The guest's mouse mode is part of the check on purpose. It makes the
    // forwarding safe. Without it, a program that did not enable mouse
    // reporting gets `\e[<64;10;5M` as literal keystrokes at its prompt.
    // Also, the user cannot get to the TUI scrollback in a plain shell.
    app.active_tab == Tab::Sandbox && sink.mouse_protocol_mode() != MouseProtocolMode::None
}

/// Handle a mouse event. Forwards it to the guest, or applies it to the TUI.
fn handle_mouse<B: Backend>(
    mouse: MouseEvent,
    app: &mut App,
    sink: &mut TuiTerminalSink,
    stdin_tx: &tokio::sync::mpsc::Sender<TuiInputEvent>,
    terminal: &mut Terminal<B>,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let size = terminal.size()?;
    let size = ratatui::layout::Rect::new(0, 0, size.width, size.height);

    // A left click often means that the user wants to select text. Thus
    // this is the time to show the selection hint. Record the click before
    // the forwarding branch below. That branch is the usual case. A hint
    // that shows only when the click does *not* go to the guest is wrong.
    if matches!(
        mouse.kind,
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) {
        app.select_hint_at = Some(std::time::Instant::now());
    }

    // The mouse belongs to the sandboxed program. Encode the event again
    // and send it to its PTY. Do not apply it here. `encode` returns `None`
    // for events outside the body rect, so the tab bar still switches tabs.
    //
    // Do not forward in a scrolled-back view. There, the rows on the screen
    // are not the same as the guest rows, so the coordinates are wrong. The
    // code below lets the wheel scroll the view down to the live screen.
    // Then forwarding starts again.
    if guest_owns_mouse(app, sink)
        && sink.scrollback() == 0
        && let Some(bytes) = mouse::encode(
            mouse,
            sink.mouse_protocol_mode(),
            sink.mouse_protocol_encoding(),
            ui::body_area(size),
        )
    {
        let _ = stdin_tx.blocking_send(TuiInputEvent::Data(bytes));
        return Ok(());
    }

    let tab_rects = ui::tab_header_rects(size, app);

    match mouse.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            // When the policy dropdown is open, it gets all clicks. A click
            // on a row selects it. All clicks close the dropdown.
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
            // A click on the policy title opens the dropdown.
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
            // Close button (×) of the details sub-tab. Check it before the
            // general sub-tab hit test. The × is inside the details label
            // rect, and the close must have priority over a new selection
            // of the active details tab.
            if app.active_tab == Tab::Monitor
                && app
                    .monitor
                    .network
                    .is_details_close(mouse.column, mouse.row)
            {
                app.monitor.network.close_details();
                return Ok(());
            }
            // Click on a sub-tab of the Monitor tab.
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
            // A click in a body area is not for the TUI. The user probably
            // wants to select text. Mouse capture stays on, and the
            // terminal's own modifier key controls the drag. The only
            // response is the hint that the code recorded above.
        }
        // In the details pane, the wheel scrolls the body. In the list
        // views, it moves the selection.
        MouseEventKind::ScrollUp => match app.active_tab {
            Tab::Monitor if app.monitor.network.details_open() => {
                app.monitor.network.scroll_details(-3);
            }
            Tab::Monitor => {
                for _ in 0..3 {
                    app.monitor.network.select_up();
                }
            }
            Tab::Sandbox => sink.scroll_up(3),
        },
        MouseEventKind::ScrollDown => match app.active_tab {
            Tab::Monitor if app.monitor.network.details_open() => {
                app.monitor.network.scroll_details(3);
            }
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
/// Args:
///  - `key`: Key event from crossterm
///  - `kitty_enabled`: True if the host terminal supports the kitty
///    keyboard protocol.
///
/// Returns:
///   Encoded bytes, or `None` if the key has no encoding.
fn key_to_bytes(key: KeyEvent, kitty_enabled: bool) -> Option<Vec<u8>> {
    // Use the legacy Xterm encoding by default, because most guests support
    // it. Use the Kitty CSI-u encoding only for keys where Xterm loses the
    // modifier. Xterm cannot encode SHIFT on Enter, Backspace, Escape or
    // Space: they give the same byte with or without Shift. All other keys
    // (Ctrl+key, Alt+key, arrows and function keys with modifiers) have a
    // correct Xterm encoding.
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

#[cfg(test)]
mod test_cfg;
#[cfg(test)]
mod tests;
