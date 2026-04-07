#[allow(clippy::all, clippy::pedantic)]
pub mod supervisor_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/supervisor_capnp.rs"));
}

pub const SUPERVISOR_PORT: u32 = 1024;
pub const CLI_SOCK_FILENAME: &str = "cli.sock";
