//! `sunrise://` links, parsed and built.
//!
//! `docs/07-clients/interaction-patterns.md` §URL scheme defines the shapes and
//! says the last word on them: *"All deep links are validated; unknown shapes
//! are ignored."* So [`DeepLink::parse`] is a parser over a closed set — a
//! link a client cannot act on is `None` and dropped, instead of arriving
//! half-understood at a router that then has to guess. A notification's link
//! is written by [`DeepLink::to_url`] and parsed back by the same code on
//! whichever client the user taps it on.
//!
//! Two shapes are the views issue #9 asks a notification to open into
//! (`morning`, `evening`). `sunrise://share/<token>` is the link an owner
//! hands the person a Stream is shared with: [`ShareLink`] is its token, the
//! relay viewer grant's claim token beside the sealed grant (ADR-0074 §1).
//! It is parsed and built here and routed by no client yet: no client can
//! claim or open what it carries.
//!
//! Ported from the Apple app's `DeepLink.swift`. One difference: an id is
//! checked by [`EntityRef::parse_any`] rather than by its shape, so a link
//! naming `tsk_` and 26 characters that are not a ULID is refused here rather
//! than by the core's `BadId` after it has been routed.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sunrise_crypto::{decode_share_grant, ShareGrant, ShareGrantError, MAX_SHARE_GRANT_BYTES};
use sunrise_domain::SnoozeSpan;
use sunrise_id::{EntityKind, EntityRef};

/// The scheme, lowercase.
pub const SCHEME: &str = "sunrise";

/// A `sunrise://` link a client can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// `sunrise://morning` — the morning summary.
    MorningSummary,
    /// `sunrise://evening` — the end-of-day plan.
    EndOfDayPlan,
    /// `sunrise://entity/<EntityRef>` — show where that entity lives.
    Entity(EntityRef),
    /// `sunrise://capture?text=…&stream=…` — quick capture, pre-filled. The
    /// stream is already folded into the line as `#name`.
    Capture(String),
    /// `sunrise://task/<TaskId>?action=…` — act on one task.
    Task(EntityRef, TaskLinkAction),
    /// `sunrise://focus/<TaskId>` — focus on that task.
    Focus(EntityRef),
    /// `sunrise://share/<token>` — a Stream shared with whoever holds the
    /// link's recipient identity.
    Share(Box<ShareLink>),
}

/// The token's format version, its first byte.
const SHARE_TOKEN_V1: u8 = 1;

/// The bytes ahead of the grant: the version, the claim token and the
/// granter's `ID_S_pub`.
const SHARE_TOKEN_HEAD: usize = 1 + 32 + 32;

/// The longest token text a link may carry: the base64url of the head and the
/// largest grant `sunrise-crypto` decodes. Checked before decoding, so a link
/// cannot make a client allocate for a grant it would refuse anyway.
const MAX_SHARE_TOKEN_CHARS: usize = (SHARE_TOKEN_HEAD + MAX_SHARE_GRANT_BYTES).div_ceil(3) * 4;

/// What a `sunrise://share/<token>` link hands the person a Stream is shared
/// with (`docs/03-crypto/sharing-with-others.md` §Granting access, step 4).
///
/// The token is base64url, unpadded, of
/// `0x01 || claim_token (32) || granter_id_s_pub (32) || grant`:
///
/// * `claim_token` claims the relay viewer grant the owner registered
///   (ADR-0074 §1). It is a credential, so [`Debug`] never prints it.
/// * `granter_id_s_pub` is the key the grant's `identity_sig` verifies under,
///   and what the recipient compares with the owner's fingerprint out of band
///   (step 5). The grant cannot name it: the relay offers no lookup from an
///   identity to its keys.
/// * `grant` is the canonical `ShareGrantPayload` `Engine::share_stream`
///   returns, decoded here and refused unless it is a viewer grant within its
///   bounds. Nothing is verified: the signature covers the recipient's
///   `ID_D_pub`, which only the recipient holds.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareLink {
    claim_token: [u8; 32],
    granter_id_s_pub: [u8; 32],
    grant: ShareGrant,
    grant_bytes: Vec<u8>,
}

impl std::fmt::Debug for ShareLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareLink")
            .field("claim_token", &"<redacted>")
            .field("granter_id_s_pub", &crate::ids::hex(&self.granter_id_s_pub))
            .field("grant", &self.grant.terms())
            .finish_non_exhaustive()
    }
}

impl ShareLink {
    /// The link for `grant`, the encoding `Engine::share_stream` returned,
    /// signed by `granter_id_s_pub`'s identity, beside the relay grant's
    /// `claim_token`.
    ///
    /// # Errors
    /// What [`decode_share_grant`] refuses: a grant that is malformed, past
    /// its bound, or not a viewer grant.
    pub fn new(
        claim_token: [u8; 32],
        granter_id_s_pub: [u8; 32],
        grant: Vec<u8>,
    ) -> Result<Self, ShareGrantError> {
        Ok(Self {
            claim_token,
            granter_id_s_pub,
            grant: decode_share_grant(&grant)?,
            grant_bytes: grant,
        })
    }

    /// The relay viewer grant's claim token.
    #[must_use]
    pub const fn claim_token(&self) -> &[u8; 32] {
        &self.claim_token
    }

    /// The `ID_S_pub` the grant is signed under.
    #[must_use]
    pub const fn granter_id_s_pub(&self) -> &[u8; 32] {
        &self.granter_id_s_pub
    }

    /// The grant, decoded and not verified.
    #[must_use]
    pub const fn grant(&self) -> &ShareGrant {
        &self.grant
    }

    /// The grant's canonical encoding, as the link carries it.
    #[must_use]
    pub fn grant_bytes(&self) -> &[u8] {
        &self.grant_bytes
    }

    /// The token, as the link's one path segment.
    #[must_use]
    pub fn to_token(&self) -> String {
        let mut bytes = Vec::with_capacity(SHARE_TOKEN_HEAD + self.grant_bytes.len());
        bytes.push(SHARE_TOKEN_V1);
        bytes.extend_from_slice(&self.claim_token);
        bytes.extend_from_slice(&self.granter_id_s_pub);
        bytes.extend_from_slice(&self.grant_bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Read a token, or refuse it: text that is not canonical unpadded
    /// base64url, an unknown version, a short head, or a grant
    /// [`Self::new`] refuses.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        if token.len() > MAX_SHARE_TOKEN_CHARS {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
        let (&version, rest) = bytes.split_first()?;
        if version != SHARE_TOKEN_V1 || rest.len() < SHARE_TOKEN_HEAD - 1 {
            return None;
        }
        let (claim_token, rest) = rest.split_at(32);
        let (granter_id_s_pub, grant) = rest.split_at(32);
        Self::new(
            claim_token.try_into().ok()?,
            granter_id_s_pub.try_into().ok()?,
            grant.to_vec(),
        )
        .ok()
    }
}

/// What a `sunrise://task/<id>` link asks for. The identifiers are the ones
/// `interaction-patterns.md` §Action wiring names, so a link built by any
/// client is understood by every other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLinkAction {
    /// Show the task: no `action`, or `action=open`. What tapping a
    /// notification's body does.
    Open,
    /// `action=complete`.
    Complete,
    /// `action=snooze_1h`, `snooze_tomorrow` or `snooze_next_week`.
    Snooze(SnoozeSpan),
}

impl TaskLinkAction {
    /// The `action=` value.
    #[must_use]
    pub const fn identifier(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Snooze(SnoozeSpan::OneHour) => "snooze_1h",
            Self::Snooze(SnoozeSpan::Tomorrow) => "snooze_tomorrow",
            Self::Snooze(SnoozeSpan::NextWeek) => "snooze_next_week",
        }
    }

    fn from_identifier(raw: &str) -> Option<Self> {
        Some(match raw.to_ascii_lowercase().as_str() {
            "open" => Self::Open,
            "complete" => Self::Complete,
            "snooze_1h" => Self::Snooze(SnoozeSpan::OneHour),
            "snooze_tomorrow" => Self::Snooze(SnoozeSpan::Tomorrow),
            "snooze_next_week" => Self::Snooze(SnoozeSpan::NextWeek),
            _ => return None,
        })
    }
}

/// The screen a link lands on, when it lands on one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkScreen {
    /// The morning summary.
    Morning,
    /// The end-of-day plan.
    Evening,
    /// Today, every section.
    Today,
    /// The calendar grid.
    Calendar,
    /// The Focus screen.
    Focus,
}

impl DeepLink {
    /// Parse a link, or refuse it.
    #[must_use]
    pub fn parse(input: &str) -> Option<Self> {
        let parsed = url::Url::parse(input.trim()).ok()?;
        if !parsed.scheme().eq_ignore_ascii_case(SCHEME) {
            return None;
        }
        // `sunrise://morning` puts "morning" in the host, not the path.
        let route = parsed.host_str()?.to_ascii_lowercase();
        let segments: Vec<String> = parsed
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode)
            .collect::<Option<_>>()?;
        let query = query_pairs(parsed.query().unwrap_or(""))?;
        Self::route(&route, &segments, &query)
    }

    /// The route table. A `None` here is the "unknown shapes are ignored"
    /// rule, in one place.
    fn route(route: &str, segments: &[String], query: &[(String, Option<String>)]) -> Option<Self> {
        match (route, segments) {
            ("morning", []) => Some(Self::MorningSummary),
            ("evening", []) => Some(Self::EndOfDayPlan),
            ("capture", []) => {
                let text = capture_line(query);
                (!text.is_empty()).then_some(Self::Capture(text))
            }
            ("entity", [id]) => EntityRef::parse_any(id).ok().map(Self::Entity),
            ("task", [id]) => {
                let id = task_ref(id)?;
                // No `action`, or one with no value, is the body tap.
                let action = match value(query, "action") {
                    Some(raw) => TaskLinkAction::from_identifier(raw)?,
                    None => TaskLinkAction::Open,
                };
                Some(Self::Task(id, action))
            }
            // A task, and only a task: a session is time spent on one piece
            // of work, and the core's `StartFocus` takes a `tsk_` id.
            ("focus", [id]) => task_ref(id).map(Self::Focus),
            ("share", [token]) => ShareLink::from_token(token).map(|l| Self::Share(Box::new(l))),
            _ => None,
        }
    }

    /// The link itself, for a notification's payload.
    #[must_use]
    pub fn to_url(&self) -> String {
        match self {
            Self::MorningSummary => format!("{SCHEME}://morning"),
            Self::EndOfDayPlan => format!("{SCHEME}://evening"),
            Self::Entity(id) => format!("{SCHEME}://entity/{id}"),
            Self::Capture(text) => format!("{SCHEME}://capture?text={}", percent_encode(text)),
            Self::Task(id, action) => {
                format!("{SCHEME}://task/{id}?action={}", action.identifier())
            }
            Self::Focus(id) => format!("{SCHEME}://focus/{id}"),
            Self::Share(link) => format!("{SCHEME}://share/{}", link.to_token()),
        }
    }

    /// Where the link lands, when it lands on a screen at all.
    ///
    /// `None` for the links that are an operation: completing or snoozing a
    /// task from a notification button must not drag a window forward
    /// ("translates to a CRDT op without opening UI when possible"). `None`
    /// too for a share: what it opens is the recipient's decision to accept,
    /// over whatever screen is showing.
    #[must_use]
    pub fn screen(&self) -> Option<LinkScreen> {
        match self {
            Self::MorningSummary => Some(LinkScreen::Morning),
            Self::EndOfDayPlan => Some(LinkScreen::Evening),
            Self::Capture(_)
            | Self::Share(_)
            | Self::Task(_, TaskLinkAction::Complete | TaskLinkAction::Snooze(_)) => None,
            Self::Entity(id) => Some(home_of(id)),
            Self::Task(_, TaskLinkAction::Open) => Some(LinkScreen::Today),
            Self::Focus(_) => Some(LinkScreen::Focus),
        }
    }

    /// The entity the link asks to be **shown** on that screen.
    ///
    /// A second question beside [`Self::screen`]: `sunrise://entity/<blk_…>`
    /// is the link a block reminder carries, and moving only the screen opened
    /// the calendar on today — an alert about Thursday's block landed on a
    /// grid the block was not on (#40). `None` for a focus link: the session
    /// it starts is what the Focus screen shows.
    #[must_use]
    pub fn reveal(&self) -> Option<EntityRef> {
        match self {
            Self::Entity(id) | Self::Task(id, TaskLinkAction::Open) => Some(*id),
            _ => None,
        }
    }
}

/// The screen an entity lives on: a block on the calendar grid, anything else
/// on Today.
fn home_of(id: &EntityRef) -> LinkScreen {
    if id.kind() == EntityKind::Block {
        LinkScreen::Calendar
    } else {
        LinkScreen::Today
    }
}

fn task_ref(raw: &str) -> Option<EntityRef> {
    EntityRef::parse_any(raw)
        .ok()
        .filter(|id| id.kind() == EntityKind::Task)
}

/// `text` and `stream`, recombined into one capture line.
///
/// `#stream` is the shared capture parser's own annotation, so folding the
/// parameter into the text hands its resolution to the code a typed line goes
/// through. A name with a space cannot be written as `#name`, and inventing a
/// quoting rule the parser lacks would read back wrong: the name is dropped,
/// the text is not.
fn capture_line(query: &[(String, Option<String>)]) -> String {
    let text = value(query, "text").map_or("", str::trim);
    let stream = value(query, "stream").map_or("", str::trim);
    if stream.is_empty() || stream.contains(' ') {
        return text.to_owned();
    }
    if text.is_empty() {
        format!("#{stream}")
    } else {
        format!("{text} #{stream}")
    }
}

/// The value of the first item named `name`; `None` when there is no such
/// item or it has no `=`, which every route reads alike.
fn value<'a>(query: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, v)| v.as_deref())
}

/// `a=1&b&c=%20` as name/value pairs, percent-decoded. `+` is a literal plus,
/// not a space: this is a URL query, not a form body, and a capture of
/// "C++" must survive the round trip.
fn query_pairs(query: &str) -> Option<Vec<(String, Option<String>)>> {
    query
        .split('&')
        .filter(|item| !item.is_empty())
        .map(|item| match item.split_once('=') {
            Some((name, value)) => Some((percent_decode(name)?, Some(percent_decode(value)?))),
            None => Some((percent_decode(item)?, None)),
        })
        .collect()
}

/// Decode `%XX` escapes; `None` for a malformed escape or bytes that are not
/// UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Escape everything but RFC 3986's unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push_str(&crate::ids::hex(&[b]).to_ascii_uppercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const BLOCK: &str = "blk_01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn id(s: &str) -> EntityRef {
        EntityRef::parse_any(s).unwrap()
    }

    fn link(s: &str) -> Option<DeepLink> {
        DeepLink::parse(s)
    }

    /// A viewer grant from one fixed identity to another, as
    /// `Engine::share_stream` encodes it.
    fn grant_bytes() -> (Vec<u8>, [u8; 32]) {
        use rand_core::SeedableRng;
        use sunrise_crypto::{
            identity_id_from_pub, seal_share_grant, IdentityDhKeyPair, IdentitySigningKeyPair,
            ShareGrantTerms, ShareRole, StreamKey, StreamMetadata,
        };
        let granter = IdentitySigningKeyPair::from_secret_bytes(&[1; 32]);
        let recipient = IdentitySigningKeyPair::from_secret_bytes(&[2; 32]);
        let recipient_d = IdentityDhKeyPair::from_secret_bytes([3; 32]);
        let grant = seal_share_grant(
            &granter,
            ShareGrantTerms {
                stream_id: [0x5a; 16],
                epoch: 4,
                recipient_identity_id: identity_id_from_pub(&recipient.public_bytes()),
                role: ShareRole::Viewer,
                expires_at_ms: Some(1_900_000_000_000),
            },
            &recipient_d.public_bytes(),
            &StreamKey::from_bytes([0x33; 32]),
            &StreamMetadata {
                name: "Garden".into(),
                color: "green".into(),
            },
            &mut rand_chacha::ChaCha20Rng::seed_from_u64(7),
        )
        .unwrap();
        (grant.encode().unwrap(), granter.public_bytes())
    }

    fn share() -> ShareLink {
        let (grant, granter) = grant_bytes();
        ShareLink::new([0xc1; 32], granter, grant).unwrap()
    }

    #[test]
    fn a_share_link_carries_the_claim_token_the_granter_and_the_grant() {
        let original = share();
        let url = DeepLink::Share(Box::new(original.clone())).to_url();
        assert!(url.starts_with("sunrise://share/"), "{url}");
        let Some(DeepLink::Share(parsed)) = link(&url) else {
            panic!("{url} should parse as a share");
        };
        assert_eq!(parsed.claim_token(), &[0xc1; 32]);
        assert_eq!(parsed.granter_id_s_pub(), original.granter_id_s_pub());
        assert_eq!(parsed.grant_bytes(), grant_bytes().0.as_slice());
        assert_eq!(parsed.grant().terms().stream_id, [0x5a; 16]);
        assert_eq!(
            parsed.grant().terms().expires_at_ms,
            Some(1_900_000_000_000)
        );
        assert_eq!(*parsed, original);
    }

    /// The token's layout, byte for byte, so a client in another language
    /// that builds one is held to the same shape.
    #[test]
    fn the_share_token_is_the_version_the_claim_token_the_key_and_the_grant() {
        let link = share();
        let bytes = URL_SAFE_NO_PAD.decode(link.to_token()).unwrap();
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..33], &[0xc1; 32]);
        assert_eq!(&bytes[33..65], link.granter_id_s_pub());
        assert_eq!(&bytes[65..], link.grant_bytes());
    }

    #[test]
    fn a_share_opens_no_screen_and_reveals_nothing() {
        let l = DeepLink::Share(Box::new(share()));
        assert_eq!(l.screen(), None);
        assert_eq!(l.reveal(), None);
    }

    #[test]
    fn a_share_links_debug_never_prints_the_claim_token() {
        let printed = format!("{:?}", share());
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(
            !printed.contains(&crate::ids::hex(&[0xc1; 32])),
            "{printed}"
        );
    }

    #[test]
    fn a_share_token_that_is_not_one_is_ignored() {
        let token = share().to_token();
        let bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        let with = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut b = bytes.clone();
            f(&mut b);
            format!("sunrise://share/{}", URL_SAFE_NO_PAD.encode(b))
        };
        let refused = [
            // Another version.
            with(&|b| b[0] = 2),
            // A head cut short: no grant at all.
            with(&|b| b.truncate(SHARE_TOKEN_HEAD)),
            with(&|b| b.truncate(40)),
            // A grant that is not one.
            with(&|b| b.truncate(b.len() - 1)),
            with(&|b| b.push(0)),
            // A grant past `sunrise-crypto`'s bound, refused before decoding.
            format!("sunrise://share/{}", "A".repeat(MAX_SHARE_TOKEN_CHARS + 1)),
            // Padded, or outside base64url's alphabet.
            format!("sunrise://share/{token}="),
            format!("sunrise://share/+{token}"),
            // A second segment, or none.
            format!("sunrise://share/{token}/x"),
            "sunrise://share".to_owned(),
        ];
        for raw in refused {
            assert_eq!(link(&raw), None, "{raw} should be ignored");
        }
        assert_eq!(ShareLink::from_token(""), None);
    }

    #[test]
    fn a_share_link_refuses_a_grant_that_is_not_one() {
        assert!(ShareLink::new([0; 32], [0; 32], vec![0xa0]).is_err());
    }

    #[test]
    fn the_two_briefs_have_a_link_each_and_name_their_screen() {
        assert_eq!(link("sunrise://morning"), Some(DeepLink::MorningSummary));
        assert_eq!(link("sunrise://evening"), Some(DeepLink::EndOfDayPlan));
        assert_eq!(link("SUNRISE://Morning"), Some(DeepLink::MorningSummary));
        assert_eq!(DeepLink::MorningSummary.screen(), Some(LinkScreen::Morning));
        assert_eq!(DeepLink::EndOfDayPlan.screen(), Some(LinkScreen::Evening));
    }

    #[test]
    fn the_documented_task_actions_parse_and_no_action_opens() {
        let t = id(TASK);
        let cases = [
            ("complete", TaskLinkAction::Complete),
            ("snooze_1h", TaskLinkAction::Snooze(SnoozeSpan::OneHour)),
            (
                "snooze_tomorrow",
                TaskLinkAction::Snooze(SnoozeSpan::Tomorrow),
            ),
            (
                "SNOOZE_NEXT_WEEK",
                TaskLinkAction::Snooze(SnoozeSpan::NextWeek),
            ),
        ];
        for (raw, action) in cases {
            assert_eq!(
                link(&format!("sunrise://task/{TASK}?action={raw}")),
                Some(DeepLink::Task(t, action))
            );
        }
        assert_eq!(
            link(&format!("sunrise://task/{TASK}")),
            Some(DeepLink::Task(t, TaskLinkAction::Open))
        );
    }

    #[test]
    fn only_the_open_action_goes_to_a_screen() {
        let t = id(TASK);
        assert_eq!(
            DeepLink::Task(t, TaskLinkAction::Open).screen(),
            Some(LinkScreen::Today)
        );
        assert_eq!(DeepLink::Task(t, TaskLinkAction::Complete).screen(), None);
        assert_eq!(
            DeepLink::Task(t, TaskLinkAction::Snooze(SnoozeSpan::Tomorrow)).screen(),
            None
        );
        assert_eq!(DeepLink::Capture("x".into()).screen(), None);
    }

    #[test]
    fn an_entity_link_lands_where_that_kind_lives_and_reveals_it() {
        let block = link(&format!("sunrise://entity/{BLOCK}")).unwrap();
        assert_eq!(block.screen(), Some(LinkScreen::Calendar));
        assert_eq!(block.reveal(), Some(id(BLOCK)));
        let task = link(&format!("sunrise://entity/{TASK}")).unwrap();
        assert_eq!(task.screen(), Some(LinkScreen::Today));
        assert_eq!(task.reveal(), Some(id(TASK)));
        assert_eq!(
            link(&format!("sunrise://task/{TASK}")).unwrap().reveal(),
            Some(id(TASK))
        );
    }

    #[test]
    fn a_link_that_names_no_entity_reveals_nothing() {
        let t = id(TASK);
        for l in [
            DeepLink::MorningSummary,
            DeepLink::EndOfDayPlan,
            DeepLink::Capture("Buy milk".into()),
            DeepLink::Task(t, TaskLinkAction::Complete),
            DeepLink::Task(t, TaskLinkAction::Snooze(SnoozeSpan::OneHour)),
            DeepLink::Focus(t),
        ] {
            assert_eq!(l.reveal(), None, "{l:?}");
        }
    }

    #[test]
    fn a_focus_link_names_a_task_and_the_focus_screen() {
        assert_eq!(
            link(&format!("sunrise://focus/{TASK}")),
            Some(DeepLink::Focus(id(TASK)))
        );
        assert_eq!(DeepLink::Focus(id(TASK)).screen(), Some(LinkScreen::Focus));
    }

    #[test]
    fn capture_folds_the_stream_into_the_line() {
        assert_eq!(
            link("sunrise://capture?text=Buy%20milk&stream=Errands"),
            Some(DeepLink::Capture("Buy milk #Errands".into()))
        );
        assert_eq!(
            link("sunrise://capture?text=Buy%20milk"),
            Some(DeepLink::Capture("Buy milk".into()))
        );
        assert_eq!(
            link("sunrise://capture?stream=Errands"),
            Some(DeepLink::Capture("#Errands".into()))
        );
    }

    #[test]
    fn a_stream_name_with_a_space_is_dropped_rather_than_mangled() {
        assert_eq!(
            link("sunrise://capture?text=Call&stream=Big%20Client"),
            Some(DeepLink::Capture("Call".into()))
        );
    }

    /// A plus is a plus: the query is a URL's, not a form body's.
    #[test]
    fn a_plus_in_a_capture_survives() {
        assert_eq!(
            link("sunrise://capture?text=Learn%20C++"),
            Some(DeepLink::Capture("Learn C++".into()))
        );
    }

    #[test]
    fn unknown_shapes_are_ignored() {
        let refused = [
            "https://morning".to_owned(),
            "sunrise:morning".to_owned(),
            "sunrise://sunrise".to_owned(),
            "sunrise://morning/extra".to_owned(),
            "sunrise://task/not-an-id".to_owned(),
            format!("sunrise://task/{BLOCK}"),
            format!("sunrise://task/{TASK}?action=detonate"),
            "sunrise://entity/xxx_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            "sunrise://entity/tsk_short".to_owned(),
            // Not a ULID: `U` is outside Crockford's alphabet.
            "sunrise://entity/tsk_01ARZ3NDEKTSV4RRFFQ69G5FAU".to_owned(),
            "sunrise://capture".to_owned(),
            "sunrise://capture?text=%20%20".to_owned(),
            "sunrise://capture?text=%ZZ".to_owned(),
            format!("sunrise://focus/{BLOCK}"),
            "sunrise://focus".to_owned(),
            "sunrise://share/abc123".to_owned(),
            "not a link".to_owned(),
        ];
        for raw in refused {
            assert_eq!(link(&raw), None, "{raw} should be ignored");
        }
    }

    #[test]
    fn every_link_survives_a_round_trip() {
        let t = id(TASK);
        let all = [
            DeepLink::MorningSummary,
            DeepLink::EndOfDayPlan,
            DeepLink::Entity(id(BLOCK)),
            DeepLink::Capture("Buy milk #Errands".into()),
            DeepLink::Capture("a+b & c=d? 100% ünïcode".into()),
            DeepLink::Task(t, TaskLinkAction::Open),
            DeepLink::Task(t, TaskLinkAction::Complete),
            DeepLink::Task(t, TaskLinkAction::Snooze(SnoozeSpan::OneHour)),
            DeepLink::Task(t, TaskLinkAction::Snooze(SnoozeSpan::Tomorrow)),
            DeepLink::Task(t, TaskLinkAction::Snooze(SnoozeSpan::NextWeek)),
            DeepLink::Focus(t),
        ];
        for original in all {
            let url = original.to_url();
            assert_eq!(DeepLink::parse(&url), Some(original.clone()), "{url}");
        }
    }

    #[test]
    fn the_built_links_are_the_documented_shapes() {
        let t = id(TASK);
        assert_eq!(DeepLink::MorningSummary.to_url(), "sunrise://morning");
        assert_eq!(
            DeepLink::Task(t, TaskLinkAction::Snooze(SnoozeSpan::OneHour)).to_url(),
            format!("sunrise://task/{TASK}?action=snooze_1h")
        );
        assert_eq!(
            DeepLink::Capture("Buy milk".into()).to_url(),
            "sunrise://capture?text=Buy%20milk"
        );
    }
}
