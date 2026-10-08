//! Build script for the shared RPC crate.
//!
//! Generates the Rust code of the RPC protocols from their schemas.

fn main() {
    capnpc::CompilerCommand::new()
        .file("schema/network.capnp")
        .file("schema/supervisor.capnp")
        .file("schema/cli.capnp")
        .run()
        .expect("capnpc schema compilation failed");
}
