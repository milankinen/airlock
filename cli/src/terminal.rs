//! Terminal management: raw mode, stdin, signal forwarding, and resize events.

mod signals;
#[allow(clippy::module_inception)]
mod terminal;

pub use signals::*;
pub use terminal::*;
