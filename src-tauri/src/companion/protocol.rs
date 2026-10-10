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
/// samples (a torn frame) are dropped, never guessed.
pub fn decode_f32_le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Client-to-server TEXT control frames.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientFrame {
    /// First frame on a fresh socket: the pairing token from the QR URL and
    /// a human device name.
    Hello {
        token: String,
        device: String,
        lang: Option<String>,
    },
    /// Push-to-talk edges. The Mac resolves mode/hold-threshold from its
    /// own settings, exactly like the keyboard binding.
    Ptt { pressed: bool },
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
            lang: value
                .get("lang")
                .and_then(|l| l.as_str())
                .map(|l| l.chars().take(16).collect()),
        }),
        "ptt" => Some(ClientFrame::Ptt {
            pressed: value.get("pressed")?.as_bool()?,
        }),
        _ => None,
    }
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

/// Tracks the recording-session boundary for guardrails: when audio may
/// flow, and when the hard cap forces an auto-finalize.
#[derive(Debug)]
pub struct SessionTracker {
    recording_since: Option<Instant>,
    cap: Duration,
}

impl SessionTracker {
    pub fn new() -> Self {
        SessionTracker {
            recording_since: None,
            cap: SESSION_CAP,
        }
    }

    pub fn recording(&self) -> bool {
        self.recording_since.is_some()
    }

    pub fn press(&mut self, now: Instant) {
        if self.recording_since.is_none() {
            self.recording_since = Some(now);
        }
    }

    pub fn release(&mut self) {
        self.recording_since = None;
    }

    /// True exactly once when the live session has run past the cap: the
    /// caller must synthesize the release edge (auto-finalize).
    pub fn cap_exceeded(&mut self, now: Instant) -> bool {
        let Some(started) = self.recording_since else {
            return false;
        };
        if now.duration_since(started) >= self.cap {
            self.recording_since = None;
            return true;
        }
        false
    }
}

impl Default for SessionTracker {
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
                lang: None,
            }
        );
    }

    #[test]
    fn ptt_edges_parse() {
        assert_eq!(
            parse_client_frame(r#"{"type":"ptt","pressed":true}"#),
            Some(ClientFrame::Ptt { pressed: true })
        );
        assert_eq!(
            parse_client_frame(r#"{"type":"ptt","pressed":false}"#),
            Some(ClientFrame::Ptt { pressed: false })
        );
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
        assert_eq!(decode_f32_le(&bytes), vec![1.0, -0.5, 2.5]);
        // A torn trailing sample is dropped, not guessed.
        let mut torn = bytes.clone();
        torn.extend_from_slice(&[0, 0]);
        assert_eq!(decode_f32_le(&torn), vec![1.0, -0.5, 2.5]);
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

    #[test]
    fn session_tracker_caps_at_fifteen_minutes_exactly_once() {
        let mut tracker = SessionTracker::new();
        let t0 = Instant::now();
        assert!(!tracker.recording());
        assert!(!tracker.cap_exceeded(t0), "idle never caps");

        tracker.press(t0);
        assert!(tracker.recording());
        assert!(!tracker.cap_exceeded(t0 + Duration::from_secs(14 * 60)));
        assert!(tracker.cap_exceeded(t0 + Duration::from_secs(15 * 60)));
        // Fired once: the synthesized release already ended the session.
        assert!(!tracker.recording());
        assert!(!tracker.cap_exceeded(t0 + Duration::from_secs(16 * 60)));

        // A second session starts fresh.
        tracker.press(t0 + Duration::from_secs(17 * 60));
        assert!(!tracker.cap_exceeded(t0 + Duration::from_secs(17 * 60 + 60)));
        tracker.release();
        assert!(!tracker.recording());
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
