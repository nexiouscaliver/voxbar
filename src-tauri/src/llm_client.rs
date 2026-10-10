use crate::settings::PostProcessProvider;
use log::{debug, error, info};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use specta::Type;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::sync::{Mutex, OnceLock};

/// Failure classes shared across the post-process surface: the Test
/// Connection verdict line here, and the pp: observability lifecycle that
/// classifies every failed run. The wire values are stable tokens the UI
/// prints verbatim (auth, network, timeout, context_length,
/// output_invalid, oom, cancelled), so they are part of the contract with
/// the frontend and must never be renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PostProcessFailureClass {
    Auth,
    Network,
    Timeout,
    ContextLength,
    OutputInvalid,
    Oom,
    Cancelled,
}

/// Structured error for the cloud model-list path (and the connection
/// probe), replacing the bare String `fetch_post_process_models` used to
/// return. The tag/kind is the failure class the UI prints; `detail`
/// carries the sanitized diagnostics (never key material, never response
/// payloads that could quote transcription content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PostProcessModelError {
    Auth { detail: String },
    Network { detail: String },
    Timeout { detail: String },
    Parse { detail: String },
    Other { detail: String },
}

impl std::fmt::Display for PostProcessModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, detail) = match self {
            PostProcessModelError::Auth { detail } => ("auth", detail),
            PostProcessModelError::Network { detail } => ("network", detail),
            PostProcessModelError::Timeout { detail } => ("timeout", detail),
            PostProcessModelError::Parse { detail } => ("parse", detail),
            PostProcessModelError::Other { detail } => ("other", detail),
        };
        write!(f, "{kind}: {detail}")
    }
}

impl PostProcessModelError {
    /// The shared failure class this error maps onto, if any maps. Parse
    /// failures of a model list or probe response are output-invalid (the
    /// endpoint produced something unusable); `Other` (an HTTP 500, an
    /// unknown provider id) has no class token and reports as None with
    /// its detail.
    pub fn failure_class(&self) -> Option<PostProcessFailureClass> {
        match self {
            PostProcessModelError::Auth { .. } => Some(PostProcessFailureClass::Auth),
            PostProcessModelError::Network { .. } => Some(PostProcessFailureClass::Network),
            PostProcessModelError::Timeout { .. } => Some(PostProcessFailureClass::Timeout),
            PostProcessModelError::Parse { .. } => Some(PostProcessFailureClass::OutputInvalid),
            PostProcessModelError::Other { .. } => None,
        }
    }
}

/// Structured failure for the completion path (the dictation pipeline's
/// chat request and its legacy retry), replacing the bare String errors
/// `send_chat_completion_with_schema` used to return. `class` is the same
/// vocabulary the pp: observability lifecycle and Test Connection print;
/// `detail` carries the sanitized diagnostics (never key material, never
/// decode payloads that could quote transcription content); `retries` is
/// how many bounded network retries the request consumed (0 on every
/// non-network class; a request is never retried after a cancellation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PostProcessError {
    pub class: PostProcessFailureClass,
    pub detail: String,
    pub retries: u32,
}

impl std::fmt::Display for PostProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}",
            crate::post_process_runs::failure_class_str(self.class),
            self.detail
        )
    }
}

impl PostProcessError {
    pub fn new(class: PostProcessFailureClass, detail: String) -> Self {
        Self {
            class,
            detail,
            retries: 0,
        }
    }

    /// The same error with `retries` stamped in (the retry wrapper's report
    /// to the pp: generation phase).
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }
}

/// How many bounded retries a network-class failure earns: 2 (so a flaky
/// connect or a 5xx gets three total attempts), then the run fails network
/// and the raw transcript is used.
pub const POST_PROCESS_NETWORK_RETRIES: u32 = 2;

/// The backoff schedule between network retries: 500 ms, then 1 s. Indexed
/// by retries already consumed.
pub const POST_PROCESS_RETRY_BACKOFF_MS: [u64; POST_PROCESS_NETWORK_RETRIES as usize] = [500, 1000];

/// The bounded retry policy, pure so it is pinnable without a network:
/// ONLY network-class failures retry (auth keys, timeouts, context-length
/// rejections, and invalid output are deterministic; retrying them wastes
/// the dictation's latency budget), and only twice, on the fixed backoff
/// schedule. Everything else, and every request past the budget, returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    Return,
    RetryAfterMillis(u64),
}

pub(crate) fn network_retry_decision(
    class: PostProcessFailureClass,
    retries_used: u32,
) -> RetryDecision {
    if class != PostProcessFailureClass::Network {
        return RetryDecision::Return;
    }
    match POST_PROCESS_RETRY_BACKOFF_MS
        .get(retries_used as usize)
        .copied()
    {
        Some(backoff_ms) => RetryDecision::RetryAfterMillis(backoff_ms),
        None => RetryDecision::Return,
    }
}

/// Classify a transport-phase reqwest failure from the completion path: a
/// request that outran its deadline is a timeout; a decode failure (the
/// endpoint answered something unparseable) is output-invalid; everything
/// else (connect refused, DNS, TLS, reset) is a network failure.
fn classify_completion_transport(context: &str, error: &reqwest::Error) -> PostProcessError {
    let detail = report_reqwest_error(context, error);
    let class = if error.is_timeout() {
        PostProcessFailureClass::Timeout
    } else if error.is_decode() {
        PostProcessFailureClass::OutputInvalid
    } else {
        PostProcessFailureClass::Network
    };
    PostProcessError::new(class, detail)
}

/// Does this error body text name a context-length rejection? Providers
/// word it differently ("maximum context length", "context window", "too
/// many tokens"); the markers are lowercase.
fn body_names_context_length(lower_body: &str) -> bool {
    lower_body.contains("context length")
        || lower_body.contains("context window")
        || lower_body.contains("maximum context")
        || lower_body.contains("too many tokens")
}

/// Classify a non-success HTTP status from the completion path: 401/403
/// are auth (bad or missing key), 413 and the context-length wordings of
/// 400/422 are context-length rejections, gateway timeouts (408/504) are
/// timeouts, and every other status is network class (the endpoint was
/// unreachable or misbehaved; a bounded retry is the right response).
fn classify_completion_status(status: reqwest::StatusCode, body: &str) -> PostProcessError {
    let detail = format!(
        "API request failed with status {}: {}",
        status,
        truncate_for_detail(body.trim(), 300)
    );
    let class = match status.as_u16() {
        401 | 403 => PostProcessFailureClass::Auth,
        413 => PostProcessFailureClass::ContextLength,
        400 | 422 if body_names_context_length(&body.to_lowercase()) => {
            PostProcessFailureClass::ContextLength
        }
        408 | 504 => PostProcessFailureClass::Timeout,
        _ => PostProcessFailureClass::Network,
    };
    PostProcessError::new(class, detail)
}

/// Keep error details bounded: an error body of arbitrary size must never
/// blow up the UI verdict row or the log line.
fn truncate_for_detail(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut cut = max_chars;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

/// Classify a non-success HTTP status from the model list or the probe.
/// 401/403 are auth failures (bad or missing key); everything else keeps
/// its status and body in an unclassed detail.
fn classify_status_error(
    context: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> PostProcessModelError {
    let detail = format!(
        "{} ({}): {}",
        context,
        status,
        truncate_for_detail(body.trim(), 300)
    );
    match status.as_u16() {
        401 | 403 => PostProcessModelError::Auth { detail },
        _ => PostProcessModelError::Other { detail },
    }
}

/// Classify a transport-phase reqwest failure: a request that outran its
/// deadline is a timeout; anything else (connect refused, DNS, TLS) is a
/// network failure. The detail comes from `report_reqwest_error`, which
/// sanitizes URLs and never echoes decode payloads.
fn classify_transport_error(context: &str, error: &reqwest::Error) -> PostProcessModelError {
    let detail = report_reqwest_error(context, error);
    if error.is_timeout() {
        PostProcessModelError::Timeout { detail }
    } else {
        PostProcessModelError::Network { detail }
    }
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct JsonSchema {
    name: String,
    strict: bool,
    schema: Value,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    json_schema: JsonSchema,
}

#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<bool>,
}

/// Request fields used to ask an endpoint to skip reasoning/thinking.
/// Providers disagree on the field name and accepted values, so at most one of
/// these is set per request (see `reasoning_disable_params`).
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
}

impl ReasoningParams {
    fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none() && self.reasoning.is_none() && self.thinking.is_none()
    }
}

/// Pick the reasoning-disable request fields an endpoint understands.
/// Unknown endpoints get the common OpenAI-style field; if they reject it,
/// the request is retried without it (see `send_chat_completion_with_schema`).
fn reasoning_disable_params(provider: &PostProcessProvider) -> ReasoningParams {
    let base_url = provider.base_url.to_lowercase();
    if base_url.contains("api.deepseek.com") {
        // DeepSeek rejects reasoning_effort "none" and uses its own field:
        // https://api-docs.deepseek.com/guides/thinking_mode
        ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
    } else if provider.id == "openrouter" {
        // OpenRouter nested object; exclude:true also keeps reasoning text out
        // of the response so it can't pollute structured-output JSON parsing
        ReasoningParams {
            reasoning: Some(ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
            ..Default::default()
        }
    } else {
        ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
    }
}

/// Endpoints (base_url|model) that rejected the reasoning-disable fields with a
/// 4xx. Remembered for the lifetime of the process so every dictation after the
/// first skips the doomed attempt and goes straight to a plain request.
fn reasoning_rejections() -> &'static Mutex<HashSet<String>> {
    static REJECTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REJECTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn endpoint_key(provider: &PostProcessProvider, model: &str) -> String {
    format!("{}|{}", provider.base_url.trim_end_matches('/'), model)
}

fn is_known_rejected(key: &str) -> bool {
    reasoning_rejections()
        .lock()
        .map(|set| set.contains(key))
        .unwrap_or(false)
}

fn remember_rejection(key: String) {
    if let Ok(mut set) = reasoning_rejections().lock() {
        set.insert(key);
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(flatten)]
    reasoning: ReasoningParams,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
}

/// Build headers for API requests based on provider type
fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    // Common headers
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://github.com/nexiouscaliver/voxbar"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("VoxBar/1.0 (+https://github.com/nexiouscaliver/voxbar)"),
    );
    headers.insert("X-Title", HeaderValue::from_static("VoxBar"));

    // Provider-specific auth headers
    if !api_key.is_empty() {
        if provider.id == "anthropic" {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(api_key)
                    .map_err(|e| format!("Invalid API key header value: {}", e))?,
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .map_err(|e| format!("Invalid authorization header value: {}", e))?,
            );
        }
    }

    Ok(headers)
}

/// Default total-request timeout for post-process calls, in seconds. Also
/// the fallback a 0 (hand-edited store) setting resolves to: 0 would mean
/// "no total timeout" for reqwest, which is exactly the forever-wedge this
/// bounds.
pub(crate) const DEFAULT_POST_PROCESS_TIMEOUT_SECS: u64 = 60;

/// Resolve the configured timeout (seconds) into the value the client uses.
/// 0 resolves to the default; everything else passes through unchanged (the
/// setting's command enforces its own bounds on writes).
pub(crate) fn resolve_request_timeout_secs(configured_secs: u64) -> u64 {
    if configured_secs == 0 {
        DEFAULT_POST_PROCESS_TIMEOUT_SECS
    } else {
        configured_secs
    }
}

/// Create an HTTP client with provider-specific headers and a bounded
/// request lifetime. Without a total timeout, an endpoint that accepts the
/// connection but never answers hangs the post-process pipeline forever:
/// during Processing the keyboard cancel path is inert (the handler gate
/// requires a live recording), so nothing but the tray Cancel escapes.
/// `timeout_secs` is the resolved post-process setting; the connect phase
/// is bounded separately so a dead host fails in seconds, not minutes.
fn create_client(
    provider: &PostProcessProvider,
    api_key: &str,
    timeout_secs: u64,
) -> Result<reqwest::Client, String> {
    let headers = build_headers(provider, api_key)?;
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(
            resolve_request_timeout_secs(timeout_secs),
        ))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| report_reqwest_error("Failed to build HTTP client", &e))
}

/// Format a bounded error source chain.
///
/// `reqwest::Error`'s Display implementation intentionally gives only a short
/// summary. Nested causes contain the useful transport details, such as a
/// certificate validation failure, an HTTP/2 error, or a connection reset.
/// Callers must skip source types whose Display text can quote payload data.
fn error_source_chain(error: &(dyn StdError + 'static)) -> Vec<String> {
    let mut causes = Vec::new();
    let mut source = error.source();

    // Defensive cap in case a third-party error exposes a cyclic source chain.
    for _ in 0..16 {
        let Some(cause) = source else {
            break;
        };
        causes.push(cause.to_string());
        source = cause.source();
    }

    causes
}

fn reqwest_error_kinds(error: &reqwest::Error) -> String {
    let mut kinds = Vec::new();

    if error.is_builder() {
        kinds.push("builder");
    }
    if error.is_connect() {
        kinds.push("connect");
    }
    if error.is_request() {
        kinds.push("request");
    }
    if error.is_redirect() {
        kinds.push("redirect");
    }
    if error.is_timeout() {
        kinds.push("timeout");
    }
    if error.is_status() {
        kinds.push("status");
    }
    if error.is_body() {
        kinds.push("body");
    }
    if error.is_decode() {
        kinds.push("decode");
    }
    if error.is_upgrade() {
        kinds.push("upgrade");
    }

    if kinds.is_empty() {
        "unknown".to_string()
    } else {
        kinds.join(", ")
    }
}

fn sanitized_url(url: &reqwest::Url) -> String {
    let mut url = url.clone();

    // Custom endpoints should not contain credentials or query-string tokens,
    // but omit them from diagnostics in case one does.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);

    url.to_string()
}

fn sanitized_url_for_log(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|url| sanitized_url(&url))
        // Do not echo an invalid URL: the parse failure might have been caused
        // by sensitive data entered in the custom endpoint field.
        .unwrap_or_else(|_| "<invalid URL>".to_string())
}

fn report_reqwest_error(context: &str, error: &reqwest::Error) -> String {
    let kinds = reqwest_error_kinds(error);
    let url = error
        .url()
        .map(sanitized_url)
        .map(|url| format!(", url: {url}"))
        .unwrap_or_default();

    // serde_json's error text can quote values from a malformed response. That
    // response may contain transcription content, so retain the useful decode
    // classification but never put its nested source in logs or UI errors.
    let causes = if error.is_decode() {
        Vec::new()
    } else {
        error_source_chain(error)
    };
    let cause_details = if !causes.is_empty() {
        format!(": caused by: {}", causes.join(" -> "))
    } else if error.url().is_none() {
        // Reqwest's short Display text is safe when it cannot append a raw URL.
        format!(": {error}")
    } else {
        // The sanitized URL is already included above. Avoid formatting the
        // original error because its Display implementation includes the raw URL.
        String::new()
    };

    let details = format!("{context} (kind: {kinds}{url}){cause_details}");
    error!("{details}");
    details
}

/// One completed request cycle through the bounded retry wrapper: the
/// endpoint's answer (None = a 200 with no content, an output-invalid
/// failure the caller classifies) and how many network-class transport
/// retries it took. The retry count rides here so the pp: generation
/// phase can report it on SUCCESS too, not only on the final failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostProcessCompletion {
    pub content: Option<String>,
    pub transport_retries: u32,
}

/// Send a chat completion request to an OpenAI-compatible API with the
/// bounded network-only retry policy: a network-class failure (connect
/// refused, DNS, TLS, a 5xx) is retried twice on the 500 ms / 1 s backoff
/// schedule; every other class (auth, timeout, context_length,
/// output_invalid) and every cancelled request returns at once. The
/// cancellation flag is polled before each attempt and after each backoff,
/// so a request that outlived the dictation is never retried and never
/// started. `retries` on the error (or on [`PostProcessCompletion`]) is
/// the count the pp: generation phase reports.
pub async fn send_chat_completion_with_schema(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    user_content: String,
    system_prompt: Option<String>,
    json_schema: Option<Value>,
    disable_reasoning: bool,
    timeout_secs: u64,
    is_cancelled: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Result<PostProcessCompletion, PostProcessError> {
    let mut retries: u32 = 0;
    loop {
        // A cancelled dictation never sends (or re-sends) a request.
        if let Some(is_cancelled) = is_cancelled {
            if is_cancelled() {
                return Err(PostProcessError::new(
                    PostProcessFailureClass::Cancelled,
                    "the dictation was cancelled before the request ran".to_string(),
                ));
            }
        }
        match send_chat_completion_once(
            provider,
            api_key.clone(),
            model,
            user_content.clone(),
            system_prompt.clone(),
            json_schema.clone(),
            disable_reasoning,
            timeout_secs,
        )
        .await
        {
            Ok(content) => {
                return Ok(PostProcessCompletion {
                    content,
                    transport_retries: retries,
                })
            }
            Err(error) => {
                let decision = network_retry_decision(error.class, retries);
                let backoff_ms = match decision {
                    RetryDecision::Return => return Err(error.with_retries(retries)),
                    RetryDecision::RetryAfterMillis(backoff_ms) => backoff_ms,
                };
                // Cancellation wins over the retry: no new request, no
                // lingering backoff, the run reports cancelled.
                if let Some(is_cancelled) = is_cancelled {
                    if is_cancelled() {
                        return Err(PostProcessError::new(
                            PostProcessFailureClass::Cancelled,
                            "the dictation was cancelled before the retry ran".to_string(),
                        )
                        .with_retries(retries));
                    }
                }
                debug!(
                    "post-process request failed ({}); network retry {}/{} after {}ms",
                    error,
                    retries + 1,
                    POST_PROCESS_NETWORK_RETRIES,
                    backoff_ms
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                retries += 1;
            }
        }
    }
}

/// ONE chat completion request, no transport retry. Structured output
/// support: when json_schema is provided, uses structured outputs mode.
/// system_prompt is used as the system message when provided.
///
/// When disable_reasoning is set, the request carries the reasoning-disable
/// fields the endpoint is expected to understand. Not every OpenAI-compatible
/// endpoint accepts them (DeepSeek, Gemini's compat layer, and some OpenRouter
/// upstreams reject with 400), so a 400/422 answer to such a request triggers
/// one retry without the fields, and the rejection is remembered per
/// (base_url, model) so later requests skip the failing attempt entirely.
/// That prompt-shape retry is orthogonal to the network retry wrapper above.
async fn send_chat_completion_once(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    user_content: String,
    system_prompt: Option<String>,
    json_schema: Option<Value>,
    disable_reasoning: bool,
    timeout_secs: u64,
) -> Result<Option<String>, PostProcessError> {
    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/chat/completions", base_url);

    debug!(
        "Sending chat completion request to: {}",
        sanitized_url_for_log(&url)
    );

    let client = create_client(provider, &api_key, timeout_secs)
        .map_err(|detail| PostProcessError::new(PostProcessFailureClass::Network, detail))?;

    // Build messages vector
    let mut messages = Vec::new();

    // Add system prompt if provided
    if let Some(system) = system_prompt {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system,
        });
    }

    // Add user message
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: user_content,
    });

    // Build response_format if schema is provided
    let response_format = json_schema.map(|schema| ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: JsonSchema {
            name: "transcription_output".to_string(),
            strict: true,
            schema,
        },
    });

    let key = endpoint_key(provider, model);
    let reasoning = if disable_reasoning && !is_known_rejected(&key) {
        reasoning_disable_params(provider)
    } else {
        ReasoningParams::default()
    };

    let mut request_body = ChatCompletionRequest {
        model: model.to_string(),
        messages,
        stream: false,
        response_format,
        reasoning,
    };

    let mut response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| classify_completion_transport("HTTP request failed", &e))?;
    let mut status = response.status();
    debug!(
        "Chat completion response received with status {} over {:?} from {}",
        status,
        response.version(),
        sanitized_url(response.url())
    );

    // A 400/422 on a request carrying reasoning-disable fields is almost always
    // the endpoint rejecting those fields - retry once without them.
    if !status.is_success()
        && matches!(status.as_u16(), 400 | 422)
        && !request_body.reasoning.is_empty()
    {
        let error_text = response.text().await.unwrap_or_else(|e| {
            report_reqwest_error("Failed to read reasoning rejection response", &e)
        });
        info!(
            "Endpoint rejected request with reasoning disabled (status {}): {}. Retrying without reasoning fields",
            status, error_text
        );

        request_body.reasoning = ReasoningParams::default();
        response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| classify_completion_transport("HTTP retry failed", &e))?;
        status = response.status();
        debug!(
            "Chat completion retry response received with status {} over {:?} from {}",
            status,
            response.version(),
            sanitized_url(response.url())
        );

        if status.is_success() {
            info!(
                "Retry without reasoning fields succeeded; '{}' (model '{}') will skip them from now on",
                sanitized_url_for_log(base_url), model
            );
            remember_rejection(key);
        }
    }

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|e| report_reqwest_error("Failed to read API error response", &e));
        return Err(classify_completion_status(status, &error_text));
    }

    let completion: ChatCompletionResponse = response.json().await.map_err(|e| {
        // report_reqwest_error drops nested causes for decode errors (they
        // can quote response values, which can quote transcription
        // content), so the detail is safe to surface.
        PostProcessError::new(
            PostProcessFailureClass::OutputInvalid,
            report_reqwest_error("Failed to parse API response", &e),
        )
    })?;

    Ok(completion
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone()))
}

/// Resolve the model-list path for a provider: its declared
/// `models_endpoint` (leading '/' trimmed) when set, else the
/// OpenAI-style `models` default. On-device providers (local, Apple
/// Intelligence) declare None and never reach this.
fn models_list_path(provider: &PostProcessProvider) -> String {
    provider
        .models_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| path.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "models".to_string())
}

/// Fetch available models from an provider's declared models endpoint.
/// Returns a list of model IDs; failures come back classified (auth,
/// network, timeout, parse) with sanitized detail instead of a raw string.
pub async fn fetch_models(
    provider: &PostProcessProvider,
    api_key: String,
    timeout_secs: u64,
) -> Result<Vec<String>, PostProcessModelError> {
    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/{}", base_url, models_list_path(provider));

    debug!("Fetching models from: {}", sanitized_url_for_log(&url));

    let client = create_client(provider, &api_key, timeout_secs)
        .map_err(|detail| PostProcessModelError::Other { detail })?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| classify_transport_error("Failed to fetch models", &e))?;

    let status = response.status();
    debug!(
        "Model list response received with status {} over {:?} from {}",
        status,
        response.version(),
        sanitized_url(response.url())
    );
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_else(|e| {
            // Reading the error body failed; classify from the transport
            // error and keep the status in the detail.
            let detail = report_reqwest_error("Failed to read model list error", &e);
            format!("<unreadable body: {detail}>")
        });
        return Err(classify_status_error(
            "Model list request failed",
            status,
            &error_text,
        ));
    }

    let parsed: serde_json::Value = response.json().await.map_err(|e| {
        // report_reqwest_error drops nested causes for decode errors (they
        // can quote response values), so this detail is safe to surface.
        PostProcessModelError::Parse {
            detail: report_reqwest_error("Failed to parse model list response", &e),
        }
    })?;

    let mut models = Vec::new();

    // Handle OpenAI format: { data: [ { id: "..." }, ... ] }. Anthropic's
    // /v1/models uses the same { data: [ { id, display_name, ... } ] }
    // shape, so this arm covers it too.
    if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                models.push(name.to_string());
            }
        }
    }
    // Handle array format: [ "model1", "model2", ... ]
    else if let Some(array) = parsed.as_array() {
        for entry in array {
            if let Some(model) = entry.as_str() {
                models.push(model.to_string());
            }
        }
    }

    Ok(models)
}

/// Hard total-request timeout for the Test Connection completion probe,
/// deliberately independent of `post_process_timeout_secs`: the verdict
/// must arrive in bounded time even when the operator has configured a
/// long transcription budget.
pub const CONNECTION_PROBE_TIMEOUT_SECS: u64 = 10;

/// The probe's full instruction: a one-word reply proves the model answers.
const CONNECTION_PROBE_PROMPT: &str = "Reply with the single word OK";

/// The Test Connection verdict, assembled by the command from the model
/// list, the completion probe, or the on-device state. `completion_ok` is
/// None when no probe ran (local/Apple providers, or no model selected);
/// `latency_ms` is the round-trip of the model-list request.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct TestConnectionResult {
    pub model_list_ok: bool,
    pub completion_ok: Option<bool>,
    pub latency_ms: Option<u64>,
    pub failure_class: Option<PostProcessFailureClass>,
    pub detail: String,
}

/// Send the tiny completion used by Test Connection: one user message,
/// `max_tokens: 5`, on its own client with its own (hard) timeout. Shares
/// the post-process retry policy (bounded, network-class only) with the
/// dictation path so a flaky connect does not fail the verdict. Returns
/// the message content, or None when the endpoint answers 200 with no
/// content (the caller reads that as an invalid output).
pub async fn probe_completion(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    timeout_secs: u64,
) -> Result<Option<String>, PostProcessModelError> {
    let mut retries: u32 = 0;
    loop {
        match probe_completion_once(provider, api_key.clone(), model, timeout_secs).await {
            Ok(content) => return Ok(content),
            Err(error) => {
                // Other-class failures (a 5xx and friends) retry like the
                // completion path's else-network rule; auth, timeout, and
                // parse failures are deterministic and return at once.
                let retry_class = error
                    .failure_class()
                    .unwrap_or(PostProcessFailureClass::Network);
                let backoff_ms = match network_retry_decision(retry_class, retries) {
                    RetryDecision::Return => return Err(error),
                    RetryDecision::RetryAfterMillis(backoff_ms) => backoff_ms,
                };
                debug!(
                    "completion probe failed ({}); retry {}/{} after {}ms",
                    error,
                    retries + 1,
                    POST_PROCESS_NETWORK_RETRIES,
                    backoff_ms
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                retries += 1;
            }
        }
    }
}

/// ONE probe request, no retry (the wrapper above owns the policy).
async fn probe_completion_once(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    timeout_secs: u64,
) -> Result<Option<String>, PostProcessModelError> {
    let base_url = provider.base_url.trim_end_matches('/');
    let url = format!("{}/chat/completions", base_url);

    let headers = build_headers(provider, &api_key)
        .map_err(|detail| PostProcessModelError::Other { detail })?;
    // The probe builds its own client so its timeout stays independent of
    // the configured post-process timeout (and of any client the pipeline
    // holds on to).
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| PostProcessModelError::Other {
            detail: report_reqwest_error("Failed to build probe client", &e),
        })?;

    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": CONNECTION_PROBE_PROMPT }
        ],
        "stream": false,
        "max_tokens": 5,
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| classify_transport_error("Completion probe failed", &e))?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(classify_status_error(
            "Completion probe failed",
            status,
            &error_text,
        ));
    }

    let completion: ChatCompletionResponse =
        response
            .json()
            .await
            .map_err(|e| PostProcessModelError::Parse {
                detail: report_reqwest_error("Failed to parse completion probe response", &e),
            })?;

    Ok(completion
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone()))
}

/// Assemble the cloud verdict from the two probes. Pure so the
/// classification is pinnable without HTTP: a list failure short-circuits
/// (no completion verdict, no made-up latency), and an answer with no
/// content is an output-invalid failure, not a success.
pub(crate) fn assemble_cloud_verdict(
    list_outcome: Result<(usize, u64), PostProcessModelError>,
    probe_outcome: Option<Result<Option<String>, PostProcessModelError>>,
) -> TestConnectionResult {
    let (model_count, latency_ms) = match list_outcome {
        Err(error) => {
            return TestConnectionResult {
                model_list_ok: false,
                completion_ok: None,
                latency_ms: None,
                failure_class: error.failure_class(),
                detail: error.to_string(),
            };
        }
        Ok(ok) => ok,
    };

    match probe_outcome {
        None => TestConnectionResult {
            model_list_ok: true,
            completion_ok: None,
            latency_ms: Some(latency_ms),
            failure_class: None,
            detail: format!("model list ok ({model_count} models); no model selected to probe"),
        },
        Some(Err(error)) => TestConnectionResult {
            model_list_ok: true,
            completion_ok: Some(false),
            latency_ms: Some(latency_ms),
            failure_class: error.failure_class(),
            detail: error.to_string(),
        },
        Some(Ok(content)) => {
            let answered = content
                .as_deref()
                .map(str::trim)
                .is_some_and(|c| !c.is_empty());
            if answered {
                TestConnectionResult {
                    model_list_ok: true,
                    completion_ok: Some(true),
                    latency_ms: Some(latency_ms),
                    failure_class: None,
                    detail: String::new(),
                }
            } else {
                TestConnectionResult {
                    model_list_ok: true,
                    completion_ok: Some(false),
                    latency_ms: Some(latency_ms),
                    failure_class: Some(PostProcessFailureClass::OutputInvalid),
                    detail: "completion probe returned no content".to_string(),
                }
            }
        }
    }
}

/// The local provider's verdict, from the selected model's downloaded
/// state alone (no worker spawn, no load): the engine is usable exactly
/// when the selected model is on disk.
pub(crate) fn local_provider_connection_verdict(
    downloaded: bool,
    downloading: bool,
) -> TestConnectionResult {
    let detail = if downloaded {
        String::new()
    } else if downloading {
        "the selected local model is still downloading".to_string()
    } else {
        "the selected local model is not downloaded".to_string()
    };
    TestConnectionResult {
        model_list_ok: true,
        completion_ok: Some(downloaded),
        latency_ms: None,
        failure_class: None,
        detail,
    }
}

/// The Apple Intelligence verdict: its availability check is the whole
/// story; there is no list endpoint and no probe to run.
pub(crate) fn apple_intelligence_connection_verdict(available: bool) -> TestConnectionResult {
    TestConnectionResult {
        model_list_ok: available,
        completion_ok: None,
        latency_ms: None,
        failure_class: None,
        detail: if available {
            String::new()
        } else {
            "Apple Intelligence is not available on this device".to_string()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug)]
    struct TestError {
        message: &'static str,
        source: Option<Box<TestError>>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for TestError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn StdError + 'static))
        }
    }

    fn provider(id: &str, base_url: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: id.to_string(),
            label: id.to_string(),
            base_url: base_url.to_string(),
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: false,
            default_timeout_secs: crate::settings::PROVIDER_CLASS_DEFAULT_TIMEOUT_SECS,
        }
    }

    fn provider_with_endpoint(
        id: &str,
        base_url: &str,
        models_endpoint: Option<&str>,
    ) -> PostProcessProvider {
        PostProcessProvider {
            models_endpoint: models_endpoint.map(str::to_string),
            ..provider(id, base_url)
        }
    }

    /// Like `serve_one_response`, but hands the raw request bytes back so a
    /// test can assert on the request line and headers, not just the reply.
    async fn serve_one_response_with_request(
        status: &str,
        body: &str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&request[..read]).to_string());
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{address}"), rx)
    }

    /// A server that answers every connection with 401 and counts how many
    /// connections it served: the "no retry" assertion for auth failures
    /// needs to see exactly one.
    async fn serve_counting_401() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = count.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = [0_u8; 8192];
                let _ = stream.read(&mut request).await;
                let response =
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), count)
    }

    fn request_json(reasoning: ReasoningParams) -> Value {
        let request = ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: false,
            response_format: None,
            reasoning,
        };
        serde_json::to_value(&request).unwrap()
    }

    async fn serve_one_response(status: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        format!("http://{address}")
    }

    #[test]
    fn error_source_chain_includes_all_nested_causes() {
        let error = TestError {
            message: "request failed",
            source: Some(Box::new(TestError {
                message: "TLS handshake failed",
                source: Some(Box::new(TestError {
                    message: "unknown certificate authority",
                    source: None,
                })),
            })),
        };

        assert_eq!(
            error_source_chain(&error),
            vec!["TLS handshake failed", "unknown certificate authority"]
        );
    }

    #[test]
    fn log_url_sanitization_removes_credentials_and_tokens() {
        let url = "https://user:password@example.com/v1/models?api_key=secret#private";
        assert_eq!(sanitized_url_for_log(url), "https://example.com/v1/models");
    }

    #[test]
    fn invalid_log_urls_are_not_echoed() {
        assert_eq!(
            sanitized_url_for_log("not a URL containing secret"),
            "<invalid URL>"
        );
    }

    #[tokio::test]
    async fn decode_error_does_not_echo_response_values() {
        let base_url =
            serve_one_response("200 OK", r#"{"choices":"PRIVATE TRANSCRIPTION CONTENT"}"#).await;
        let error = reqwest::get(base_url)
            .await
            .unwrap()
            .json::<ChatCompletionResponse>()
            .await
            .unwrap_err();

        let details = report_reqwest_error("Failed to parse API response", &error);
        assert!(details.contains("kind: decode"));
        assert!(!details.contains("PRIVATE TRANSCRIPTION CONTENT"));
    }

    #[tokio::test]
    async fn raw_error_url_is_not_reintroduced_without_a_source() {
        let base_url = serve_one_response("400 Bad Request", "bad request").await;
        let error = reqwest::get(format!(
            "{base_url}/private?api_key=SECRET_QUERY_TOKEN#private"
        ))
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();

        let details = report_reqwest_error("Request failed", &error);
        assert!(details.contains(&format!("url: {base_url}/private")));
        assert!(!details.contains("SECRET_QUERY_TOKEN"));
        assert!(!details.contains("#private"));
    }

    #[test]
    fn requests_explicitly_disable_streaming() {
        let json = request_json(ReasoningParams::default());
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn default_reasoning_params_serialize_to_no_fields() {
        let json = request_json(ReasoningParams::default());
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn custom_provider_uses_top_level_reasoning_effort() {
        let params = reasoning_disable_params(&provider("custom", "http://localhost:11434/v1"));
        let json = request_json(params);
        assert_eq!(json["reasoning_effort"], "none");
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn openrouter_uses_nested_reasoning_object() {
        let params =
            reasoning_disable_params(&provider("openrouter", "https://openrouter.ai/api/v1"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert_eq!(json["reasoning"]["effort"], "none");
        assert_eq!(json["reasoning"]["exclude"], true);
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn deepseek_base_url_uses_thinking_disabled() {
        let params = reasoning_disable_params(&provider("custom", "https://api.deepseek.com"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert_eq!(json["thinking"]["type"], "disabled");
    }

    #[test]
    fn reasoning_params_is_empty_tracks_all_fields() {
        assert!(ReasoningParams::default().is_empty());
        assert!(!ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn outbound_headers_identify_the_app_as_voxbar() {
        // Identity headers are sent to user-configured LLM providers
        // (OpenRouter et al.) to attribute traffic; they must carry the
        // product's own name and repo, not the fork's upstream.
        let headers = build_headers(&provider("custom", "http://localhost:11434/v1"), "").unwrap();
        assert_eq!(headers.get("x-title").unwrap(), "VoxBar");
        assert_eq!(
            headers.get(REFERER).unwrap(),
            "https://github.com/nexiouscaliver/voxbar"
        );
        assert_eq!(
            headers.get(USER_AGENT).unwrap(),
            "VoxBar/1.0 (+https://github.com/nexiouscaliver/voxbar)"
        );
        assert!(!headers
            .get(USER_AGENT)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Handy"));
    }

    #[test]
    fn rejection_memo_is_keyed_by_base_url_and_model() {
        let deepseek = provider("custom", "https://api.deepseek.com/");
        let key = endpoint_key(&deepseek, "deepseek-chat");
        assert_eq!(key, "https://api.deepseek.com|deepseek-chat");
        assert!(!is_known_rejected(&key));
        remember_rejection(key.clone());
        assert!(is_known_rejected(&key));
        // A different model on the same endpoint is tracked separately
        assert!(!is_known_rejected(&endpoint_key(&deepseek, "other-model")));
    }

    #[test]
    fn timeout_resolution_never_disables_the_timeout() {
        // A 0 setting (hand-edited store) resolves to the default instead of
        // "no total timeout", which would reintroduce the forever-wedge.
        assert_eq!(
            resolve_request_timeout_secs(0),
            DEFAULT_POST_PROCESS_TIMEOUT_SECS
        );
        assert_eq!(resolve_request_timeout_secs(0), 60);
        // Ordinary values pass through unchanged.
        assert_eq!(resolve_request_timeout_secs(1), 1);
        assert_eq!(resolve_request_timeout_secs(30), 30);
        assert_eq!(resolve_request_timeout_secs(600), 600);
    }

    /// The bounded retry policy, pure: network failures retry exactly twice
    /// on the fixed 500 ms / 1 s schedule; every other class returns at
    /// once at any retry count; network past the budget returns.
    #[test]
    fn network_retry_policy_is_bounded_and_network_only() {
        use PostProcessFailureClass as Class;
        use RetryDecision::*;

        for class in [
            Class::Auth,
            Class::Timeout,
            Class::ContextLength,
            Class::OutputInvalid,
            Class::Oom,
            Class::Cancelled,
        ] {
            assert_eq!(
                network_retry_decision(class, 0),
                Return,
                "{class:?} must never retry"
            );
            assert_eq!(
                network_retry_decision(class, 1),
                Return,
                "{class:?} must never retry, even mid-budget"
            );
        }
        assert_eq!(
            network_retry_decision(Class::Network, 0),
            RetryAfterMillis(500)
        );
        assert_eq!(
            network_retry_decision(Class::Network, 1),
            RetryAfterMillis(1000)
        );
        assert_eq!(network_retry_decision(Class::Network, 2), Return);
        assert_eq!(network_retry_decision(Class::Network, 99), Return);
    }

    /// An address whose port is closed: every connection attempt is refused
    /// (reqwest reports is_connect), the canonical network-class failure.
    async fn refused_base_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{address}")
    }

    /// A connect error is network class: the wrapper retries exactly twice
    /// (the backoff schedule is observable as wall clock) and then fails
    /// with the retry count stamped on the error.
    #[tokio::test]
    async fn connect_error_retries_twice_then_fails_network() {
        let base_url = refused_base_url().await;
        let started = std::time::Instant::now();
        let error = send_chat_completion_with_schema(
            &provider("custom", &base_url),
            String::new(),
            "test-model",
            "hi".to_string(),
            None,
            None,
            false,
            5,
            None,
        )
        .await
        .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(
            error.class,
            PostProcessFailureClass::Network,
            "a refused connect is network class: {error}"
        );
        assert_eq!(
            error.retries, POST_PROCESS_NETWORK_RETRIES,
            "exactly two retries before giving up: {error}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(1500),
            "the 500ms + 1s backoff must run between attempts (took {elapsed:?})"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the retries stay bounded (took {elapsed:?})"
        );
    }

    /// A 401 from the completion endpoint is auth class and is NEVER
    /// retried: the endpoint served exactly one connection.
    #[tokio::test]
    async fn auth_401_completion_is_not_retried() {
        let (base_url, served) = serve_counting_401().await;
        let error = send_chat_completion_with_schema(
            &provider("custom", &base_url),
            "sk-bad".to_string(),
            "test-model",
            "hi".to_string(),
            None,
            None,
            false,
            5,
            None,
        )
        .await
        .unwrap_err();

        assert_eq!(error.class, PostProcessFailureClass::Auth, "{error}");
        assert_eq!(error.retries, 0, "an auth failure never retries: {error}");
        assert!(error.to_string().starts_with("auth: "), "{error}");
        assert!(error.detail.contains("401"), "{error}");

        // Give a would-be retry a beat to land, then assert it never came.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let connections = served.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            connections, 1,
            "an auth failure must not be retried, but {connections} connections were made"
        );
    }

    /// A cancelled dictation never retries: the cancel flag checked after a
    /// failed attempt stops the wrapper before the backoff (and any second
    /// request), reporting the cancelled class with zero retries.
    #[tokio::test]
    async fn cancelled_requests_are_never_retried() {
        let base_url = refused_base_url().await;
        let checks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flag = std::sync::Arc::clone(&checks);
        // Cancel fires on the SECOND check (after the failed attempt,
        // before the retry decision sleeps).
        let is_cancelled = move || flag.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1;

        let started = std::time::Instant::now();
        let error = send_chat_completion_with_schema(
            &provider("custom", &base_url),
            String::new(),
            "test-model",
            "hi".to_string(),
            None,
            None,
            false,
            5,
            Some(&is_cancelled),
        )
        .await
        .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(
            error.class,
            PostProcessFailureClass::Cancelled,
            "a cancelled run reports the cancelled class: {error}"
        );
        assert_eq!(error.retries, 0, "no retry may follow a cancellation");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the wrapper must stop before the first backoff (took {elapsed:?})"
        );

        // Already-cancelled before the first attempt: no request at all.
        let always_cancelled = || true;
        let error = send_chat_completion_with_schema(
            &provider("custom", &base_url),
            String::new(),
            "test-model",
            "hi".to_string(),
            None,
            None,
            false,
            5,
            Some(&always_cancelled),
        )
        .await
        .unwrap_err();
        assert_eq!(error.class, PostProcessFailureClass::Cancelled, "{error}");
        assert_eq!(error.retries, 0, "{error}");
    }

    /// The completion status classifier, pure over status + body: 401/403
    /// auth; 413 and the context-length wordings of 400/422
    /// context_length; 408/504 timeout; everything else (500, a plain 400)
    /// network, which is what earns the bounded retry.
    #[test]
    fn completion_status_classification_table() {
        use PostProcessFailureClass as Class;
        let cases: [(u16, &str, Class); 8] = [
            (401, r#"{"error":"bad key"}"#, Class::Auth),
            (403, "forbidden", Class::Auth),
            (413, "payload too large", Class::ContextLength),
            (
                400,
                "This model's maximum context length is 4096 tokens",
                Class::ContextLength,
            ),
            (
                422,
                "request exceeds the context window",
                Class::ContextLength,
            ),
            (408, "request timeout", Class::Timeout),
            (504, "gateway timeout", Class::Timeout),
            (500, "internal error", Class::Network),
        ];
        for (status, body, class) in cases {
            let error =
                classify_completion_status(reqwest::StatusCode::from_u16(status).unwrap(), body);
            assert_eq!(error.class, class, "status {status} body {body}");
            assert!(
                error.detail.contains(&status.to_string()),
                "the status rides the detail: {error}"
            );
        }
        // A plain 400 (no context wording) is network class: it may retry,
        // bounded, like a 5xx.
        let plain_400 = classify_completion_status(
            reqwest::StatusCode::from_u16(400).unwrap(),
            "invalid request",
        );
        assert_eq!(plain_400.class, Class::Network);
        // Error bodies are truncated so a huge provider error cannot blow
        // up the log line or the overlay notice.
        let huge = classify_completion_status(
            reqwest::StatusCode::from_u16(500).unwrap(),
            &"x".repeat(10_000),
        );
        assert!(
            huge.detail.len() < 500,
            "detail bounded: {}",
            huge.detail.len()
        );
    }

    /// A 200 whose body is not a chat completion is output_invalid (the
    /// endpoint answered something unusable), never retried, and the
    /// detail never quotes the malformed payload.
    #[tokio::test]
    async fn unparseable_completion_is_output_invalid_without_retry() {
        let base_url = serve_one_response("200 OK", r#"{"choices":"PRIVATE CONTENT"}"#).await;
        let error = send_chat_completion_with_schema(
            &provider("custom", &base_url),
            String::new(),
            "test-model",
            "hi".to_string(),
            None,
            None,
            false,
            5,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.class,
            PostProcessFailureClass::OutputInvalid,
            "{error}"
        );
        assert_eq!(error.retries, 0, "a parse failure never retries: {error}");
        assert!(!error.detail.contains("PRIVATE CONTENT"), "{error}");
    }

    /// A listener that accepts the connection but never writes a response:
    /// the exact wedge shape a stalled endpoint produces. The accepted
    /// streams are HELD (not dropped) so the connection stays open and the
    /// client waits on a read that never arrives.
    async fn serve_never_responding() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        format!("http://{address}")
    }

    /// The wedge fix: against an endpoint that accepts connections but never
    /// responds, the request must fail with a timeout error (the pipeline
    /// returns to Idle) rather than hang forever. The 1s configured timeout
    /// keeps the test fast; the assertion is bounded so a regression fails
    /// instead of stalling the suite. A timeout is deterministic, so it is
    /// never retried (retries == 0) even under the network retry policy.
    #[tokio::test]
    async fn wedged_endpoint_fails_at_the_configured_timeout() {
        let base_url = serve_never_responding().await;
        let started = std::time::Instant::now();
        let result = send_chat_completion_with_schema(
            &provider("custom", &base_url),
            String::new(),
            "test-model",
            "hi".to_string(),
            None,
            None,
            false,
            1,
            None,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(result.is_err(), "the wedged request must fail, not hang");
        let error = result.unwrap_err();
        assert_eq!(
            error.class,
            PostProcessFailureClass::Timeout,
            "the failure should be the timeout, got: {error}"
        );
        assert_eq!(error.retries, 0, "a timeout must never be retried: {error}");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the timeout must bound the request (took {elapsed:?})"
        );
    }

    /// The declared models_endpoint is the path actually fetched (the field
    /// used to be declared per provider but never read).
    #[tokio::test]
    async fn models_endpoint_is_honored_in_the_fetch_path() {
        let (base_url, request_rx) =
            serve_one_response_with_request("200 OK", r#"{"data":[{"id":"m1"}]}"#).await;
        let provider = provider_with_endpoint("custom", &base_url, Some("/api/models"));

        let models = fetch_models(&provider, String::new(), 5).await.unwrap();
        assert_eq!(models, vec!["m1".to_string()]);

        let request = request_rx.await.unwrap();
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("GET /api/models"),
            "the declared endpoint path must be fetched, got: {request_line}"
        );
    }

    /// A provider without a models_endpoint (or with a blank one) keeps the
    /// historical OpenAI-style /models path.
    #[tokio::test]
    async fn missing_models_endpoint_falls_back_to_the_models_path() {
        for endpoint in [None, Some(""), Some("   "), Some("/models")] {
            let (base_url, request_rx) =
                serve_one_response_with_request("200 OK", r#"{"data":[]}"#).await;
            let provider = provider_with_endpoint("custom", &base_url, endpoint);

            fetch_models(&provider, String::new(), 5).await.unwrap();

            let request = request_rx.await.unwrap();
            let request_line = request.lines().next().unwrap_or_default();
            assert!(
                request_line.contains("GET /models"),
                "endpoint {:?} must resolve to /models, got: {request_line}",
                endpoint
            );
        }
    }

    /// Both wire shapes parse: the OpenAI {data:[{id}]} object (which
    /// Anthropic shares) and the bare ["model", ...] array.
    #[tokio::test]
    async fn model_list_accepts_data_object_and_bare_array_shapes() {
        let openai = provider(
            "custom",
            &serve_one_response(
                "200 OK",
                r#"{"data":[{"id":"gpt-4o-mini"},{"name":"legacy-name"}]}"#,
            )
            .await,
        );
        let models = fetch_models(&openai, String::new(), 5).await.unwrap();
        assert_eq!(
            models,
            vec!["gpt-4o-mini".to_string(), "legacy-name".to_string()]
        );

        let array = provider(
            "custom",
            &serve_one_response("200 OK", r#"["llama3.1:8b","qwen2.5:7b"]"#).await,
        );
        let models = fetch_models(&array, String::new(), 5).await.unwrap();
        assert_eq!(
            models,
            vec!["llama3.1:8b".to_string(), "qwen2.5:7b".to_string()]
        );
    }

    /// A 401 from the models endpoint is an auth-class failure with the
    /// status in the detail, not an unclassified raw string.
    #[tokio::test]
    async fn unauthorized_model_list_is_an_auth_class_failure() {
        let base_url = serve_one_response(
            "401 Unauthorized",
            r#"{"error":{"message":"Incorrect API key provided"}}"#,
        )
        .await;
        let provider = provider_with_endpoint("openai", &base_url, Some("/models"));

        let error = fetch_models(&provider, "sk-bad".to_string(), 5)
            .await
            .unwrap_err();
        assert!(
            matches!(error, PostProcessModelError::Auth { .. }),
            "a 401 must classify as auth, got: {error}"
        );
        assert_eq!(
            error.failure_class(),
            Some(PostProcessFailureClass::Auth),
            "the shared class for the verdict line is auth"
        );
        assert!(
            error.to_string().starts_with("auth: "),
            "the display form leads with the class, got: {error}"
        );
        assert!(error.to_string().contains("401"));
    }

    /// Anthropic's /v1/models body ({data:[{id, display_name, created_at}],
    /// has_more, ...}) parses through the OpenAI-shaped arm, and the request
    /// authenticates with x-api-key + anthropic-version (not a Bearer
    /// header), per build_headers.
    #[tokio::test]
    async fn anthropic_model_list_parses_with_anthropic_auth_headers() {
        let (base_url, request_rx) = serve_one_response_with_request(
            "200 OK",
            r#"{"data":[{"id":"claude-3-5-haiku-20241022","display_name":"Claude 3.5 Haiku","created_at":"2025-01-01T00:00:00Z"}],"first_id":"claude-3-5-haiku-20241022","has_more":false,"last_id":"claude-3-5-haiku-20241022"}"#,
        )
        .await;
        let provider = provider_with_endpoint("anthropic", &base_url, Some("/models"));

        let models = fetch_models(&provider, "sk-ant-probe".to_string(), 5)
            .await
            .unwrap();
        assert_eq!(
            models,
            vec!["claude-3-5-haiku-20241022".to_string()],
            "the Anthropic list shape must parse to model ids"
        );

        let request = request_rx.await.unwrap();
        let lowercase = request.to_lowercase();
        assert!(
            lowercase.contains("x-api-key: sk-ant-probe"),
            "Anthropic authenticates with x-api-key, got headers: {request}"
        );
        assert!(
            request.contains("anthropic-version: 2023-06-01"),
            "the anthropic-version header must be sent, got headers: {request}"
        );
        assert!(
            !lowercase.contains("authorization:"),
            "Anthropic must not get a Bearer header, got headers: {request}"
        );
    }

    /// A models endpoint that accepts the connection but never answers is a
    /// timeout-class failure bounded by the configured timeout.
    #[tokio::test]
    async fn wedged_model_list_endpoint_is_a_timeout_failure() {
        let base_url = serve_never_responding().await;
        let provider = provider_with_endpoint("custom", &base_url, Some("/models"));

        let started = std::time::Instant::now();
        let error = fetch_models(&provider, String::new(), 1).await.unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            matches!(error, PostProcessModelError::Timeout { .. }),
            "a wedged endpoint must classify as timeout, got: {error}"
        );
        assert_eq!(
            error.failure_class(),
            Some(PostProcessFailureClass::Timeout)
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the timeout must bound the fetch (took {elapsed:?})"
        );
    }

    /// The wire shape the frontend receives is a tagged object; pin it so
    /// the enum stays a discriminated union on the TS side.
    #[test]
    fn model_error_serializes_as_a_tagged_object() {
        let error = PostProcessModelError::Auth {
            detail: "bad key".to_string(),
        };
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "kind": "auth", "detail": "bad key" })
        );
        let round_tripped: PostProcessModelError =
            serde_json::from_value(serde_json::to_value(&error).unwrap()).unwrap();
        assert_eq!(round_tripped, error);
    }

    /// Detail truncation stays on char boundaries and bounds oversized
    /// error bodies.
    #[test]
    fn detail_truncation_bounds_bodies_on_char_boundaries() {
        assert_eq!(truncate_for_detail("short", 300), "short");
        let long = "x".repeat(500);
        let truncated = truncate_for_detail(&long, 300);
        assert_eq!(truncated.len(), 303);
        assert!(truncated.ends_with("..."));
        // A multi-byte character straddling the cut must not panic.
        let multibyte = "é".repeat(200);
        let truncated = truncate_for_detail(&multibyte, 301);
        assert!(truncated.ends_with("..."));
    }

    /// A 200 with content is the ok verdict: model list ok, completion ok,
    /// latency carried through, no failure class.
    #[test]
    fn cloud_verdict_ok_when_list_and_probe_answer() {
        let verdict = assemble_cloud_verdict(Ok((3, 42)), Some(Ok(Some("OK".to_string()))));
        assert!(verdict.model_list_ok);
        assert_eq!(verdict.completion_ok, Some(true));
        assert_eq!(verdict.latency_ms, Some(42), "latency is measured");
        assert_eq!(verdict.failure_class, None);
        assert!(verdict.detail.is_empty());
    }

    /// A list failure short-circuits the verdict: the auth class surfaces,
    /// no completion claim, no fabricated latency, and even a (defensive)
    /// probe outcome cannot smuggle a success in.
    #[test]
    fn cloud_verdict_list_failure_short_circuits() {
        let verdict = assemble_cloud_verdict(
            Err(PostProcessModelError::Auth {
                detail: "401".to_string(),
            }),
            Some(Ok(Some("OK".to_string()))),
        );
        assert!(!verdict.model_list_ok);
        assert_eq!(verdict.completion_ok, None);
        assert_eq!(verdict.latency_ms, None);
        assert_eq!(verdict.failure_class, Some(PostProcessFailureClass::Auth));
        assert!(verdict.detail.contains("401"));
    }

    /// A 200 answer with no content is an output-invalid failure, not a
    /// success: the probe asked for one word and got nothing usable.
    #[test]
    fn cloud_verdict_empty_completion_is_output_invalid() {
        for content in [None, Some(String::new()), Some("   ".to_string())] {
            let verdict = assemble_cloud_verdict(Ok((1, 5)), Some(Ok(content)));
            assert!(verdict.model_list_ok);
            assert_eq!(verdict.completion_ok, Some(false));
            assert_eq!(
                verdict.failure_class,
                Some(PostProcessFailureClass::OutputInvalid)
            );
        }
    }

    #[test]
    fn cloud_verdict_probe_error_keeps_the_list_verdict() {
        let verdict = assemble_cloud_verdict(
            Ok((2, 7)),
            Some(Err(PostProcessModelError::Timeout {
                detail: "wedged".to_string(),
            })),
        );
        assert!(verdict.model_list_ok);
        assert_eq!(verdict.completion_ok, Some(false));
        assert_eq!(verdict.latency_ms, Some(7));
        assert_eq!(
            verdict.failure_class,
            Some(PostProcessFailureClass::Timeout)
        );
        assert!(verdict.detail.contains("wedged"));
    }

    /// The local provider's verdict is pure downloaded state: ready when the
    /// selected model is on disk, a clear detail when it is missing or still
    /// downloading. No worker is involved in assembling it.
    #[test]
    fn local_verdict_reflects_downloaded_state() {
        let ready = local_provider_connection_verdict(true, false);
        assert!(ready.model_list_ok);
        assert_eq!(ready.completion_ok, Some(true));
        assert_eq!(ready.failure_class, None);
        assert!(ready.detail.is_empty());

        let missing = local_provider_connection_verdict(false, false);
        assert_eq!(missing.completion_ok, Some(false));
        assert!(missing.detail.contains("not downloaded"));

        let downloading = local_provider_connection_verdict(false, true);
        assert_eq!(downloading.completion_ok, Some(false));
        assert!(downloading.detail.contains("downloading"));
    }

    #[test]
    fn apple_verdict_tracks_availability() {
        let ready = apple_intelligence_connection_verdict(true);
        assert!(ready.model_list_ok);
        assert_eq!(ready.completion_ok, None);
        assert!(ready.detail.is_empty());

        let unavailable = apple_intelligence_connection_verdict(false);
        assert!(!unavailable.model_list_ok);
        assert!(unavailable.detail.contains("not available"));
    }

    /// The probe prompt is a one-word ask with max_tokens 5 on the wire.
    #[tokio::test]
    async fn probe_completion_sends_a_tiny_bounded_request() {
        let (base_url, request_rx) = serve_one_response_with_request(
            "200 OK",
            r#"{"choices":[{"message":{"content":"OK"}}]}"#,
        )
        .await;
        let provider = provider("custom", &base_url);

        let content = probe_completion(&provider, String::new(), "test-model", 5)
            .await
            .unwrap();
        assert_eq!(content.as_deref(), Some("OK"));

        let request = request_rx.await.unwrap();
        assert!(
            request.contains("\"max_tokens\":5"),
            "the probe must cap tokens, got body: {request}"
        );
        assert!(
            request.contains("Reply with the single word OK"),
            "the probe must send the fixed one-word prompt, got body: {request}"
        );
        assert!(
            request.contains("\"stream\":false"),
            "the probe must not stream, got body: {request}"
        );
    }

    /// A 401 from the completion probe is an auth-class failure, and the
    /// probe must not retry it (exactly one connection served).
    #[tokio::test]
    async fn probe_completion_auth_failure_is_not_retried() {
        let (base_url, served) = serve_counting_401().await;
        let provider = provider("custom", &base_url);

        let error = probe_completion(&provider, "sk-bad".to_string(), "test-model", 5)
            .await
            .unwrap_err();
        assert!(
            matches!(error, PostProcessModelError::Auth { .. }),
            "a 401 probe must classify as auth, got: {error}"
        );
        assert_eq!(error.failure_class(), Some(PostProcessFailureClass::Auth));

        // Give a would-be retry a beat to land, then assert it never came.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let connections = served.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            connections, 1,
            "an auth failure must not be retried, but {connections} connections were made"
        );
    }

    /// A wedged endpoint makes the probe fail as a timeout within its own
    /// budget, and the budget constant stays independent of the
    /// post-process timeout setting.
    #[tokio::test]
    async fn probe_completion_times_out_within_its_own_budget() {
        let base_url = serve_never_responding().await;
        let provider = provider("custom", &base_url);

        let started = std::time::Instant::now();
        let error = probe_completion(&provider, String::new(), "test-model", 1)
            .await
            .unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            matches!(error, PostProcessModelError::Timeout { .. }),
            "a wedged endpoint must classify as timeout, got: {error}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the probe timeout must bound the request (took {elapsed:?})"
        );

        // The hard budget is its own constant: 10s regardless of a much
        // longer (or shorter) post_process_timeout_secs.
        assert_eq!(CONNECTION_PROBE_TIMEOUT_SECS, 10);
        assert_ne!(
            CONNECTION_PROBE_TIMEOUT_SECS,
            DEFAULT_POST_PROCESS_TIMEOUT_SECS
        );
    }
}
