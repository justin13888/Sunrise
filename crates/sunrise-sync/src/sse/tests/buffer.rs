//! The buffer half: `take_event` needs no socket.

use super::*;

/// A transport whose receive buffer has been filled directly.
///
/// [`SseTransport::take_event`] reads and drains `self.buf` and touches
/// nothing else, so the buffer is its whole input. This module is a child
/// of `sse`, which is what lets a test fill a field production code keeps
/// private — and why these cases live here rather than under `tests/`.
fn buffered(bytes: &[u8]) -> SseTransport {
    let mut t = SseTransport::connect("http://127.0.0.1:1");
    t.buf = bytes.to_vec();
    t
}

/// A terminated event yields its `id` and its `data`, and the bytes it was
/// made of — separator included — leave the buffer.
///
/// The separator is the load-bearing part: an event reported but not fully
/// drained leaves its own terminator at the head of the buffer, and the
/// next read finds an empty event nobody sent.
#[test]
fn a_complete_event_yields_its_id_and_data_and_drains_the_buffer() {
    let mut t = buffered(b"id: 42\ndata: {\"kind\":\"gap\"}\n\n");
    let (id, data) = t.take_event().expect("a terminated event");
    assert_eq!(id.as_deref(), Some("42"));
    assert_eq!(data, "{\"kind\":\"gap\"}");
    assert!(
        t.buf.is_empty(),
        "the event and its blank line are both consumed, not just the event"
    );
}

/// An event that has not arrived whole is not an event, and none of it is
/// consumed: the rest of it is still coming.
///
/// A single newline is not the separator. SSE ends an event with a blank
/// line, which is two bytes, and treating one of them as the boundary
/// would cut every multi-line event in half.
#[test]
fn a_partial_event_is_left_in_the_buffer_untouched() {
    let head = b"id: 42\ndata: {\"kind\"";
    let mut t = buffered(head);
    assert!(t.take_event().is_none(), "the event has not terminated");
    assert_eq!(
        t.buf, head,
        "nothing is consumed until the blank line lands"
    );

    let mut t = buffered(b"data: {}\n");
    assert!(t.take_event().is_none(), "one newline is not a blank line");
}

/// Two events that arrived in one read come out one at a time, in order.
#[test]
fn two_buffered_events_come_out_one_at_a_time_in_order() {
    let mut t = buffered(b"id: 1\ndata: one\n\nid: 2\ndata: two\n\n");
    let (id, data) = t.take_event().expect("the first event");
    assert_eq!((id.as_deref(), data.as_str()), (Some("1"), "one"));
    let (id, data) = t.take_event().expect("the second event");
    assert_eq!((id.as_deref(), data.as_str()), (Some("2"), "two"));
    assert!(t.take_event().is_none(), "and then the buffer is empty");
}

/// A keep-alive comment is consumed and reported as an event with no data,
/// not as "nothing arrived".
///
/// The distinction keeps [`SseTransport::recv_frame`]'s drain loop moving:
/// `None` means "the buffer holds no terminated event" and sends it back to
/// the socket, so a comment reported as `None` would leave the event behind
/// it unread until the next chunk happened to arrive.
#[test]
fn a_keep_alive_comment_is_consumed_rather_than_reported_as_nothing() {
    let mut t = buffered(b": keep-alive\n\nid: 7\ndata: after\n\n");
    let (id, data) = t.take_event().expect("the comment is a terminated event");
    assert!(id.is_none(), "a comment carries no id");
    assert!(data.is_empty(), "and nothing for the driver");
    let (id, data) = t.take_event().expect("the event behind it");
    assert_eq!(id.as_deref(), Some("7"));
    assert_eq!(data, "after");
}

// The cases below are ones the relay's own encoder never writes, so the
// loopback relay cannot script them: the parser reads the format, not the
// encoder, and these pin the difference byte by byte.

/// Successive `data:` fields are one value with a newline between each,
/// which is the only way the format carries a newline at all.
#[test]
fn successive_data_fields_are_joined_with_a_newline() {
    let mut t = buffered(b"data: line one\ndata: line two\n\n");
    let (_, data) = t.take_event().expect("a terminated event");
    assert_eq!(data, "line one\nline two", "not concatenated end to end");

    let mut t = buffered(b"data: {\"kind\":\"gap\",\ndata: \"reason\":\"x\"}\n\n");
    let (_, data) = t.take_event().expect("a terminated event");
    assert_eq!(data, "{\"kind\":\"gap\",\n\"reason\":\"x\"}");
}

/// A stream delimited with `\r\n` ends its events at `\r\n\r\n`, and each
/// line loses its `\r` rather than carrying it into the value.
#[test]
fn a_crlf_stream_terminates_its_events_and_drains_the_buffer() {
    let mut t = buffered(b"id: 9\r\ndata: one\r\n\r\nid: 10\r\ndata: two\r\n\r\n");
    let (id, data) = t.take_event().expect("a CRLF-terminated event");
    assert_eq!((id.as_deref(), data.as_str()), (Some("9"), "one"));
    let (id, data) = t.take_event().expect("the second CRLF event");
    assert_eq!((id.as_deref(), data.as_str()), (Some("10"), "two"));
    assert!(t.buf.is_empty());

    let mut t = buffered(b"data: {}\r\n");
    assert!(t.take_event().is_none(), "one CRLF is not a blank line");
}

/// A lone `\r` is a line ending too, so `\r\r` ends an event.
#[test]
fn a_bare_cr_stream_terminates_its_events() {
    let mut t = buffered(b"id: 3\rdata: a\rdata: b\r\r");
    let (id, data) = t.take_event().expect("a CR-terminated event");
    assert_eq!((id.as_deref(), data.as_str()), (Some("3"), "a\nb"));
    assert!(t.buf.is_empty());
}

/// A `\r\n` split across two reads neither stalls the event it ends nor
/// invents one: the `\r` that completes a blank line dispatches at once,
/// and the `\n` that arrives after it is a no-op, not an empty event ahead
/// of the real next one.
#[test]
fn a_crlf_split_across_reads_yields_each_event_once() {
    let mut t = buffered(b"data: first\r\n\r");
    let (_, data) = t.take_event().expect("the blank line's \\r ends it");
    assert_eq!(data, "first");
    assert!(t.take_event().is_none(), "nothing else has arrived");

    t.buf.extend_from_slice(b"\ndata: second\r\n\r\n");
    let (_, data) = t.take_event().expect("the next real event");
    assert_eq!(data, "second", "the stray \\n is not an event of its own");
    assert!(t.buf.is_empty());

    let head = b"data: x\r";
    let mut t = buffered(head);
    assert!(
        t.take_event().is_none(),
        "a trailing \\r cannot yet say whether the next line is blank"
    );
    assert_eq!(t.buf, head, "and nothing is consumed while it waits");
}

/// One space after the colon is the separator; anything further is value.
/// A field with no colon is a field with an empty value, and an `id`
/// holding a NUL is ignored.
#[test]
fn field_values_follow_the_format_s_whitespace_and_colon_rules() {
    let mut t = buffered(b"data:x\n\ndata:  indented\n\n");
    assert_eq!(t.take_event().expect("first").1, "x");
    assert_eq!(t.take_event().expect("second").1, " indented");

    let mut t = buffered(b"data\ndata: x\n\n");
    assert_eq!(t.take_event().expect("an event").1, "\nx");

    let mut t = buffered(b"id: a\0b\ndata: x\n\n");
    let (id, data) = t.take_event().expect("an event");
    assert!(id.is_none(), "an id with a NUL is not an id");
    assert_eq!(data, "x");
}

/// The cases above pin a few hand-picked cut points. This one covers every
/// cut point: a stream of events whose lines end in any mix of `\r`, `\n`
/// and `\r\n`, cut anywhere, yields the events and leaves the remainder
/// that the same stream read whole does — and reading it whole yields the
/// events it was built from.
mod framing_split {
    use super::{buffered, SseTransport};
    use proptest::prelude::*;

    type Event = (Option<String>, String);

    /// One line of a generated event, before its line ending is chosen.
    #[derive(Debug, Clone)]
    enum Line {
        Id(String),
        Data(String),
        Comment(String),
    }

    fn line_ending() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("\r"), Just("\n"), Just("\r\n")]
    }

    /// A value holding no line ending: colons and leading spaces included,
    /// since those are what the field rules read.
    fn value() -> impl Strategy<Value = String> {
        "[a-z0-9 :{}\"]{0,8}"
    }

    fn line() -> impl Strategy<Value = Line> {
        prop_oneof![
            value().prop_map(Line::Id),
            value().prop_map(Line::Data),
            value().prop_map(Line::Comment),
        ]
    }

    /// The lines of one event, each with its ending, then the ending of
    /// the blank line that closes it.
    fn event() -> impl Strategy<Value = (Vec<(Line, &'static str)>, &'static str)> {
        (
            prop::collection::vec((line(), line_ending()), 1..4),
            line_ending(),
        )
    }

    /// The bytes of `events` followed by `tail`, and what each event
    /// carries.
    ///
    /// A lone `\r` followed by a lone `\n` is one `\r\n`, not a line
    /// ending then a blank line, so a closing `\n` after a `\r` would merge
    /// the event into the next. That is the format, not the parser, so the
    /// generator closes such an event with `\r` instead.
    fn render(
        events: &[(Vec<(Line, &'static str)>, &'static str)],
        tail: &str,
    ) -> (Vec<u8>, Vec<Event>) {
        let mut bytes = String::new();
        let mut expected = Vec::new();
        for (lines, blank) in events {
            let mut id = None;
            let mut data: Vec<&str> = Vec::new();
            let mut last = "";
            for (line, ending) in lines {
                let (field, v) = match line {
                    Line::Id(v) => {
                        id = Some(v.clone());
                        ("id: ", v)
                    }
                    Line::Data(v) => {
                        data.push(v);
                        ("data: ", v)
                    }
                    Line::Comment(v) => (":", v),
                };
                bytes.push_str(field);
                bytes.push_str(v);
                bytes.push_str(ending);
                last = ending;
            }
            let blank = if last == "\r" && *blank == "\n" {
                "\r"
            } else {
                blank
            };
            bytes.push_str(blank);
            expected.push((id, data.join("\n")));
        }
        bytes.push_str(tail);
        (bytes.into_bytes(), expected)
    }

    fn drain(t: &mut SseTransport, out: &mut Vec<Event>) {
        while let Some(event) = t.take_event() {
            out.push(event);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            rng_seed: sunrise_test_seed::proptest_rng_seed(),
            ..ProptestConfig::default()
        })]

        #[test]
        fn a_stream_cut_anywhere_reads_as_the_stream_read_whole(
            events in prop::collection::vec(event(), 0..5),
            tail in prop_oneof![
                Just(String::new()),
                value().prop_map(|v| format!("data: {v}")),
                (value(), line_ending()).prop_map(|(v, e)| format!("data: {v}{e}")),
            ],
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
        ) {
            let (stream, expected) = render(&events, &tail);

            let mut whole = buffered(&stream);
            let mut read_whole = Vec::new();
            drain(&mut whole, &mut read_whole);
            prop_assert_eq!(&read_whole, &expected);
            prop_assert_eq!(
                whole.buf.as_slice(),
                tail.as_bytes(),
                "the unterminated tail is the remainder"
            );

            let mut at: Vec<usize> = cuts.iter().map(|i| i.index(stream.len() + 1)).collect();
            at.push(stream.len());
            at.sort_unstable();
            let mut cut = buffered(b"");
            let mut read_cut = Vec::new();
            let mut from = 0;
            for to in at {
                cut.buf.extend_from_slice(&stream[from..to]);
                drain(&mut cut, &mut read_cut);
                from = to;
            }
            prop_assert_eq!(&read_cut, &read_whole);
            prop_assert_eq!(cut.buf, whole.buf);
        }
    }
}
