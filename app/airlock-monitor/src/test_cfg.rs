//! Test helpers of the crate: a TUI that gets real terminal events and draws
//! into a buffer in memory, and network events for tests.

mod events;
mod tui;

pub(crate) use events::*;
pub(crate) use tui::*;
