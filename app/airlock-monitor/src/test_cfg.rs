//! Crate-level test helpers: a TUI driven by real terminal events and
//! rendered into an in-memory buffer, and network event fixtures.

mod events;
mod tui;

pub(crate) use events::*;
pub(crate) use tui::*;
