//! Pack installation into the sandbox.
//!
//! `airlock start` installs the configured packs that have a setup script.
//! It boots the sandbox once with a restricted config, runs the script of
//! each pack, and records the result next to the sandbox disk. A later
//! start does not install a pack again when it is already installed.
//!
//! Install scripts get their pack args as environment variables. They
//! report their progress to the host with a status line protocol.

pub mod compose;
pub mod facts;
pub mod phase;
pub mod plan;
pub mod progress;
pub mod setup;
pub mod state;
