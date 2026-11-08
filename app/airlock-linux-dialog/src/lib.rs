//! Window that shows a request to the user and asks for allow or deny.
//!
//! The caller gives a title, labeled values and a time limit. The dialog
//! shows all values in full, with scrolling for long text and colors for
//! changes. The user selects allow or deny. No answer in time, a closed
//! window and every error mean deny.
//!
//! The dialog uses only an X server. It has no C library dependency, so
//! the same build runs on any Linux host.

mod layout;
mod protocol;
mod render;
mod ui;
mod x11;

pub use protocol::{Answer, DiffKind, DiffLine, Error, Item, Request};
pub use x11::{show, show_on};
