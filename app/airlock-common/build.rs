fn main() {
    capnpc::CompilerCommand::new()
        .file("schema/network.capnp")
        .file("schema/supervisor.capnp")
        .file("schema/cli.capnp")
        .run()
        .expect("capnpc schema compilation failed");
}
