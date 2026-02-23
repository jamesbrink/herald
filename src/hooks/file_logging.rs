use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex;

use orra::hook::Hook;
use orra::message::{ToolCall, ToolResult};
use orra::namespace::Namespace;
use orra::provider::{CompletionRequest, CompletionResponse};
use orra::store::Session;

use crate::hlog;

/// Per-turn file logging hook.
///
/// Logs every lifecycle event (provider calls, tool calls, session I/O) via
/// `hlog!` so they appear in both stderr and the rotating log file.  Replaces
/// the orra built-in `LoggingHook` which only writes to stderr.
pub struct FileLoggingHook {
    call_start: Mutex<Option<Instant>>,
    turn: AtomicUsize,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
}

impl FileLoggingHook {
    pub fn new() -> Self {
        Self {
            call_start: Mutex::new(None),
            turn: AtomicUsize::new(0),
            total_input_tokens: AtomicU64::new(0),
            total_output_tokens: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Hook for FileLoggingHook {
    async fn after_session_load(&self, namespace: &Namespace, session: &Session) {
        let count = session.message_count();
        if count > 0 {
            hlog!(
                "[session] loaded {} messages for {}",
                count,
                namespace.key()
            );
        } else {
            hlog!("[session] new session for {}", namespace.key());
        }
        // Reset per-session counters
        self.turn.store(0, Ordering::Relaxed);
        self.total_input_tokens.store(0, Ordering::Relaxed);
        self.total_output_tokens.store(0, Ordering::Relaxed);
    }

    async fn before_provider_call(&self, request: &mut CompletionRequest) {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed) + 1;
        let msg_count = request.messages.len();
        let tool_count = request.tools.len();
        hlog!(
            "[llm] turn {}: calling provider ({} messages, {} tools)",
            turn,
            msg_count,
            tool_count
        );
        *self.call_start.lock().await = Some(Instant::now());
    }

    async fn after_provider_call(&self, response: &CompletionResponse) {
        let elapsed = self
            .call_start
            .lock()
            .await
            .map(|s| s.elapsed())
            .unwrap_or_default();

        self.total_input_tokens
            .fetch_add(response.usage.input_tokens as u64, Ordering::Relaxed);
        self.total_output_tokens
            .fetch_add(response.usage.output_tokens as u64, Ordering::Relaxed);

        let turn = self.turn.load(Ordering::Relaxed);
        hlog!(
            "[llm] turn {}: response in {:.1}s — {} in / {} out tokens (total: {} in / {} out)",
            turn,
            elapsed.as_secs_f64(),
            response.usage.input_tokens,
            response.usage.output_tokens,
            self.total_input_tokens.load(Ordering::Relaxed),
            self.total_output_tokens.load(Ordering::Relaxed),
        );
    }

    async fn before_tool_call(
        &self,
        _namespace: &Namespace,
        call: &mut ToolCall,
    ) -> Result<(), String> {
        let args = call.arguments.to_string();
        let preview = if args.len() > 120 {
            format!("{}...", &args[..120])
        } else {
            args
        };
        hlog!("[tool] calling: {} ({})", call.name, preview.replace('\n', " "));
        Ok(())
    }

    async fn after_tool_call(&self, call: &ToolCall, result: &mut ToolResult) {
        let status = if result.is_error { "error" } else { "ok" };
        let preview = if result.content.len() > 120 {
            format!("{}...", &result.content[..120])
        } else {
            result.content.clone()
        };
        hlog!(
            "[tool] {} → {} ({})",
            call.name,
            status,
            preview.replace('\n', " ")
        );
    }

    async fn before_session_save(&self, namespace: &Namespace, session: &mut Session) {
        hlog!(
            "[session] saving {} messages for {}",
            session.message_count(),
            namespace.key()
        );
    }
}
