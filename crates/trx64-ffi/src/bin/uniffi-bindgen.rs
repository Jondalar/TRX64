//! Bundled uniffi-bindgen CLI (no global install needed).
//!
//! `cargo run --bin uniffi-bindgen -- generate --library <dylib> --language swift ...`
//! drives the SAME uniffi version this crate links, so the generated Swift bindings
//! always match the compiled scaffolding. Used by `scripts/build-xcframework.sh`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
