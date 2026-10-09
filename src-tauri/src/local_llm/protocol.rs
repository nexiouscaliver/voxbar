//! Line-oriented request/response protocol between the swap runner (parent)
//! and the `--llm-worker` child process (spec 3.3).
//!
//! One JSON object per line: the parent writes requests to the child's
//! stdin, the child writes responses to its stdout. The child exits when
//! stdin closes (parent death included); it never loops on a closed stdin.
//! A malformed or unknown frame is an ERROR, never a panic: the reader
//! answers `Failed` and stays alive, so one bad frame cannot wedge the
//! worker.
//!
//! Pure data plus serde; no engine imports anywhere in this file (spec 10
//! hermeticity contract: unit tests never link or execute llama.cpp).

use serde::{Deserialize, Serialize};

/// Context window the worker opens. Budgeted so the system prompt and
/// template wrapping (256-token reserve) plus the 1200-est-token input cap
/// plus the minimum 192-token output floor always fit with margin.
pub const WORKER_N_CTX: u32 = 4096;

/// A request from the parent, one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// Load the GGUF at `path` with a context of `n_ctx` tokens. Must be
    /// the first request of a session; responses `Loaded` or `Failed`.
    Load { path: String, n_ctx: u32 },
    /// Grammar-constrained greedy completion. `grammar` is a GBNF string
    /// (the parent renders it from the JSON schema via
    /// json_schema_to_grammar). Responses `Generated` or `Failed`.
    Generate {
        system: String,
        user: String,
        grammar: Option<String>,
        max_gen_tokens: u32,
    },
    /// Best-effort cancellation marker. The parent does NOT rely on it for
    /// prompt cancellation (generation is not resumable, so cancel = kill
    /// the child); it exists so a future streaming worker has a frame.
    /// No response is sent.
    Cancel,
    /// Graceful shutdown: finish any in-flight response, then exit 0. No
    /// response is sent; the parent observes process exit.
    Exit,
}

/// A response from the worker, one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerResponse {
    Loaded,
    /// The full completion text (nothing is streamed).
    Generated {
        text: String,
    },
    Failed {
        reason: String,
    },
}

/// Serialize a frame as one protocol line. Never embeds a newline in the
/// payload: serde_json escapes control characters, so the output is always
/// a single line ending in exactly one '\n'.
pub fn to_line<T: Serialize>(frame: &T) -> String {
    let mut line = serde_json::to_string(frame).expect("protocol frames are always serializable");
    line.push('\n');
    line
}

/// Parse a request line. Any unknown or malformed frame is an Err, never a
/// panic (T25): the caller answers Failed and keeps the worker alive.
pub fn parse_request_line(line: &str) -> Result<WorkerRequest, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("malformed request frame: {}", e))?;
    if !value.is_object() {
        return Err("request frame must be a JSON object".to_string());
    }
    let frame_type = value
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "request frame is missing the type tag".to_string())?
        .to_string();
    match frame_type.as_str() {
        "load" | "generate" | "cancel" | "exit" => serde_json::from_value(value)
            .map_err(|e| format!("invalid '{}' request frame: {}", frame_type, e)),
        other => Err(format!("unknown request frame type '{}'", other)),
    }
}

/// Parse a response line; same never-panic contract as
/// [`parse_request_line`].
pub fn parse_response_line(line: &str) -> Result<WorkerResponse, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("malformed response frame: {}", e))?;
    if !value.is_object() {
        return Err("response frame must be a JSON object".to_string());
    }
    let frame_type = value
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "response frame is missing the type tag".to_string())?
        .to_string();
    match frame_type.as_str() {
        "loaded" | "generated" | "failed" => serde_json::from_value(value)
            .map_err(|e| format!("invalid '{}' response frame: {}", frame_type, e)),
        other => Err(format!("unknown response frame type '{}'", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T25: every frame survives a serialize -> parse round-trip with no
    /// field loss, and every serialized frame is exactly one line (the wire
    /// contract the worker's line loop depends on).
    #[test]
    fn every_frame_round_trips_through_one_line() {
        let requests = [
            WorkerRequest::Load {
                path: "/models/qwen.gguf".to_string(),
                n_ctx: WORKER_N_CTX,
            },
            WorkerRequest::Generate {
                system: "clean the transcript".to_string(),
                user: "um hello world\n/no_think".to_string(),
                grammar: Some("root ::= \"{\"".to_string()),
                max_gen_tokens: 352,
            },
            WorkerRequest::Generate {
                system: String::new(),
                user: String::new(),
                grammar: None,
                max_gen_tokens: 192,
            },
            WorkerRequest::Cancel,
            WorkerRequest::Exit,
        ];
        for request in requests {
            let line = to_line(&request);
            assert_eq!(line.matches('\n').count(), 1, "one line per frame");
            assert!(line.ends_with('\n'));
            let parsed = parse_request_line(line.trim_end()).expect("round-trip parses");
            assert_eq!(parsed, request);
        }

        let responses = [
            WorkerResponse::Loaded,
            WorkerResponse::Generated {
                text: "{\"transcription\": \"Hello.\"}".to_string(),
            },
            WorkerResponse::Generated {
                text: "multi\nline\twith\u{200b}invisibles".to_string(),
            },
            WorkerResponse::Failed {
                reason: "gguf parse error".to_string(),
            },
        ];
        for response in responses {
            let line = to_line(&response);
            assert_eq!(line.matches('\n').count(), 1, "one line per frame");
            let parsed = parse_response_line(line.trim_end()).expect("round-trip parses");
            assert_eq!(parsed, response);
        }
    }

    /// T25: unknown and malformed frames are errors, never panics. The
    /// worker must be able to answer Failed and keep serving.
    #[test]
    fn unknown_or_malformed_frames_error_never_panic() {
        // Unknown type tags on both directions.
        assert!(parse_request_line(r#"{"type":"dream"}"#).is_err());
        assert!(parse_response_line(r#"{"type":"dream"}"#).is_err());
        // Missing type tag, non-object payloads, and broken JSON.
        assert!(parse_request_line(r#"{"path":"/x"}"#).is_err());
        assert!(parse_request_line("not json at all").is_err());
        assert!(parse_request_line("[1,2,3]").is_err());
        assert!(parse_response_line("}").is_err());
        assert!(parse_response_line("42").is_err());
        // Known tag with invalid fields is still an error, not a panic.
        assert!(parse_request_line(r#"{"type":"load"}"#).is_err());
        assert!(parse_response_line(r#"{"type":"generated"}"#).is_err());
        // A known frame mixed with trailing garbage does not parse either.
        let line = to_line(&WorkerRequest::Cancel);
        assert!(parse_request_line(&format!("{}garbage", line.trim_end())).is_err());
        // And the error paths really are reachable without panicking.
        for bad in ["", " ", "\n", "null"] {
            let _ = parse_request_line(bad);
            let _ = parse_response_line(bad);
        }
    }

    /// A payload containing literal newlines and quotes still serializes to
    /// a single line: serde_json escapes control characters. The worker's
    /// line reader depends on this invariant for transcript-sized payloads.
    #[test]
    fn newlines_in_payloads_stay_on_one_line() {
        let request = WorkerRequest::Generate {
            system: "line1\nline2 \"quoted\"".to_string(),
            user: "transcript\nwith\nnewlines".to_string(),
            grammar: None,
            max_gen_tokens: 192,
        };
        let line = to_line(&request);
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(
            parse_request_line(line.trim_end()).expect("parses"),
            request
        );
    }
}
