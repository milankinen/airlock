//! End-to-end tests of the dialog on a real X server (Xvfb).
//!
//! Each test starts its own X server, runs the dialog in a thread and acts
//! as the user with the XTEST extension. A host with no Xvfb skips the
//! tests, unless the `AIRLOCK_REQUIRE_XVFB` variable is set.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use airlock_linux_dialog::{Answer, DiffKind, DiffLine, Error, Item, Request, show_on};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const MOTION: u8 = 6;
/// Key code of Escape on the key map of Xvfb.
const KEYCODE_ESCAPE: u8 = 9;
/// Key code of Return on the key map of Xvfb.
const KEYCODE_RETURN: u8 = 36;
/// Colors of the dialog, as `0x00RRGGBB`. They must agree with the colors
/// of the dialog look.
const COLOR_FOOTER_LINE: u32 = 0x808080;
const COLOR_DEFAULT_BORDER: u32 = 0x0a4a8a;
const COLOR_BUTTON: u32 = 0xe4e4e4;
/// Variable that makes a missing Xvfb an error, for example in CI.
const REQUIRE_XVFB: &str = "AIRLOCK_REQUIRE_XVFB";

/// X server that stops when it drops.
struct Server {
    child: Child,
    display: String,
}

impl Server {
    /// Start Xvfb. `None` when the host has no Xvfb.
    fn start() -> Option<Self> {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Xvfb writes the display number when it is ready.
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let display = format!(":{}", line.trim());
        Some(Self { child, display })
    }
}

/// Start Xvfb for a test. `None` when the host has no Xvfb and the
/// `AIRLOCK_REQUIRE_XVFB` variable is not set.
fn start_server() -> Option<Server> {
    let server = Server::start();
    if server.is_none() {
        assert!(
            std::env::var_os(REQUIRE_XVFB).is_none(),
            "Xvfb did not start and {REQUIRE_XVFB} is set"
        );
        eprintln!("skipped: the host has no Xvfb");
    }
    server
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The user: a second connection to the server.
struct User {
    conn: RustConnection,
    root: Window,
}

impl User {
    fn new(server: &Server) -> Self {
        let (conn, num) = RustConnection::connect(Some(&server.display)).unwrap();
        let root = conn.setup().roots[num].root;
        Self { conn, root }
    }

    /// Wait for the window of the dialog and return it with its size.
    fn wait_for_window(&self) -> (Window, i32, i32) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            let tree = self.conn.query_tree(self.root).unwrap().reply().unwrap();
            for win in tree.children {
                let class = self
                    .conn
                    .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 16)
                    .unwrap()
                    .reply()
                    .unwrap();
                let attrs = self
                    .conn
                    .get_window_attributes(win)
                    .unwrap()
                    .reply()
                    .unwrap();
                if class.value.starts_with(b"airlock")
                    && attrs.map_state == x11rb::protocol::xproto::MapState::VIEWABLE
                {
                    let g = self.conn.get_geometry(win).unwrap().reply().unwrap();
                    return (win, i32::from(g.x), i32::from(g.y));
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the dialog window did not show");
    }

    fn size(&self, win: Window) -> (i32, i32) {
        let g = self.conn.get_geometry(win).unwrap().reply().unwrap();
        (i32::from(g.width), i32::from(g.height))
    }

    /// Move the pointer to a point in the window.
    fn point(&self, x: i32, y: i32) {
        self.conn
            .xtest_fake_input(MOTION, 0, 0, self.root, x as i16, y as i16, 0)
            .unwrap();
        self.conn.flush().unwrap();
    }

    fn click(&self, x: i32, y: i32) {
        self.point(x, y);
        for kind in [BUTTON_PRESS, BUTTON_RELEASE] {
            self.conn
                .xtest_fake_input(kind, 1, 0, self.root, 0, 0, 0)
                .unwrap();
        }
        self.conn.flush().unwrap();
    }

    fn key(&self, code: u8) {
        for kind in [KEY_PRESS, KEY_RELEASE] {
            self.conn
                .xtest_fake_input(kind, code, 0, self.root, 0, 0, 0)
                .unwrap();
        }
        self.conn.flush().unwrap();
    }

    /// Get the pixels of the window as `0x00RRGGBB`, row by row.
    fn pixels(&self, win: Window) -> Vec<u32> {
        let (w, h) = self.size(win);
        let image = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, win, 0, 0, w as u16, h as u16, !0)
            .unwrap()
            .reply()
            .unwrap();
        // Xvfb with depth 24 gives 32 bit pixels in little-endian order.
        image
            .data
            .chunks_exact(4)
            .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]) & 0xff_ffff)
            .collect()
    }

    /// Count the pixels of the window that are dark: text.
    fn dark_pixels(&self, win: Window) -> usize {
        self.pixels(win)
            .iter()
            .filter(|&&p| p >> 16 < 90 && (p >> 8) & 0xff < 90 && p & 0xff < 90)
            .count()
    }

    /// Find the deny and allow buttons from the pixels of the window.
    ///
    /// Returns:
    ///   The centers of the deny and the allow button, in window
    ///   coordinates.
    fn buttons(&self, win: Window) -> ((i32, i32), (i32, i32)) {
        let (w, h) = self.size(win);
        let pixels = self.pixels(win);
        let at = |x: i32, y: i32| pixels[(y * w + x) as usize];
        // The footer starts at a line across the full width.
        let footer = (0..h)
            .rev()
            .find(|&y| (0..w).all(|x| at(x, y) == COLOR_FOOTER_LINE))
            .expect("the dialog has no footer line");
        let bounds = |color: u32, min_x: i32| {
            let points: Vec<(i32, i32)> = (footer + 1..h)
                .flat_map(|y| (min_x..w).map(move |x| (x, y)))
                .filter(|&(x, y)| at(x, y) == color)
                .collect();
            assert!(!points.is_empty(), "no button with color {color:06x}");
            let x0 = points.iter().map(|p| p.0).min().unwrap();
            let x1 = points.iter().map(|p| p.0).max().unwrap();
            let y0 = points.iter().map(|p| p.1).min().unwrap();
            let y1 = points.iter().map(|p| p.1).max().unwrap();
            (x0, x1, y0, y1)
        };
        // Deny is the default button and has the thick colored border.
        let deny = bounds(COLOR_DEFAULT_BORDER, 0);
        let allow = bounds(COLOR_BUTTON, deny.1 + 1);
        let center = |(x0, x1, y0, y1): (i32, i32, i32, i32)| (x0.midpoint(x1), y0.midpoint(y1));
        (center(deny), center(allow))
    }
}

fn request(timeout_ms: u64) -> Request {
    let diff = |kind, text: &str| DiffLine {
        kind,
        text: text.into(),
    };
    Request {
        title: "Network access requested".into(),
        items: vec![
            Item {
                label: "Command".into(),
                value: "curl https://example.com/".repeat(20),
                diff: None,
            },
            Item {
                label: "Changes".into(),
                value: String::new(),
                diff: Some(vec![
                    diff(DiffKind::Remove, "allow = []"),
                    diff(DiffKind::Add, "allow = [\"example.com\"]"),
                ]),
            },
        ],
        allow: "Allow".into(),
        deny: "Deny".into(),
        countdown: "Denies in".into(),
        timeout_ms,
    }
}

/// Run the dialog in a thread, like the airlock process does.
fn spawn_dialog(server: &Server, timeout_ms: u64) -> JoinHandle<Result<Answer, Error>> {
    let display = server.display.clone();
    std::thread::spawn(move || show_on(Some(&display), request(timeout_ms)))
}

/// Wait until the guard time after the first show has passed and the
/// dialog drew its text.
fn wait_for_input_ready(user: &User, win: Window) {
    let end = Instant::now() + Duration::from_secs(10);
    while user.dark_pixels(win) == 0 {
        assert!(Instant::now() < end, "the dialog drew no text");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(800));
}

/// Test that a click on the allow button gives the allow answer.
///   1. Show a request with a long command and a change
///   2. Check that the window has text
///   3. Click the allow button at the bottom right
///   4. Check the answer
#[test]
fn click_on_allow_button_allows() {
    let Some(server) = start_server() else {
        return;
    };
    let user = User::new(&server);
    let dialog = spawn_dialog(&server, 30_000);
    let (win, x, y) = user.wait_for_window();
    wait_for_input_ready(&user, win);
    let (_, (ax, ay)) = user.buttons(win);
    user.click(x + ax, y + ay);
    assert_eq!(dialog.join().unwrap(), Ok(Answer::Allow));
}

/// Test that the deny button, Escape and the Return key behave as
/// specified: deny and Escape deny, Return does nothing.
///   1. Press Return and check that the dialog stays open
///   2. Click the deny button and check the answer
///   3. Show again and press Escape
#[test]
fn deny_button_and_escape_deny_and_return_does_nothing() {
    let Some(server) = start_server() else {
        return;
    };
    let user = User::new(&server);
    let dialog = spawn_dialog(&server, 30_000);
    let (win, x, y) = user.wait_for_window();
    wait_for_input_ready(&user, win);
    let (w, h) = user.size(win);
    user.point(x + w / 2, y + h / 2);
    user.key(KEYCODE_RETURN);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!dialog.is_finished());
    let ((dx, dy), _) = user.buttons(win);
    user.click(x + dx, y + dy);
    assert_eq!(dialog.join().unwrap(), Ok(Answer::Deny));

    let dialog = spawn_dialog(&server, 30_000);
    let (win, x, y) = user.wait_for_window();
    wait_for_input_ready(&user, win);
    let (w, h) = user.size(win);
    user.point(x + w / 2, y + h / 2);
    user.key(KEYCODE_ESCAPE);
    assert_eq!(dialog.join().unwrap(), Ok(Answer::Deny));
}

/// Test that the dialog ends with a timeout answer when nobody answers.
///   1. Show a request with a short time limit
///   2. Check the answer
#[test]
fn no_answer_in_time_gives_timeout() {
    let Some(server) = start_server() else {
        return;
    };
    let dialog = spawn_dialog(&server, 700);
    assert_eq!(dialog.join().unwrap(), Ok(Answer::Timeout));
}

/// Test that a missing X server is an error, which the caller reads as
/// deny.
///   1. Show a request on a display that does not exist
///   2. Check that the call returns an error
#[test]
fn missing_x_server_is_an_error() {
    assert!(show_on(Some(":987"), request(1000)).is_err());
}
