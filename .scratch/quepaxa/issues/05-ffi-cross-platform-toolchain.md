# What toolchain delivers the C ABI across our platform tiers

Parent: [map](../map.md)
Type: research
Status: open
Blocked by: 

## Question

Compare cbindgen vs safer-ffi vs diplomat (vs uniffi for higher-level bindings) for a stable C ABI; panic/unwind policy across FFI; opaque handle and buffer ownership patterns; thread-safety contracts; producing artifacts for Linux/macOS/Windows/Android (cargo-ndk)/iOS (xcframework)/wasm32; GitHub Actions runner availability for Tier 1 (incl. aarch64 Linux/Windows) and cross-compilation for Tier 2; ABI stability checking tools. Recommend a toolchain.
