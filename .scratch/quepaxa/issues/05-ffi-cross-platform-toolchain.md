# What toolchain delivers the C ABI across our platform tiers

Parent: [map](../map.md)
Type: research
Status: resolved
Blocked by: 

## Question

Compare cbindgen vs safer-ffi vs diplomat (vs uniffi for higher-level bindings) for a stable C ABI; panic/unwind policy across FFI; opaque handle and buffer ownership patterns; thread-safety contracts; producing artifacts for Linux/macOS/Windows/Android (cargo-ndk)/iOS (xcframework)/wasm32; GitHub Actions runner availability for Tier 1 (incl. aarch64 Linux/Windows) and cross-compilation for Tier 2; ABI stability checking tools. Recommend a toolchain.

## Answer

Full survey: `docs/research/ffi-cross-platform-toolchain.md` on branch `research/ffi-cross-platform-toolchain` (commit df226e1).

- Hand-written `extern "C"` layer; cbindgen 0.29.4 header committed as the ABI contract; cargo-c for packaging/SONAME. safer-ffi (alpha), Diplomat (non-idiomatic C), UniFFI (internal glue) rejected for the C ABI; Diplomat/UniFFI may later sit on the Rust API as separate crates.
- Panics: `extern "C"` only, `catch_unwind` per export → status code + poisoned handle; Rust ≥1.81 abort as backstop. Sans-IO = no callbacks, so no foreign unwinding into Rust.
- Ownership: opaque handles with `new`/`free`, borrowed `(ptr,len)` inputs, per-type `*_free`, caller-buffer two-call pattern, `struct_size`-prefixed config. Handles `Send` not `Sync`.
- Tier 1 runners native: `ubuntu-24.04(-arm)`, `windows-2025`, `windows-11-arm`, `macos-15`, `macos-15-intel` (Intel image only until 2027-08). No `cross`.
- Tier 2: Android via cargo-ndk 4.1.2 + NDK r28+ (16 KB pages), iOS xcframework + simulator smoke, wasm/`thumbv7em-none-eabihf` build core only. C ABI is the wrong wasm delivery; use wasm-bindgen / wasip2+wit-bindgen when a wasm user exists.
- ABI gates: header diff, exported-symbol diff, C/C++ compile test with layout `_Static_assert`; abidiff advisory; cargo-semver-checks for Rust API only.
- MSRV proposal: 1.89 (spec-compliant wasm C ABI; includes 1.81 abort semantics, edition 2024). Open: actions over ABI as accessor views vs versioned byte encoding; glibc floor; official Kotlin/Swift packages.
