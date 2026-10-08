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

/// Host network state as the TUI sees it: only the policy.
pub(crate) struct FakeNetwork(Mutex<Policy>);

impl NetworkControl for FakeNetwork {
    fn policy(&self) -> Policy {
        *self.0.lock().unwrap()
    }

    fn set_policy(&self, policy: Policy) {
        *self.0.lock().unwrap() = policy;
    }
}

/// The TUI event loop on an in-memory terminal. Each event is preceded by
/// a render, like in the real loop, so click targets match what is drawn.
pub(crate) struct Tui {
    pub app: App,
    pub sink: TuiTerminalSink,
    pub network: Arc<FakeNetwork>,
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

    /// Render, then handle one event. Returns the exit code when the TUI
    /// exits.
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

    pub fn output(&mut self, bytes: &[u8]) {
        self.send(TuiEvent::Output(bytes.to_vec()));
    }

    pub fn network_event(&mut self, event: NetworkEvent) {
        self.send(TuiEvent::Network(event));
    }

    pub fn stats(&mut self, snapshot: StatsSnapshot) {
        self.send(TuiEvent::Stats(snapshot));
    }

    pub fn key(&mut self, code: KeyCode) {
        self.key_with(code, KeyModifiers::NONE);
    }

    pub fn key_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.send(TuiEvent::Terminal(Event::Key(KeyEvent::new(
            code, modifiers,
        ))));
    }

    /// Press each char of `keys` in turn.
    pub fn type_keys(&mut self, keys: &str) {
        for c in keys.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    pub fn mouse(&mut self, kind: MouseEventKind, (column, row): (u16, u16)) {
        self.send(TuiEvent::Terminal(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })));
    }

    pub fn click(&mut self, at: (u16, u16)) {
        self.mouse(MouseEventKind::Down(MouseButton::Left), at);
    }

    /// Left-click the first rendered occurrence of `text`.
    pub fn click_text(&mut self, text: &str) {
        let at = self.find(text).unwrap_or_else(|| {
            let screen = self.screen();
            panic!("{text:?} not on screen:\n{screen}");
        });
        self.click(at);
    }

    pub fn paste(&mut self, text: &str) {
        self.send(TuiEvent::Terminal(Event::Paste(text.into())));
    }

    /// Resize the terminal, like the host terminal does.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal.backend_mut().resize(cols, rows);
        self.send(TuiEvent::Terminal(Event::Resize(cols, rows)));
    }

    fn draw(&mut self) {
        let (app, sink) = (&self.app, &self.sink);
        self.terminal.draw(|f| ui::render(f, app, sink)).unwrap();
    }

    /// Render and return the screen as text, one line per row.
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

    /// Render and return row `y` with trailing spaces trimmed.
    pub fn row(&mut self, y: u16) -> String {
        self.screen()
            .lines()
            .nth(usize::from(y))
            .unwrap_or("")
            .trim_end()
            .to_string()
    }

    /// Render and return the first row that contains `text`.
    pub fn row_with(&mut self, text: &str) -> String {
        let screen = self.screen();
        screen
            .lines()
            .find(|l| l.contains(text))
            .unwrap_or_else(|| panic!("{text:?} not on screen:\n{screen}"))
            .trim_end()
            .to_string()
    }

    /// Cell `(column, row)` where `text` first starts on the rendered screen.
    pub fn find(&mut self, text: &str) -> Option<(u16, u16)> {
        let screen = self.screen();
        screen.lines().enumerate().find_map(|(y, line)| {
            let byte = line.find(text)?;
            let col = line[..byte].chars().count();
            Some((u16::try_from(col).unwrap(), u16::try_from(y).unwrap()))
        })
    }

    /// Events sent towards the sandbox since the last call.
    pub fn sent(&mut self) -> Vec<TuiInputEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.stdin_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Bytes sent towards the sandbox since the last call, joined.
    pub fn sent_bytes(&mut self) -> Vec<u8> {
        self.sent()
            .into_iter()
            .flat_map(|ev| match ev {
                TuiInputEvent::Data(d) => d,
                TuiInputEvent::Resize(..) => Vec::new(),
            })
            .collect()
    }

    /// Signals sent to the sandbox process since the last call.
    pub fn signals(&mut self) -> Vec<i32> {
        let mut out = Vec::new();
        while let Ok(s) = self.sig_rx.try_recv() {
            out.push(s);
        }
        out
    }
}
