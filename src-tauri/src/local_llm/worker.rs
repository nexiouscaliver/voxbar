//! The `--llm-worker` child process mainloop (spec 1.1, 3.3).
//!
//! ALL llama.cpp native code in the app lives in this file: a GGML abort,
//! driver fault, or hang here kills only this worker, never the app (the
//! transcribe-cpp isolation precedent). The worker speaks one JSON frame
//! per line on stdin/stdout ([`super::protocol`]), performs one Load per
//! process lifetime, any number of Generate requests, and exits on stdin
//! EOF (parent death included) - it never loops on a closed stdin.
//!
//! Unit tests never execute anything in this file's llama paths (spec 10
//! hermeticity contract); only the pure frame helpers are covered by
//! protocol.rs tests, and real inference is exercised by the #[ignore]d
//! smoke test behind a real model download.

use super::protocol::{parse_request_line, to_line, WorkerRequest, WorkerResponse};
use crate::local_llm::LOCAL_LLM_MODEL_SIZE_BYTES;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use log::{error, info, warn};
use std::io::{self, BufRead, BufReader, Write};
use std::num::NonZeroU32;

/// Hidden first argument that turns the executable into the LLM worker
/// (sibling of engine_supervisor's --transcribe-worker).
pub const WORKER_FLAG: &str = "--llm-worker";

/// Whether this process was launched as the post-process LLM worker.
/// Checked in `main` before CLI parsing, Tauri, or single-instance
/// handling, exactly like the transcribe worker's flag.
pub fn is_llm_worker_invocation() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == WORKER_FLAG)
}

/// Run the worker until the parent closes its stdin. Returns the exit code.
pub fn run() -> i32 {
    // The worker allocates the model weights and KV cache, so it needs the
    // same allocator tuning as the app (#1792). Before anything allocates.
    crate::memory::init_allocator();
    #[cfg(unix)]
    crate::engine_supervisor::ignore_app_signals();
    #[cfg(target_os = "linux")]
    crate::engine_supervisor::set_process_name();
    // llama.cpp logs to fd 1 in places; claim the real stdout for the
    // protocol first and point fd 1 at stderr, exactly like the transcribe
    // worker, so no stray native byte can corrupt the frame stream.
    let mut protocol_out = match crate::engine_supervisor::take_stdout_for_protocol() {
        Ok(file) => file,
        Err(e) => {
            eprintln!("llm worker: failed to claim stdout: {e}");
            return 2;
        }
    };
    crate::engine_supervisor::init_logger();

    info!(
        "llm worker started (pid {}, expecting a {}-byte model)",
        std::process::id(),
        LOCAL_LLM_MODEL_SIZE_BYTES
    );

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut session: Option<Session> = None;
    let exit_code = 0;

    'mainloop: loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            // EOF: the parent is gone (or closed stdin after Exit). Never
            // loop on a closed stdin - exit now.
            Ok(0) => break 'mainloop,
            Ok(_) => {}
            Err(e) => {
                error!("llm worker: failed reading stdin: {e}");
                break 'mainloop;
            }
        }
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        let request = match parse_request_line(line) {
            Ok(request) => request,
            Err(reason) => {
                // A malformed frame is an error, never a panic: answer
                // Failed and keep serving (T25's contract).
                warn!("llm worker: {}", reason);
                if respond(&mut protocol_out, &WorkerResponse::Failed { reason }).is_err() {
                    break 'mainloop;
                }
                continue;
            }
        };

        let response = match request {
            WorkerRequest::Load { path, n_ctx } => match Session::load(&path, n_ctx, &session) {
                Ok(new_session) => {
                    session = Some(new_session);
                    WorkerResponse::Loaded
                }
                Err(reason) => {
                    session = None;
                    WorkerResponse::Failed { reason }
                }
            },
            WorkerRequest::Generate {
                system,
                user,
                grammar,
                max_gen_tokens,
            } => match session.as_mut() {
                Some(current) => {
                    match current.generate(&system, &user, grammar.as_deref(), max_gen_tokens) {
                        Ok(text) => WorkerResponse::Generated { text },
                        Err(reason) => WorkerResponse::Failed { reason },
                    }
                }
                None => WorkerResponse::Failed {
                    reason: "no model is loaded in this worker".to_string(),
                },
            },
            // Best-effort marker only: the parent does not rely on Cancel
            // for prompt cancellation (generation is not resumable, so the
            // parent kills the process). No response either way.
            WorkerRequest::Cancel => continue,
            WorkerRequest::Exit => break 'mainloop,
        };

        if respond(&mut protocol_out, &response).is_err() {
            break 'mainloop;
        }
    }

    // Dropping the session releases the model and its context before
    // process exit; the OS reclaims the rest either way.
    drop(session);
    info!("llm worker exiting ({exit_code})");
    exit_code
}

fn respond(out: &mut impl Write, response: &WorkerResponse) -> io::Result<()> {
    out.write_all(to_line(response).as_bytes())?;
    out.flush()
}

/// One loaded model plus its inference context. Built once per process;
/// the model is leaked to give the context a 'static lifetime (the worker
/// exists to hold exactly this model until it exits).
struct Session {
    _backend: LlamaBackend,
    model: &'static LlamaModel,
    template: LlamaChatTemplate,
    ctx: LlamaContext<'static>,
}

impl Session {
    fn load(path: &str, n_ctx: u32, existing: &Option<Session>) -> Result<Session, String> {
        if existing.is_some() {
            return Err("a model is already loaded in this worker".to_string());
        }

        // Cheap integrity check ONLY (spec 6.1): the full sha256 was
        // verified once at download; a 610 MB re-hash per dictation would
        // add seconds. A length mismatch means a corrupted or wrong cache
        // file: fail the load so the parent falls back to the raw
        // transcript (delete + re-download repairs it).
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("cannot open model file '{}': {}", path, e))?;
        if meta.len() != LOCAL_LLM_MODEL_SIZE_BYTES {
            return Err(format!(
                "model file is {} bytes but the pinned model is {} bytes; delete and \
                 re-download it in Settings",
                meta.len(),
                LOCAL_LLM_MODEL_SIZE_BYTES
            ));
        }

        let backend = LlamaBackend::init().map_err(|e| format!("backend init failed: {}", e))?;
        let model_params = LlamaModelParams::default();
        let model = LlamaModel::load_from_file(&backend, path, &model_params)
            .map_err(|e| format!("failed to load gguf: {}", e))?;
        let model: &'static LlamaModel = Box::leak(Box::new(model));

        let template = model
            .chat_template(None)
            .map_err(|e| format!("the model has no usable chat template: {}", e))?;

        let n_ctx = NonZeroU32::new(n_ctx.max(1)).ok_or("n_ctx must be positive")?;
        let ctx_params = LlamaContextParams::default().with_n_ctx(Some(n_ctx));
        let ctx = model
            .new_context(&backend, ctx_params)
            .map_err(|e| format!("failed to create inference context: {}", e))?;

        info!("llm worker: model loaded from {}", path);
        Ok(Session {
            _backend: backend,
            model,
            template,
            ctx,
        })
    }

    /// Grammar-constrained greedy completion of one chat turn. Nothing is
    /// streamed: the full text comes back in the single Generated frame.
    fn generate(
        &mut self,
        system: &str,
        user: &str,
        grammar: Option<&str>,
        max_gen_tokens: u32,
    ) -> Result<String, String> {
        // Render the prompt with the model's OWN embedded template; the
        // parent already appended /no_think to the user content (the only
        // thinking-off mechanism this crate version supports).
        let mut messages = Vec::with_capacity(2);
        if !system.is_empty() {
            messages.push(
                LlamaChatMessage::new("system".to_string(), system.to_string())
                    .map_err(|e| format!("invalid system message: {}", e))?,
            );
        }
        messages.push(
            LlamaChatMessage::new("user".to_string(), user.to_string())
                .map_err(|e| format!("invalid user message: {}", e))?,
        );
        let prompt = self
            .model
            .apply_chat_template(&self.template, &messages, true)
            .map_err(|e| format!("failed to apply the chat template: {}", e))?;

        let prompt_tokens = self.model.vocab().tokenize(prompt.as_bytes(), true, true);
        if prompt_tokens.is_empty() {
            return Err("the rendered prompt is empty".to_string());
        }
        let n_ctx = self.ctx.n_ctx();
        if prompt_tokens.len() as u64 + 1 > n_ctx as u64 {
            return Err(format!(
                "the prompt ({} tokens) does not fit the {}-token context",
                prompt_tokens.len(),
                n_ctx
            ));
        }

        // Decode the whole prompt; only the final position carries logits.
        let mut batch = LlamaBatch::new(prompt_tokens.len(), 1);
        for (i, token) in prompt_tokens.iter().enumerate() {
            batch
                .add(*token, i as _, &[0], i + 1 == prompt_tokens.len())
                .map_err(|e| format!("batch fill: {}", e))?;
        }
        self.ctx
            .decode(&mut batch)
            .map_err(|e| format!("prompt decode failed: {}", e))?;

        // Grammar-constrained greedy sampling: deterministic output, and
        // the grammar guarantees shape at the token level (stronger than a
        // server-side best-effort JSON mode).
        let mut sampler = match grammar {
            Some(gbnf) => {
                let grammar_sampler = LlamaSampler::grammar(self.model, gbnf, "root")
                    .map_err(|e| format!("invalid grammar: {}", e))?;
                LlamaSampler::chain_simple([grammar_sampler, LlamaSampler::greedy()])
            }
            None => LlamaSampler::greedy(),
        };

        let vocab = self.model.vocab();
        let mut output: Vec<u8> = Vec::new();
        let mut pos = prompt_tokens.len() as i32;
        let mut logits_index = batch.n_tokens() - 1;
        let mut generated = 0u32;
        while generated < max_gen_tokens {
            generated += 1;
            let token = sampler.sample(&self.ctx, logits_index);
            if vocab.is_eog(token) {
                break;
            }
            output.extend_from_slice(&vocab.token_to_piece(token, true, None));

            // Feed the sampled token back; its logits drive the next step.
            let mut next = LlamaBatch::new(1, 1);
            next.add(token, pos, &[0], true)
                .map_err(|e| format!("batch fill: {}", e))?;
            self.ctx
                .decode(&mut next)
                .map_err(|e| format!("generation decode failed: {}", e))?;
            pos += 1;
            logits_index = 0;
        }

        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_llm::protocol::WORKER_N_CTX;

    /// The worker flag is a distinct argv token: a normal launch (no args,
    /// or any other first argument) must never match it.
    #[test]
    fn worker_flag_matches_only_itself() {
        assert_eq!(WORKER_FLAG, "--llm-worker");
        // The check reads argv, so exercise the comparison directly.
        assert!("--llm-worker" == WORKER_FLAG);
        assert!("--transcribe-worker" != WORKER_FLAG);
        assert!("" != WORKER_FLAG);
    }

    /// The pinned byte length matches the registry size rounding: 610 MB
    /// display size must be the rounded-down form of the exact byte count.
    #[test]
    fn pinned_byte_length_matches_display_size() {
        assert_eq!(LOCAL_LLM_MODEL_SIZE_BYTES, 639_446_688);
        let mb = LOCAL_LLM_MODEL_SIZE_BYTES / (1024 * 1024);
        assert!(
            crate::local_llm::LOCAL_LLM_MODEL_SIZE_MB == mb
                || crate::local_llm::LOCAL_LLM_MODEL_SIZE_MB == mb + 1,
            "display size {} MB must round from {} bytes",
            crate::local_llm::LOCAL_LLM_MODEL_SIZE_MB,
            LOCAL_LLM_MODEL_SIZE_BYTES
        );
        assert_eq!(WORKER_N_CTX, 4096);
    }

    // Real-inference verification against the actual pinned model runs as
    // a STANDALONE check outside this crate (a tiny cargo project that
    // depends on llama-cpp-2 and loads the GGUF), NOT as a test here:
    // llama_cpp_sys_2 and transcribe_cpp_sys each embed their own ggml,
    // and the lib-test binary cannot link references to both (duplicate
    // ggml symbols). The app binary links fine (llama's ggml objects are
    // simply never pulled behind transcribe-cpp's identical symbols), and
    // unit tests never execute llama paths anyway (spec 10).
}
