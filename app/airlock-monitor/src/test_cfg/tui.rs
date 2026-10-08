//! A monitor TUI on a test terminal, driven by real terminal events.

use std::sync::{Arc, Mutex};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::app::App;
use crate::pty::TuiTerminalSink;
use crate::{
    NetworkControl, NetworkEvent, Policy, StatsSnapshot, TuiEvent, TuiInputEvent, TuiSettings,
    handle_event, ui,
};

/// Network state of the host, as the TUI sees it. It holds only the policy.
pub(crate) struct FakeNetwork(Mutex<Policy>);

impl NetworkControl for FakeNetwork {
    fn policy(&self) -> Policy {
        *self.0.lock().unwrap()
    }

    fn set_policy(&self, policy: Policy) {
        *self.0.lock().unwrap() = policy;
    }
}

/// The TUI event loop on a terminal in memory. It draws the screen before
/// each event, as the real loop does. Thus click targets agree with the
/// screen.
pub(crate) struct Tui {
    /// TUI application state.
    pub app: App,
    /// Virtual terminal of the sandbox process.
    pub sink: TuiTerminalSink,
    /// Network state that the TUI reads and changes.
    pub network: Arc<FakeNetwork>,
    /// True if the host terminal supports the kitty keyboard protocol.
    pub kitty: bool,
    terminal: Terminal<TestBackend>,
    stdin_tx: mpsc::Sender<TuiInputEvent>,
    stdin_rx: mpsc::Receiver<TuiInputEvent>,
    sig_tx: mpsc::Sender<i32>,
    sig_rx: mpsc::Receiver<i32>,
}

impl Tui {
    /// A 120x30 terminal with default settings, on the Sandbox tab.
    pub fn new() -> Self {
        Self::with(120, 30, TuiSettings::default())
    }

    /// A `cols` x `rows` terminal with `settings`, on the Sandbox tab.
    pub fn with(cols: u16, rows: u16, settings: TuiSettings) -> Self {
        let network = Arc::new(FakeNetwork(Mutex::new(Policy::AllowByDefault)));
        let mut sink = TuiTerminalSink::new(80, 24, settings.scrollback);
        let body = ui::body_area(Rect::new(0, 0, cols, rows));
        sink.resize(body.height, body.width);
        let app = App::new(
            network.clone(),
            "/work/project".into(),
            "1.2.3".into(),
            settings,
        );
        let (stdin_tx, stdin_rx) = mpsc::channel(1024);
        let (sig_tx, sig_rx) = mpsc::channel(16);
        Self {
            app,
            sink,
            network,
            kitty: false,
            terminal: Terminal::new(TestBackend::new(cols, rows)).unwrap(),
            stdin_tx,
            stdin_rx,
            sig_tx,
            sig_rx,
        }
    }

    /// Draw the screen, then handle one event. Returns the exit code when
    /// the TUI exits.
    pub fn send(&mut self, event: TuiEvent) -> Option<i32> {
        self.draw();
        handle_event(
            event,
            &mut self.app,
            &mut self.sink,
            &self.stdin_tx,
            &self.sig_tx,
            &mut self.terminal,
            self.kitty,
        )
        .unwrap()
    }

    /// Send sandbox output to the terminal.
    pub fn output(&mut self, bytes: &[u8]) {
        self.send(TuiEvent::Output(bytes.to_vec()));
    }

    /// Send one network event.
    pub fn network_event(&mut self, event: NetworkEvent) {
        self.send(TuiEvent::Network(event));
    }

    /// Send one statistics snapshot.
    pub fn stats(&mut self, snapshot: StatsSnapshot) {
        self.send(TuiEvent::Stats(snapshot));
    }

    /// Press one key without modifiers.
    pub fn key(&mut self, code: KeyCode) {
        self.key_with(code, KeyModifiers::NONE);
    }

    /// Press one key with `modifiers`.
    pub fn key_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.send(TuiEvent::Terminal(Event::Key(KeyEvent::new(
            code, modifiers,
        ))));
    }

    /// Press each character of `keys`, in order.
    pub fn type_keys(&mut self, keys: &str) {
        for c in keys.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    /// Send one mouse event at cell `(column, row)`.
    pub fn mouse(&mut self, kind: MouseEventKind, (column, row): (u16, u16)) {
        self.send(TuiEvent::Terminal(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })));
    }

    /// Press the left mouse button at cell `at`.
    pub fn click(&mut self, at: (u16, u16)) {
        self.mouse(MouseEventKind::Down(MouseButton::Left), at);
    }

    /// Click the first place where `text` is on the screen. Panics if the
    /// screen does not show `text`.
    pub fn click_text(&mut self, text: &str) {
        let at = self.find(text).unwrap_or_else(|| {
            let screen = self.screen();
            panic!("{text:?} not on screen:\n{screen}");
        });
        self.click(at);
    }

    /// Send a bracketed paste of `text`.
    pub fn paste(&mut self, text: &str) {
        self.send(TuiEvent::Terminal(Event::Paste(text.into())));
    }

    /// Change the terminal size, as the host terminal does.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal.backend_mut().resize(cols, rows);
        self.send(TuiEvent::Terminal(Event::Resize(cols, rows)));
    }

    /// Draw the UI into the test terminal.
    fn draw(&mut self) {
        let (app, sink) = (&self.app, &self.sink);
        self.terminal.draw(|f| ui::render(f, app, sink)).unwrap();
    }

    /// Draw and return the screen as text, one line for each row.
    pub fn screen(&mut self) -> String {
        self.draw();
        let buf = self.terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Draw and return row `y` without spaces at the end.
    pub fn row(&mut self, y: u16) -> String {
        self.screen()
            .lines()
            .nth(usize::from(y))
            .unwrap_or("")
            .trim_end()
            .to_string()
    }

    /// Draw and return the first row that contains `text`. Panics if no
    /// row contains it.
    pub fn row_with(&mut self, text: &str) -> String {
        let screen = self.screen();
        screen
            .lines()
            .find(|l| l.contains(text))
            .unwrap_or_else(|| panic!("{text:?} not on screen:\n{screen}"))
            .trim_end()
            .to_string()
    }

    /// Return the cell `(column, row)` where `text` first starts on the
    /// screen.
    pub fn find(&mut self, text: &str) -> Option<(u16, u16)> {
        let screen = self.screen();
        screen.lines().enumerate().find_map(|(y, line)| {
            let byte = line.find(text)?;
            let col = line[..byte].chars().count();
            Some((u16::try_from(col).unwrap(), u16::try_from(y).unwrap()))
        })
    }

    /// Return the events sent to the sandbox since the last call.
    pub fn sent(&mut self) -> Vec<TuiInputEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.stdin_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Return the input bytes sent to the sandbox since the last call, as
    /// one buffer. Resize events are ignored.
    pub fn sent_bytes(&mut self) -> Vec<u8> {
        self.sent()
            .into_iter()
            .flat_map(|ev| match ev {
                TuiInputEvent::Data(d) => d,
                TuiInputEvent::Resize(..) => Vec::new(),
            })
            .collect()
    }

    /// Return the signals sent to the sandbox process since the last call.
    pub fn signals(&mut self) -> Vec<i32> {
        let mut out = Vec::new();
        while let Ok(s) = self.sig_rx.try_recv() {
            out.push(s);
        }
        out
    }
}
