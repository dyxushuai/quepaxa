//! Compiles the vendored etcd v3 KV protos (shared with `kv-tokio`) and the internal
//! Openraft peer/admin proto without needing a system `protoc`.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // When built from the repo checkout, etcd protos live at `../../proto`; inside the
    // Docker build context they are copied to `vendor-proto`.
    let etcd_proto_root = if Path::new("../../proto/etcd/api/etcdserverpb/rpc.proto").exists() {
        "../../proto"
    } else {
        "vendor-proto"
    };
    let etcd_rpc = format!("{etcd_proto_root}/etcd/api/etcdserverpb/rpc.proto");
    let etcd_kv = format!("{etcd_proto_root}/etcd/api/mvccpb/kv.proto");

    let etcd_fds = protox::compile([etcd_rpc.as_str()], [etcd_proto_root])?;
    tonic_prost_build::configure().compile_fds(etcd_fds)?;

    let raft_fds = protox::compile(["proto/raft.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(raft_fds)?;

    println!("cargo:rerun-if-changed={etcd_rpc}");
    println!("cargo:rerun-if-changed={etcd_kv}");
    println!("cargo:rerun-if-changed=proto/raft.proto");
    Ok(())
}
