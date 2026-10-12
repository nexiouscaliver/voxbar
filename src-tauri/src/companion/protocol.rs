//! The companion wire protocol, as pure data. TEXT control frames carry
//! JSON; BINARY frames carry raw little-endian f32 16 kHz mono audio. The
//! frame types and the session-boundary guardrails (15-minute session cap,
//! 2x-realtime input cap) are decided here so tests pin them without a
//! socket.

use std::time::{Duration, Instant};

/// 16 kHz mono capture, matching the remote recorder's fixed input rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// A recording session hard-caps at 15 minutes; the server auto-finalizes
/// (synthesizes the release edge) so a forgotten open mic cannot run the
/// ring and the model forever.
pub const SESSION_CAP: Duration = Duration::from_secs(15 * 60);

/// Input is capped at ~2x realtime (with a one-second slack burst for
/// network jitter) against ring flooding: a compromised client cannot
/// fast-forward audio into the recorder faster than speech could arrive.
pub const INPUT_RATE_MULTIPLIER: f64 = 2.0;

/// Decode a binary frame into f32 samples (little-endian). Trailing partial
/// samples (a torn frame) are dropped, never guessed. Non-finite samples
/// (NaN or infinity, from a hostile or buggy client) are scrubbed to 0.0 and
/// counted in the returned total: a single NaN poisons Silero's voice
/// probability into a permanent unvoiced read (`NaN > threshold` is false),
/// which withholds the entire recording - the v1.6.3 companion-zero-audio
/// defect - so no non-finite sample may ever reach the VAD by construction.
pub fn decode_f32_le(bytes: &[u8]) -> (Vec<f32>, usize) {
    let mut non_finite = 0;
    let samples = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            if v.is_finite() {
                v
            } else {
                non_finite += 1;
                0.0
            }
        })
        .collect();
    (samples, non_finite)
}

/// How many companion-source history entries the RecentHistory frame (and
/// the matching Tauri command) carries: the phone's recent list window.
pub const RECENT_HISTORY_LIMIT: usize = 5;

/// Where a companion session's final transcript lands. The wire values are
/// the strings the phone sends on its Ptt edges ("pc" | "phone").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputTarget {
    /// Paste on the Mac, exactly as every session behaved before v1.6.0.
    #[default]
    Pc,
    /// Deliver the transcript to the phone as a TranscriptResult frame; no
    /// Mac paste.
    Phone,
}

impl OutputTarget {
    /// Parse one wire value; None means the field was absent or carried an
    /// unrecognized string (the connection keeps its previous value).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "pc" => Some(OutputTarget::Pc),
            "phone" => Some(OutputTarget::Phone),
            _ => None,
        }
    }
}

/// The per-session preferences the server honors for one companion
/// connection's dictations: where the transcript goes and whether the
/// post-process leg runs. Defaults are exactly the pre-v1.6.0 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionPrefs {
    pub output_target: OutputTarget,
    pub post_process: bool,
}

/// The preference fields one Ptt edge may carry. Every field is optional:
/// absent (or unrecognized) means "unchanged", so an older phone that sends
/// the bare frame keeps working and a newer phone only needs to send a
/// field when it wants to change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionPrefsUpdate {
    pub target: Option<OutputTarget>,
    pub post_process: Option<bool>,
}

impl SessionPrefs {
    /// Fold one edge's partial update into the connection's prefs.
    pub fn apply(&mut self, update: SessionPrefsUpdate) {
        if let Some(target) = update.target {
            self.output_target = target;
        }
        if let Some(post_process) = update.post_process {
            self.post_process = post_process;
        }
    }
}

/// Client-to-server TEXT control frames.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientFrame {
    /// First frame on a fresh socket: the pairing token from the QR URL, a
    /// human device name, and (v1.6.2 fleet pass 1) the phone's persistent
    /// device id. The name is a UA-derived PLATFORM token every
    /// same-platform phone shares (two iPhones both say "iPhone", every
    /// Chrome-Android phone says "Linux"), so it cannot identify one phone;
    /// the id disambiguates same-platform phones (see
    /// [`device_identity_name`]). Older clients send no id.
    Hello {
        token: String,
        device: String,
        device_id: Option<String>,
        lang: Option<String>,
    },
    /// Push-to-talk edges. The Mac resolves mode/hold-threshold from its
    /// own settings, exactly like the keyboard binding. The optional
    /// per-session preferences ride every edge (v1.6.0); the server snaps
    /// them up on the press edge that starts a session.
    Ptt {
        pressed: bool,
        prefs: SessionPrefsUpdate,
    },
}

/// Parse one TEXT frame. Unknown types and malformed JSON return None (the
/// server drops the connection on protocol garbage).
pub fn parse_client_frame(text: &str) -> Option<ClientFrame> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    match value.get("type")?.as_str()? {
        "hello" => Some(ClientFrame::Hello {
            token: value.get("token")?.as_str()?.to_string(),
            device: value
                .get("device")
                .and_then(|d| d.as_str())
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .unwrap_or("Companion")
                .chars()
                .take(64)
                .collect(),
            device_id: value
                .get("deviceId")
                .and_then(|d| d.as_str())
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(|d| d.chars().take(64).collect()),
            lang: value
                .get("lang")
                .and_then(|l| l.as_str())
                .map(|l| l.chars().take(16).collect()),
        }),
        "ptt" => Some(ClientFrame::Ptt {
            pressed: value.get("pressed")?.as_bool()?,
            prefs: SessionPrefsUpdate {
                target: value
                    .get("target")
                    .and_then(|t| t.as_str())
                    .and_then(OutputTarget::from_wire),
                post_process: value.get("postProcess").and_then(|p| p.as_bool()),
            },
        }),
        _ => None,
    }
}

/// v1.6.2 fleet pass 1: the connection's IDENTITY name, built from the
/// hello. The human `device` name is a UA platform token every
/// same-platform phone shares (two iPhones both hello "iPhone", every
/// Chrome-Android phone "Linux"), so per-phone data keyed on it alone
/// collapses same-platform phones into one Recent list - private
/// phone-destined dictations included. The client's persistent device id
/// disambiguates: identity is `name#tag`, where the tag is the id's first
/// four alphanumeric characters (client ids are UUID-ish; the filter keeps
/// a hostile id from smuggling length or separators into a display
/// string). A client with no id keeps the bare name, exactly the
/// pre-existing behavior. Pure, so the rule is pinnable.
pub fn device_identity_name(device: &str, device_id: Option<&str>) -> String {
    let Some(id) = device_id else {
        return device.to_string();
    };
    let tag: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect();
    if tag.is_empty() {
        device.to_string()
    } else {
        format!("{device}#{tag}")
    }
}

/// One companion-source history entry, as the phone's recent list renders
/// it: the deliverable text (post-processed when the session polished it,
/// else the raw transcript) plus the entry timestamp.
#[derive(Debug, Clone, PartialEq, serde::Serialize, specta::Type)]
pub struct RecentHistoryEntry {
    pub text: String,
    pub timestamp: i64,
}

/// Server-to-client TEXT control frames.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    /// A successful hello: the activation semantics the Mac will apply to
    /// this client's edges, plus the current recording state.
    Welcome {
        activation: &'static str,
        hold_threshold_ms: u64,
        recording: bool,
    },
    /// Recording-state broadcasts (session started / finished).
    State { recording: bool },
    /// A notice code forwarded from the Mac's overlay notice channel (busy
    /// arbitration, disconnects) so a swallowed press is never silent on
    /// the phone either.
    Notice { code: String },
    /// Phone-target delivery (v1.6.0): the final transcript of a companion
    /// session whose output target is the phone. `post_processed` says the
    /// text went through the post-process leg; `model` is the model id that
    /// actually produced it (empty when unknown).
    TranscriptResult {
        text: String,
        post_processed: bool,
        model: String,
    },
    /// The last few companion-source history entries (text + timestamp), so
    /// the phone can render its recent list at welcome and refresh it after
    /// every companion session. Broadcast; see RECENT_HISTORY_LIMIT.
    RecentHistory { entries: Vec<RecentHistoryEntry> },
    /// A refused hello (bad token, wrong subnet, rate limited).
    Error { code: &'static str },
    /// The server is closing the connection (session cap, rate cap, stop).
    Goodbye { reason: &'static str },
}

impl ServerFrame {
    pub fn to_json(&self) -> String {
        match self {
            ServerFrame::Welcome {
                activation,
                hold_threshold_ms,
                recording,
            } => serde_json::json!({
                "type": "welcome",
                "activation": activation,
                "holdThresholdMs": hold_threshold_ms,
                "recording": recording,
            })
            .to_string(),
            ServerFrame::State { recording } => serde_json::json!({
                "type": "state",
                "recording": recording,
            })
            .to_string(),
            ServerFrame::Notice { code } => serde_json::json!({
                "type": "notice",
                "code": code,
            })
            .to_string(),
            ServerFrame::TranscriptResult {
                text,
                post_processed,
                model,
            } => serde_json::json!({
                "type": "transcript",
                "text": text,
                "postProcessed": post_processed,
                "model": model,
            })
            .to_string(),
            ServerFrame::RecentHistory { entries } => serde_json::json!({
                "type": "recentHistory",
                "entries": entries.iter().map(|e| serde_json::json!({
                    "text": e.text,
                    "timestamp": e.timestamp,
                })).collect::<Vec<_>>(),
            })
            .to_string(),
            ServerFrame::Error { code } => serde_json::json!({
                "type": "error",
                "code": code,
            })
            .to_string(),
            ServerFrame::Goodbye { reason } => serde_json::json!({
                "type": "goodbye",
                "reason": reason,
            })
            .to_string(),
        }
    }
}

/// v1.6.2 W1-L0: decides when audio may flow into the recorder and when
/// the hard cap force-finalizes, from OBSERVED session truth rather than
/// raw Ptt edges. A hold-or-toggle tap releases the button within
/// milliseconds while the session it locked stays live until the next
/// press, so the pre-v1.6.2 edge tracker (admission open between press
/// and release) closed its admission window at the finger-lift and
/// silently dropped every chunk after the first: VAD saw one leading
/// silence chunk, finish collected zero samples, the zero-audio notice
/// fired on every healthy tap session. The server feeds this the
/// manager's lane-scoped session state (`session_live` plus the
/// `session_lane` frozen from this connection's own starting press)
/// wherever it can change: every received frame and every keepalive tick,
/// so admission follows exactly the state the State frames broadcast to
/// the phone (the phone itself refuses to stream before that broadcast).
#[derive(Debug)]
pub struct SessionAdmission {
    /// The Mac's live session is owned by THIS connection's lane.
    live: bool,
    /// When that session was first observed live; the cap clock.
    live_since: Option<Instant>,
    cap: Duration,
}

impl SessionAdmission {
    pub fn new() -> Self {
        SessionAdmission {
            live: false,
            live_since: None,
            cap: SESSION_CAP,
        }
    }

    /// Observe the session truth for this connection's lane. True only
    /// while the Mac's live companion session was started by THIS
    /// connection's own press (the lane its starting edge carried); a
    /// spectator phone, a reconnecting phone, and a keyboard dictation
    /// all observe false and their audio stays out-of-session.
    pub fn observe(&mut self, now: Instant, live_for_lane: bool) {
        if live_for_lane {
            if self.live_since.is_none() {
                self.live_since = Some(now);
            }
            self.live = true;
        } else {
            self.live = false;
            self.live_since = None;
        }
    }

    /// Whether an audio chunk may enter the recorder right now.
    pub fn admits(&self) -> bool {
        self.live
    }

    /// True exactly once per observed-live run past the cap: the caller
    /// must force-finalize (a locked tap session ignores release edges, so
    /// only the forced finalize can end it). Continued observation of a
    /// still-live session re-arms the clock, so a finalize that failed to
    /// land retries at the next cap interval instead of never; the normal
    /// path clears the clock first, through the false observation the
    /// stop pipeline's session-state change produces.
    pub fn cap_exceeded(&mut self, now: Instant) -> bool {
        let Some(started) = self.live_since else {
            return false;
        };
        if now.duration_since(started) >= self.cap {
            self.live_since = None;
            return true;
        }
        false
    }
}

impl Default for SessionAdmission {
    fn default() -> Self {
        Self::new()
    }
}

/// Accepts audio at up to `multiplier`x realtime (plus a one-second slack
/// burst); anything faster is flooding and disconnects the client.
#[derive(Debug)]
pub struct InputRateCap {
    opened_at: Instant,
    samples: u64,
    multiplier: f64,
    /// Allowance that has not been spent yet (starts at one second).
    slack_samples: f64,
}

impl InputRateCap {
    pub fn new(now: Instant) -> Self {
        InputRateCap {
            opened_at: now,
            samples: 0,
            multiplier: INPUT_RATE_MULTIPLIER,
            slack_samples: f64::from(SAMPLE_RATE),
        }
    }

    /// Admit `count` more samples; false means the cap was breached (the
    /// samples are NOT admitted and the connection must drop).
    pub fn admit(&mut self, count: usize, now: Instant) -> bool {
        let elapsed = now.duration_since(self.opened_at).as_secs_f64();
        let allowance = elapsed * f64::from(SAMPLE_RATE) * self.multiplier + self.slack_samples;
        if self.samples as f64 + count as f64 > allowance {
            return false;
        }
        self.samples += count as u64;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_frame_parses_token_device_and_lang() {
        let frame = parse_client_frame(
            r#"{"type":"hello","token":"abc123","device":"Pixel 7","lang":"de"}"#,
        )
        .expect("valid hello");
        assert_eq!(
            frame,
            ClientFrame::Hello {
                token: "abc123".to_string(),
                device: "Pixel 7".to_string(),
                device_id: None,
                lang: Some("de".to_string()),
            }
        );
    }

    #[test]
    fn hello_defaults_the_device_name_and_trims() {
        let frame = parse_client_frame(r#"{"type":"hello","token":"t","device":"   "}"#).unwrap();
        assert_eq!(
            frame,
            ClientFrame::Hello {
                token: "t".to_string(),
                device: "Companion".to_string(),
                device_id: None,
                lang: None,
            }
        );
    }

    /// Fleet pass 1: the hello carries the phone's persistent device id,
    /// the identity half the UA platform name cannot provide.
    #[test]
    fn hello_parses_the_persistent_device_id() {
        let frame = parse_client_frame(
            r#"{"type":"hello","token":"t","device":"iPhone","deviceId":" 7f3a9c2e-1111-2222-3333-444455556666 "}"#,
        )
        .unwrap();
        assert_eq!(
            frame,
            ClientFrame::Hello {
                token: "t".to_string(),
                device: "iPhone".to_string(),
                device_id: Some("7f3a9c2e-1111-2222-3333-444455556666".to_string()),
                lang: None,
            }
        );
        // Whitespace-only or absent ids read as None (an old client).
        assert!(matches!(
            parse_client_frame(r#"{"type":"hello","token":"t","device":"iPhone","deviceId":"  "}"#),
            Some(ClientFrame::Hello {
                device_id: None,
                ..
            })
        ));
        assert!(matches!(
            parse_client_frame(r#"{"type":"hello","token":"t","device":"iPhone"}"#),
            Some(ClientFrame::Hello {
                device_id: None,
                ..
            })
        ));
    }

    /// Fleet pass 1: the per-phone identity rule, pinned pure. The UA
    /// platform name alone cannot tell two iPhones (or two Chrome-Android
    /// phones, which all hello "Linux") apart; the persistent id's short
    /// tag can. Same name + different ids MUST differ (the Recent-list
    /// collision the fleet traced); same id MUST be stable across
    /// reconnects; a hostile id cannot smuggle length or separators into
    /// the display string.
    #[test]
    fn device_identity_name_disambiguates_same_platform_phones() {
        // Two same-platform phones get distinct identities...
        let a = device_identity_name("iPhone", Some("7f3a9c2e-1111-2222-3333-444455556666"));
        let b = device_identity_name("iPhone", Some("c3d4e5f6-1111-2222-3333-444455556666"));
        assert_eq!(a, "iPhone#7f3a");
        assert_eq!(b, "iPhone#c3d4");
        assert_ne!(a, b);
        // ...every Chrome-Android phone's "Linux" collapses the same way...
        assert_eq!(
            device_identity_name("Linux", Some("a1b2c3d4")),
            "Linux#a1b2"
        );
        // ...and the same phone keeps ONE identity across reconnects.
        assert_eq!(
            device_identity_name("iPhone", Some("7f3a9c2e-0000-0000-0000-000000000000")),
            a
        );
        // A hostile id contributes at most four plain alphanumerics - no
        // separators, no length, no injection into the display string.
        assert_eq!(
            device_identity_name("iPhone", Some("###@@--,,,,####!!!!")),
            "iPhone"
        );
        // Old clients with no id keep the bare name (pre-existing
        // behavior, no worse than before the fix).
        assert_eq!(device_identity_name("iPhone", None), "iPhone");
        assert_eq!(device_identity_name("iPhone", Some("   ")), "iPhone");
    }

    #[test]
    fn ptt_edges_parse() {
        // The bare pre-v1.6.0 shape still parses: absent preference fields
        // mean "unchanged", so an older phone keeps working.
        assert_eq!(
            parse_client_frame(r#"{"type":"ptt","pressed":true}"#),
            Some(ClientFrame::Ptt {
                pressed: true,
                prefs: SessionPrefsUpdate::default(),
            })
        );
        assert_eq!(
            parse_client_frame(r#"{"type":"ptt","pressed":false}"#),
            Some(ClientFrame::Ptt {
                pressed: false,
                prefs: SessionPrefsUpdate::default(),
            })
        );
    }

    /// v1.6.0: a Ptt edge may carry the phone's per-session preferences.
    /// Both fields are optional and independently parsed; a non-bool
    /// postProcess or an unrecognized target string reads as "unchanged"
    /// rather than failing the frame (the edge itself must never be
    /// dropped, or the phone's button state desyncs).
    #[test]
    fn ptt_edges_parse_session_preferences() {
        assert_eq!(
            parse_client_frame(
                r#"{"type":"ptt","pressed":true,"target":"phone","postProcess":true}"#
            ),
            Some(ClientFrame::Ptt {
                pressed: true,
                prefs: SessionPrefsUpdate {
                    target: Some(OutputTarget::Phone),
                    post_process: Some(true),
                },
            })
        );
        // Partial updates carry whichever fields the phone sent.
        assert_eq!(
            parse_client_frame(r#"{"type":"ptt","pressed":true,"target":"pc"}"#),
            Some(ClientFrame::Ptt {
                pressed: true,
                prefs: SessionPrefsUpdate {
                    target: Some(OutputTarget::Pc),
                    post_process: None,
                },
            })
        );
        // Unrecognized target and non-bool postProcess read as unchanged.
        assert_eq!(
            parse_client_frame(
                r#"{"type":"ptt","pressed":false,"target":"tv","postProcess":"yes"}"#
            ),
            Some(ClientFrame::Ptt {
                pressed: false,
                prefs: SessionPrefsUpdate::default(),
            })
        );
    }

    /// Prefs are sticky per connection: each edge's partial update folds
    /// into the current values, and defaults reproduce the pre-v1.6.0
    /// behavior (paste on the Mac, no post-process).
    #[test]
    fn session_prefs_apply_partial_updates_over_defaults() {
        let mut prefs = SessionPrefs::default();
        assert_eq!(prefs.output_target, OutputTarget::Pc);
        assert!(!prefs.post_process);

        prefs.apply(SessionPrefsUpdate {
            target: Some(OutputTarget::Phone),
            post_process: None,
        });
        assert_eq!(prefs.output_target, OutputTarget::Phone);
        assert!(!prefs.post_process, "an absent field never changes a value");

        prefs.apply(SessionPrefsUpdate {
            target: None,
            post_process: Some(true),
        });
        assert_eq!(prefs.output_target, OutputTarget::Phone);
        assert!(prefs.post_process);

        // A no-op update changes nothing.
        prefs.apply(SessionPrefsUpdate::default());
        assert_eq!(prefs.output_target, OutputTarget::Phone);
        assert!(prefs.post_process);

        prefs.apply(SessionPrefsUpdate {
            target: Some(OutputTarget::Pc),
            post_process: Some(false),
        });
        assert_eq!(prefs, SessionPrefs::default());
    }

    #[test]
    fn garbage_and_unknown_frames_are_refused() {
        assert!(parse_client_frame("not json").is_none());
        assert!(parse_client_frame(r#"{"type":"surprise"}"#).is_none());
        assert!(parse_client_frame(r#"{"type":"ptt"}"#).is_none());
        // Missing token in hello is fatal: no tokenless handshake exists.
        assert!(parse_client_frame(r#"{"type":"hello","device":"x"}"#).is_none());
    }

    #[test]
    fn binary_audio_decodes_little_endian_f32() {
        let bytes: Vec<u8> = [1.0f32, -0.5, 2.5]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (samples, non_finite) = decode_f32_le(&bytes);
        assert_eq!(samples, vec![1.0, -0.5, 2.5]);
        assert_eq!(non_finite, 0, "finite audio scrubs nothing");
        // A torn trailing sample is dropped, not guessed.
        let mut torn = bytes.clone();
        torn.extend_from_slice(&[0, 0]);
        let (samples, non_finite) = decode_f32_le(&torn);
        assert_eq!(samples, vec![1.0, -0.5, 2.5]);
        assert_eq!(non_finite, 0);
    }

    /// v1.6.3 defense in depth: the decode boundary scrubs non-finite
    /// samples to 0.0 and reports how many, so a hostile or buggy client
    /// can never again wedge Silero into a permanent unvoiced
    /// classification (the whole-stream NaN poisoning of the companion
    /// zero-audio defect passed through here unfiltered in v1.6.2).
    #[test]
    fn binary_audio_decode_scrubs_non_finite_samples_to_zero_and_counts_them() {
        let nan = f32::NAN.to_le_bytes();
        let inf = f32::INFINITY.to_le_bytes();
        let neg_inf = f32::NEG_INFINITY.to_le_bytes();
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0.25f32.to_le_bytes());
        bytes.extend_from_slice(&nan);
        bytes.extend_from_slice(&inf);
        bytes.extend_from_slice(&(-0.75f32).to_le_bytes());
        bytes.extend_from_slice(&neg_inf);
        let (samples, non_finite) = decode_f32_le(&bytes);
        assert_eq!(samples, vec![0.25, 0.0, 0.0, -0.75, 0.0]);
        assert_eq!(non_finite, 3, "NaN, +inf and -inf are each counted");
    }

    #[test]
    fn server_frames_serialize_to_the_wire_shape() {
        let welcome = ServerFrame::Welcome {
            activation: "hold_or_toggle",
            hold_threshold_ms: 300,
            recording: false,
        }
        .to_json();
        assert!(welcome.contains(r#""type":"welcome""#));
        assert!(welcome.contains(r#""holdThresholdMs":300"#));
        assert!(ServerFrame::State { recording: true }
            .to_json()
            .contains(r#""recording":true"#));
        assert!(ServerFrame::Notice {
            code: "binding_busy".into()
        }
        .to_json()
        .contains(r#""code":"binding_busy""#));
        assert!(ServerFrame::Goodbye {
            reason: "session_cap"
        }
        .to_json()
        .contains("session_cap"));
    }

    /// v1.6.0 wire shapes: the phone-target transcript delivery frame and
    /// the recent-history list frame serialize to the exact JSON the phone
    /// client's switch keys on ("transcript" / "recentHistory", camelCase
    /// field names matching the Welcome frame's style).
    #[test]
    fn transcript_and_recent_history_frames_serialize_to_the_wire_shape() {
        let transcript = ServerFrame::TranscriptResult {
            text: "hello world".to_string(),
            post_processed: true,
            model: "whisper-large-v3-turbo".to_string(),
        }
        .to_json();
        assert!(transcript.contains(r#""type":"transcript""#));
        assert!(transcript.contains(r#""text":"hello world""#));
        assert!(transcript.contains(r#""postProcessed":true"#));
        assert!(transcript.contains(r#""model":"whisper-large-v3-turbo""#));

        let history = ServerFrame::RecentHistory {
            entries: vec![
                RecentHistoryEntry {
                    text: "first entry".to_string(),
                    timestamp: 1_750_000_000,
                },
                RecentHistoryEntry {
                    text: "second entry".to_string(),
                    timestamp: 1_750_000_060,
                },
            ],
        }
        .to_json();
        assert!(history.contains(r#""type":"recentHistory""#));
        assert!(history.contains(r#""text":"first entry""#));
        assert!(history.contains(r#""timestamp":1750000000"#));
        assert!(history.contains(r#""text":"second entry""#));

        // The empty list frame is still a valid frame (a fresh install has
        // no companion history yet).
        let empty = ServerFrame::RecentHistory { entries: vec![] }.to_json();
        assert!(empty.contains(r#""type":"recentHistory""#));
        assert!(empty.contains(r#""entries":[]"#));
    }

    /// v1.6.2 W1-L0 pin: admission follows observed session truth, never
    /// the raw edges. The defect this replaces had admission open between
    /// a press edge and its release edge, so a hold-or-toggle TAP (press,
    /// sub-threshold release, session locked live) admitted only the
    /// finger-hold sliver and dropped every later chunk. Here the release
    /// edge is not even an input: admission stays open across any number
    /// of further observations while the session stays live, and closes
    /// only when the Mac's session state actually goes false.
    #[test]
    fn session_admission_follows_observed_truth_not_release_edges() {
        let mut admission = SessionAdmission::new();
        let t0 = Instant::now();
        assert!(!admission.admits(), "no session has ever been live");

        // The press edge is forwarded (the start commits behind it); the
        // truth it leaves behind is still false, and audio stays refused.
        admission.observe(t0, false);
        assert!(!admission.admits(), "the press edge alone admits nothing");

        // The start committed: the State broadcast went out and the phone
        // began streaming; the truth is now live for this lane.
        let t1 = t0 + Duration::from_millis(62);
        admission.observe(t1, true);
        assert!(admission.admits(), "a live session admits audio");

        // The tap's release edge arrived (and did NOT close admission),
        // then every subsequent chunk observes the session still live.
        for ms in [120u64, 400, 1_500, 60_000, 120_000] {
            admission.observe(t0 + Duration::from_millis(ms), true);
            assert!(
                admission.admits(),
                "a locked tap session keeps admitting at {ms}ms past the release edge"
            );
        }

        // The second tap's stop pipeline landed: the session state went
        // false, and audio from this point is out-of-session again.
        admission.observe(t0 + Duration::from_secs(150), false);
        assert!(!admission.admits());
    }

    /// v1.6.2 W1-L0 pin: the cap clock also follows session truth, so a
    /// locked tap session (whose release edge used to kill the tracker
    /// and with it the cap) still force-finalizes at 15 minutes. The
    /// clock arms at the FIRST live observation, fires exactly once, and
    /// re-arms only through continued observation of a still-live
    /// session.
    #[test]
    fn session_admission_caps_a_locked_session_at_fifteen_minutes() {
        let mut admission = SessionAdmission::new();
        let t0 = Instant::now();
        assert!(!admission.cap_exceeded(t0), "idle never caps");

        admission.observe(t0, true);
        assert!(!admission.cap_exceeded(t0 + Duration::from_secs(14 * 60)));
        // The tap's release edge changed nothing; chunks keep observing
        // the session live, and the cap still fires at the threshold.
        admission.observe(t0 + Duration::from_secs(14 * 60), true);
        assert!(admission.cap_exceeded(t0 + Duration::from_secs(15 * 60)));

        // The forced finalize landed and the session state went false: no
        // refire, and the next session arms a fresh clock.
        admission.observe(t0 + Duration::from_secs(15 * 60 + 5), false);
        assert!(!admission.cap_exceeded(t0 + Duration::from_secs(16 * 60)));
        admission.observe(t0 + Duration::from_secs(17 * 60), true);
        assert!(!admission.cap_exceeded(t0 + Duration::from_secs(17 * 60 + 14 * 60)));
        assert!(admission.cap_exceeded(t0 + Duration::from_secs(17 * 60 + 15 * 60)));

        // A finalize that FAILED to land leaves the session live; the
        // continued live observation re-arms the clock so the cap retries
        // at the next interval instead of never (the re-arm happens at
        // the observation instant, one second past the first fire).
        let refire_armed = t0 + Duration::from_secs(17 * 60 + 15 * 60 + 1);
        admission.observe(refire_armed, true);
        assert!(!admission.cap_exceeded(refire_armed + Duration::from_secs(14 * 60)));
        assert!(admission.cap_exceeded(refire_armed + Duration::from_secs(15 * 60)));

        // A session that never went live never caps (a swallowed press
        // used to arm the old tracker and fire a spurious cap notice).
        let mut idle = SessionAdmission::new();
        idle.observe(t0, false);
        assert!(!idle.cap_exceeded(t0 + Duration::from_secs(60 * 60)));
    }

    #[test]
    fn input_rate_cap_admits_realtime_and_rejects_flooding() {
        let t0 = Instant::now();
        let mut cap = InputRateCap::new(t0);

        // One second of audio, immediately: the one-second slack covers it.
        assert!(cap.admit(16_000, t0));
        // A second full second of audio in the same instant is already
        // flooding (nothing near 2x realtime could justify it).
        assert!(!cap.admit(16_000, t0));

        // Two seconds later, several seconds of budget accrued.
        let t2 = t0 + Duration::from_secs(2);
        assert!(cap.admit(32_000, t2));
        // Way past 2x realtime: flooding.
        assert!(!cap.admit(200_000, t2));
    }
}
