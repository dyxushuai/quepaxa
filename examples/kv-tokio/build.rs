//! Compiles the vendored etcd protos (proto/) with protox, so building needs no `protoc`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = ["proto/etcd/api/etcdserverpb/rpc.proto"];
    let fds = protox::compile(files, ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    for f in ["proto/etcd/api/etcdserverpb/rpc.proto", "proto/etcd/api/mvccpb/kv.proto"] {
        println!("cargo:rerun-if-changed={f}");
    }
    Ok(())
}
