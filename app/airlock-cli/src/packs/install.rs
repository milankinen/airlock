//! Installing packs: the install boot ([`setup`]), the install state on
//! the sandbox disk ([`state`]) and what to install ([`plan`]).

pub mod compose;
pub mod facts;
pub mod phase;
pub mod plan;
pub mod progress;
pub mod setup;
pub mod state;
