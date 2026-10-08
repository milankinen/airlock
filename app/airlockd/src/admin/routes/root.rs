//! Liveness check of the admin service.
//!
//! Answers with plain text, so tools can check that `admin.airlock` is
//! available.

/// Response body (ASCII art logo). The first newline is not sent.
const MESSAGE: &str = r"
       _      _            _
  __ _(_)_ __| | ___   ___| | __
 / _` | | '__| |/ _ \ / __| |/ /
| (_| | | |  | | (_) | (__|   <
 \__,_|_|_|  |_|\___/ \___|_|\_\

";

/// Return the [`MESSAGE`] text.
pub async fn handle() -> &'static str {
    &MESSAGE[1..]
}
