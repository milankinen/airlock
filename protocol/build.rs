fn main() {
    capnpc::CompilerCommand::new()
        .file("schema/supervisor.capnp")
        .run()
        .expect("capnpc schema compilation failed");
}
