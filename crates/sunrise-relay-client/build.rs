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

    // The two raw-binary blob operations are omitted, and the reason is a
    // disagreement between two first-party crates rather than a defect in
    // either.
    //
    // kynos describes a raw binary body as the **empty Schema Object**, and
    // says why: "raw binary as a whole message body ... is the shape 3.1
    // describes by *omitting* things", with `contentMediaType` left out because
    // it would only repeat the key the content sits under. That reading of
    // OpenAPI 3.1 is defensible. spargen refuses it (`E009`), wanting "a
    // string-like or binary schema that can be sent as a raw body".
    //
    // Nothing is lost today: the bootstrap this crate exists for is accounts
    // and devices, and no client has ever uploaded a chunk. Omitting is
    // recorded here, warned by spargen as `W009`, and narrower than making the
    // server describe its binary bodies in a shape its own framework argues
    // against. The upstream fix is for the two crates to agree.
    let spec = spargen::Spec::new(spec)
        .omit_rule(spargen::OmitRule::operation(
            spargen::OmitMethod::Put,
            "/api/v1/blobs/{upload_id}/{chunk_idx}",
        ))
        .omit_rule(spargen::OmitRule::operation(
            spargen::OmitMethod::Get,
            "/api/v1/blobs/{blob_id}",
        ));

    let report = spargen::generate(&spec.build(format!("{out_dir}/api.rs")));
    // `Generated` on a cold build, `Cached` on a warm one; both are success.
    report.expect_success();
}
