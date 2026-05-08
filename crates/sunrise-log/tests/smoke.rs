//! End-to-end smoke test: install a ring sink, emit a record, parse it back.

use std::sync::Arc;
use sunrise_log::{install_global, sink::RingSink, LogConfigBuilder, ProtoVersions, Sink};

#[test]
fn round_trip_record_through_ring() {
    let ring = Arc::new(RingSink::with_default_capacity());
    let cfg = LogConfigBuilder::new("sunrise-log-tests")
        .app("0.1.0+test")
        .dev("dev_abcdef01")
        .proto(ProtoVersions {
            wire: 1,
            doc: 1,
            crypto: 1,
        })
        .sink(ring.clone() as Arc<dyn Sink>)
        .build();
    install_global(cfg);

    let ctx = sunrise_log::Ctx::new()
        .with(
            sunrise_log::CtxKey::StreamH,
            sunrise_log::CtxValue::Str("abc12345"),
        )
        .with(sunrise_log::CtxKey::Epoch, sunrise_log::CtxValue::U64(7));
    sunrise_log::event!(
        level = sunrise_log::Level::Info,
        ev = "smoke.test.emit",
        msg = "smoke test",
        ctx = ctx,
    );

    let snap = ring.snapshot();
    assert_eq!(snap.len(), 1);
    let line = std::str::from_utf8(&snap[0]).unwrap();
    let line = line.strip_suffix('\n').unwrap();
    let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(parsed["lv"], "info");
    assert_eq!(parsed["ev"], "smoke.test.emit");
    assert_eq!(parsed["pkg"], "sunrise-log-tests");
    assert_eq!(parsed["msg"], "smoke test");
    assert_eq!(parsed["ctx"]["stream_h"], "abc12345");
    assert_eq!(parsed["ctx"]["epoch"], 7);
    assert_eq!(parsed["proto"]["wire"], 1);
    assert!(parsed["ts"].as_str().unwrap().ends_with('Z'));
    assert_eq!(parsed["ts"].as_str().unwrap().len(), 24);
}

#[test]
fn dropped_when_no_init() {
    // No `init`, no `install_global` — events go nowhere.
    sunrise_log::event!(
        level = sunrise_log::Level::Info,
        ev = "smoke.test.no_init",
        msg = "should drop silently",
    );
    // No assertion needed; it just shouldn't panic.
}
