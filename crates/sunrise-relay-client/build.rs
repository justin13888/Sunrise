//! Generate the relay client from the committed API description.
//!
//! `build.rs` rather than the `generate_api!` macro so the output lands in
//! `OUT_DIR` as ordinary source a developer can read with `cargo expand`-free
//! tooling, and so generation failures name a file rather than a macro
//! expansion.
//!
//! The input is `schemas/generated/openapi.v1.json`, which `sunrise-server`'s own
//! `the_committed_description_is_current` keeps in step with the handlers. That
//! test is what makes this safe: a stale description would generate a client
//! that disagrees with the server and compiles perfectly while doing so.

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let spec = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/generated/openapi.v1.json"
    );

    println!("cargo:rerun-if-changed={spec}");
    println!("cargo:rerun-if-changed=build.rs");

    // Every operation is generated, the two raw-binary blob ones included:
    // kynos describes a raw binary body as the empty Schema Object under
    // `application/octet-stream`, and spargen lowers that to `bytes::Bytes`.
    let spec = spargen::Spec::new(spec);

    let report = spargen::generate(&spec.build(format!("{out_dir}/api.rs")));
    // `Generated` on a cold build, `Cached` on a warm one; both are success.
    report.expect_success();
}
