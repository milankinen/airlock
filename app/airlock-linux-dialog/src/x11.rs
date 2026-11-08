//! X11 window of the dialog.
//!
//! Opens one window, draws the dialog into it and turns events into an
//! answer. Needs only an X server. A desktop with Wayland needs the X
//! compatibility layer, which most of them have.

use std::time::{Duration, Instant};

use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, ImageOrder,
    PropMode, Screen, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::protocol::{Answer, Error, Request};
use crate::render::{Canvas, Rect};
use crate::ui::{Key, Ui};

/// Time between two checks for events.
const TICK: Duration = Duration::from_millis(20);

/// Keysym values of the keys that the dialog uses.
const KEYSYM_ESCAPE: u32 = 0xff1b;
const KEYSYM_HOME: u32 = 0xff50;
const KEYSYM_UP: u32 = 0xff52;
const KEYSYM_DOWN: u32 = 0xff54;
const KEYSYM_PAGE_UP: u32 = 0xff55;
const KEYSYM_PAGE_DOWN: u32 = 0xff56;
const KEYSYM_END: u32 = 0xff57;

/// Show the dialog on the display of the `DISPLAY` variable and wait for
/// the answer.
/// Args:
///  - `request`: Request to show. Its time limit ends the wait.
pub fn show(request: Request) -> Result<Answer, Error> {
    show_on(None, request)
}

/// Show the dialog and wait for the answer.
/// Args:
///  - `display`: Name of the X display, for example `:1`. `None` reads the
///    `DISPLAY` variable.
///  - `request`: Request to show. Its time limit ends the wait.
pub fn show_on(display: Option<&str>, request: Request) -> Result<Answer, Error> {
    let timeout = Duration::from_millis(request.timeout_ms);
    let (conn, screen_num) = RustConnection::connect(display)?;
    let screen = conn.setup().roots[screen_num].clone();

    // The canvas has 32 bit pixels in little-endian order. This is the
    // format of all common X servers.
    let depth = screen.root_depth;
    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|f| f.depth == depth)
        .filter(|f| f.bits_per_pixel == 32)
        .filter(|_| conn.setup().image_byte_order == ImageOrder::LSB_FIRST);
    if format.is_none() {
        return Err(Error(format!(
            "the X server has no 32 bit format for depth {depth}"
        )));
    }

    let mut ui = Ui::new(request, dpi_scale(&conn, screen.root));
    let (min_w, min_h) = ui.minimum_size();
    let (w, h) = ui.preferred_size();
    let monitor = monitor_area(&conn, &screen);
    let (w, h) = (
        w.min(monitor.w - 40).max(min_w),
        h.min(monitor.h - 80).max(min_h),
    );
    ui.resize(w, h);

    let win = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        screen.root,
        (monitor.x + (monitor.w - w) / 2) as i16,
        (monitor.y + (monitor.h - h) / 3) as i16,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .background_pixel(screen.white_pixel)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::KEY_PRESS
                    | EventMask::BUTTON_PRESS
                    | EventMask::STRUCTURE_NOTIFY,
            ),
    )?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;
    let (wm_protocols, wm_delete) = set_properties(&conn, win, &ui)?;
    let keys = KeyMap::load(&conn)?;
    conn.map_window(win)?;
    conn.flush()?;

    let started = Instant::now();
    // Time of the first draw request from the server. The window is then
    // visible. The click guard starts here, not at the map request,
    // because the window manager can show the window much later.
    let mut shown: Option<Instant> = None;
    let mut canvas = Canvas::new(w as usize, h as usize, 0);
    let mut dirty = true;
    let mut shown_secs = u64::MAX;
    loop {
        let left = timeout.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(Answer::Timeout);
        }
        while let Some(event) = conn.poll_for_event()? {
            let answer = match event {
                Event::Expose(_) => {
                    shown.get_or_insert_with(Instant::now);
                    dirty = true;
                    None
                }
                Event::ConfigureNotify(e) => {
                    let (w, h) = (i32::from(e.width), i32::from(e.height));
                    if (w, h) != (canvas.width as i32, canvas.height as i32) {
                        ui.resize(w, h);
                        canvas = Canvas::new(w as usize, h as usize, 0);
                        dirty = true;
                    }
                    None
                }
                Event::ButtonPress(e) => match e.detail {
                    1 => ui.click(
                        i32::from(e.event_x),
                        i32::from(e.event_y),
                        shown.map_or(Duration::ZERO, |t| t.elapsed()),
                    ),
                    4 | 5 => {
                        ui.wheel(if e.detail == 4 { -1 } else { 1 });
                        dirty = true;
                        None
                    }
                    _ => None,
                },
                Event::KeyPress(e) => {
                    dirty = true;
                    keys.key(e.detail).and_then(|key| ui.key(key))
                }
                Event::ClientMessage(e)
                    if e.type_ == wm_protocols && e.data.as_data32()[0] == wm_delete =>
                {
                    Some(Answer::Deny)
                }
                _ => None,
            };
            if let Some(answer) = answer {
                conn.destroy_window(win)?;
                conn.flush()?;
                return Ok(answer);
            }
        }
        // The countdown changes once in a second.
        if left.as_secs() != shown_secs {
            shown_secs = left.as_secs();
            dirty = true;
        }
        if dirty {
            ui.draw(&mut canvas, left);
            present(&conn, win, gc, depth, &canvas)?;
            dirty = false;
        }
        std::thread::sleep(TICK);
    }
}

/// Set the properties that the window manager reads.
///
/// Returns:
///   The atoms of the window manager protocol messages and of the close
///   request in them.
fn set_properties(conn: &RustConnection, win: u32, ui: &Ui) -> Result<(u32, u32), Error> {
    let atom = |name: &str| -> Result<u32, Error> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let title = &ui.title();
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        atom("_NET_WM_NAME")?,
        atom("UTF8_STRING")?,
        title.as_bytes(),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"airlock\0airlock\0",
    )?;
    let wm_delete = atom("WM_DELETE_WINDOW")?;
    let wm_protocols = atom("WM_PROTOCOLS")?;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        win,
        atom("_NET_WM_WINDOW_TYPE")?,
        AtomEnum::ATOM,
        &[atom("_NET_WM_WINDOW_TYPE_DIALOG")?],
    )?;
    // Ask to show above the other windows, so that the user sees it.
    conn.change_property32(
        PropMode::REPLACE,
        win,
        atom("_NET_WM_STATE")?,
        AtomEnum::ATOM,
        &[atom("_NET_WM_STATE_ABOVE")?],
    )?;
    let (min_w, min_h) = ui.minimum_size();
    let mut hints = WmSizeHints::new();
    hints.min_size = Some((min_w, min_h));
    hints.set_normal_hints(conn, win)?;
    Ok((wm_protocols, wm_delete))
}

/// Get the area of the monitor that has the pointer, so that the window
/// shows where the user works. The whole screen when the server does not
/// tell the monitors.
fn monitor_area(conn: &RustConnection, screen: &Screen) -> Rect {
    let whole = Rect::new(
        0,
        0,
        i32::from(screen.width_in_pixels),
        i32::from(screen.height_in_pixels),
    );
    let pointer = conn
        .query_pointer(screen.root)
        .ok()
        .and_then(|c| c.reply().ok())
        .filter(|p| p.same_screen);
    // Monitor information needs version 1.5 of the extension.
    let monitors = conn
        .randr_query_version(1, 5)
        .ok()
        .and_then(|c| c.reply().ok())
        .filter(|v| (v.major_version, v.minor_version) >= (1, 5))
        .and_then(|_| conn.randr_get_monitors(screen.root, true).ok())
        .and_then(|c| c.reply().ok());
    let (Some(pointer), Some(monitors)) = (pointer, monitors) else {
        return whole;
    };
    let (px, py) = (i32::from(pointer.root_x), i32::from(pointer.root_y));
    monitors
        .monitors
        .iter()
        .map(|m| {
            Rect::new(
                i32::from(m.x),
                i32::from(m.y),
                i32::from(m.width),
                i32::from(m.height),
            )
        })
        .find(|r| r.contains(px, py))
        .unwrap_or(whole)
}

/// Send the canvas to the window, in bands that fit the request size
/// limit of the server.
fn present(
    conn: &RustConnection,
    win: u32,
    gc: u32,
    depth: u8,
    canvas: &Canvas,
) -> Result<(), Error> {
    let row_bytes = canvas.width * 4;
    let rows = (conn.maximum_request_bytes().saturating_sub(64) / row_bytes).max(1);
    let mut y = 0;
    while y < canvas.height {
        let end = (y + rows).min(canvas.height);
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            win,
            gc,
            canvas.width as u16,
            (end - y) as u16,
            0,
            y as i16,
            0,
            depth,
            &canvas.to_bytes(y..end),
        )?;
        y = end;
    }
    conn.flush()?;
    Ok(())
}

/// Get the scale of the screen from the `Xft.dpi` resource. 1.0 when the
/// server has no value.
fn dpi_scale(conn: &RustConnection, root: u32) -> f32 {
    let resources = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            16384,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
        .unwrap_or_default();
    parse_dpi(&resources).map_or(1.0, |dpi| (dpi / 96.0).clamp(1.0, 4.0))
}

/// Find `Xft.dpi` in the resource text of the server.
fn parse_dpi(resources: &str) -> Option<f32> {
    resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Table that gives the key for a key code.
struct KeyMap {
    first: u8,
    per_code: usize,
    keysyms: Vec<u32>,
}

impl KeyMap {
    fn load(conn: &RustConnection) -> Result<Self, Error> {
        let setup = conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let reply = conn.get_keyboard_mapping(first, count)?.reply()?;
        Ok(Self {
            first,
            per_code: usize::from(reply.keysyms_per_keycode).max(1),
            keysyms: reply.keysyms,
        })
    }

    /// Get the key of a key code. `None` for the keys that the dialog does
    /// not use.
    fn key(&self, code: u8) -> Option<Key> {
        let index = usize::from(code.checked_sub(self.first)?) * self.per_code;
        match *self.keysyms.get(index)? {
            KEYSYM_ESCAPE => Some(Key::Escape),
            KEYSYM_UP => Some(Key::Up),
            KEYSYM_DOWN => Some(Key::Down),
            KEYSYM_PAGE_UP => Some(Key::PageUp),
            KEYSYM_PAGE_DOWN => Some(Key::PageDown),
            KEYSYM_HOME => Some(Key::Home),
            KEYSYM_END => Some(Key::End),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the scale comes from the resource text.
    ///   1. Read `Xft.dpi` from text with other resources
    ///   2. Check a missing and an invalid value
    #[test]
    fn dpi_is_read_from_resources() {
        assert_eq!(parse_dpi("Xft.antialias:\t1\nXft.dpi:\t144\n"), Some(144.0));
        assert_eq!(parse_dpi("Xft.antialias:\t1\n"), None);
        assert_eq!(parse_dpi("Xft.dpi:\tabc\n"), None);
    }
}
