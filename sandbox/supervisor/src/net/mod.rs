//! Guest-side networking: DNS resolution, transparent TCP proxying, and Unix
//! socket forwarding.

pub mod dns;
pub mod proxy;
pub mod socket;

pub use proxy::start_proxy;
