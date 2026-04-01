#[allow(clippy::all)]
pub mod supervisor_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/supervisor_capnp.rs"));
}

pub mod streams;

pub const SUPERVISOR_PORT: u32 = 1024;
