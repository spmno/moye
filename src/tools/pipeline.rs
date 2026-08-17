//! 3-stage waterfall tool execution pipeline (todo 9).
//!
//! Two INDEPENDENT layers (GAP-1 fix — rig 0.41's `AgentHook` is before/after
//! callbacks, NOT next()-style middleware; we do NOT "extend hooks into a
//! waterfall"):
//!
//! 1. **pre/post layer (rig hooks)**: `PipelineHooks` are simplified
//!    `Vec<Box<dyn Fn>>` listeners invoked from `HitlHook::on_tool_call` /
//!    `on_tool_result`. `PreAction` maps to rig's `ToolCallAction`
//!    (Run/Skip/Rewrite); `PostAction` maps to `ToolResultAction`
//!    (Keep/Rewrite/Stop). rig 0.41 has no Block variant — `Skip`/`Rewrite`
//!    simulate block.
//!
//! 2. **around-execute layer (Tool trait wrapper)**: `TimeoutRetryTool<I: Tool>`
//!    impl `Tool`, wraps `inner.call()` with `tokio::time::timeout` and retries
//!    on timeout. This is the Rust equivalent of around-middleware. It does NOT
//!    use hooks.
//!
//! The `Pipeline` orchestrator composes the layers for unit testing:
//! `pre_execute` → `approval` (`ToolApproval`) → `execute` (`TimeoutRetryTool`)
//! → `post_execute` → `result` (frozen). In production, rig's runtime already
//! composes these: `on_tool_call` (HitlHook + pre listeners) → tool `call()`
//! (= `TimeoutRetryTool`) → `on_tool_result` (HitlHook + post listeners).

use std::time::Duration;

use rig_agent::tool::{IntoToolOutput, Tool, ToolContext, ToolExecutionError};
use serde_json::Value;

use crate::registry::ApprovalChain;
use crate::seam::{ApprovalRequest, ApprovalVerdict, ToolApproval};

// ---------------------------------------------------------------------------
// Pre / post action enums — map 1:1 to rig's ToolCallAction / ToolResultAction.
// ---------------------------------------------------------------------------

/// Pre-execute listener verdict. Maps to rig's `ToolCallAction`.
#[derive(Debug, Clone, PartialEq)]
pub enum PreAction {
    /// Execute the tool with current args. (= `ToolCallAction::Run`)
    Run,
    /// Do not execute; return this reason to the model. (= `Skip(String)`)
    Skip(String),
    /// Replace args, then continue. (= `Rewrite(serde_json::Value)`)
    Rewrite(Value),
}

/// Post-execute listener verdict. Maps to rig's `ToolResultAction`.
#[derive(Debug, Clone, PartialEq)]
pub enum PostAction {
    /// Pass the result through unchanged. (= `Keep`)
    Keep,
    /// Replace the result content sent to the model. (= `Rewrite(String)`)
    Rewrite(String),
    /// Stop the agent loop. (= `Stop(String)`)
    Stop,
}

// ---------------------------------------------------------------------------
// Owned listener payloads (avoid rig's borrowed `ToolCall<'_>` lifetime in
// `Box<dyn Fn>`).
// ---------------------------------------------------------------------------

/// Owned snapshot of a tool-call request, passed to pre-execute listeners.
#[derive(Debug, Clone)]
pub struct PipelineCall {
    pub tool_name: String,
    pub args: Value,
    pub role: Option<String>,
}

/// Owned snapshot of a tool-call result, passed to post-execute listeners.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineResult {
    pub tool_name: String,
    pub content: String,
    pub ok: bool,
}

/// Final pipeline outcome after all stages.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineOutcome {
    /// Tool executed (or skipped by pre/approval); post stage processed.
    Executed(PipelineResult),
    /// Pre-execute or approval denied; tool NOT executed.
    Skipped(String),
    /// Post-execute requested the agent loop stop.
    Stopped(String),
}

// ---------------------------------------------------------------------------
// PipelineHooks — simplified `Vec<Box<dyn Fn>>` listeners (NOT a full event
// system; todo 11 handles the full mechanism).
// ---------------------------------------------------------------------------

type PreListener = Box<dyn Fn(&PipelineCall) -> PreAction + Send + Sync>;
type PostListener = Box<dyn Fn(&PipelineResult) -> PostAction + Send + Sync>;

/// Simplified pre/post listener set. `Send + Sync` bounds let it live inside
/// `HitlHook` (which is shared across rig's async runtime).
#[derive(Default)]
pub struct PipelineHooks {
    pre: Vec<PreListener>,
    post: Vec<PostListener>,
}

impl PipelineHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_pre<F>(&mut self, f: F)
    where
        F: Fn(&PipelineCall) -> PreAction + Send + Sync + 'static,
    {
        self.pre.push(Box::new(f));
    }

    pub fn add_post<F>(&mut self, f: F)
    where
        F: Fn(&PipelineResult) -> PostAction + Send + Sync + 'static,
    {
        self.post.push(Box::new(f));
    }

    /// Run all pre-execute listeners in order. Each listener's return fully
    /// replaces the prior verdict; the last listener's action is effective
    /// (default `Run` when there are no listeners).
    pub fn pre_execute(&self, call: &PipelineCall) -> PreAction {
        let mut action = PreAction::Run;
        for f in &self.pre {
            action = f(call);
        }
        action
    }

    /// Run all post-execute listeners in order. The first non-`Keep` action
    /// wins and short-circuits (later listeners are not consulted). Returns
    /// `Keep` when there are no listeners or all return `Keep`.
    pub fn post_execute(&self, result: &PipelineResult) -> PostAction {
        for f in &self.post {
            let action = f(result);
            if !matches!(action, PostAction::Keep) {
                return action;
            }
        }
        PostAction::Keep
    }
}

// ---------------------------------------------------------------------------
// TimeoutRetryTool — around-execute wrapper (Tool trait). Independent of hooks.
// ---------------------------------------------------------------------------

/// Error from the around-execute wrapper: either the inner tool's typed error
/// or a timeout after exhausting retries.
#[derive(Debug, thiserror::Error)]
pub enum TimeoutRetryError<E: std::error::Error + 'static> {
    /// The inner tool returned an error (no retry on inner errors; only
    /// timeouts trigger retry per the design).
    #[error(transparent)]
    Inner(#[from] E),
    /// `tokio::time::timeout` elapsed and `max_retries` exhausted.
    #[error("tool timed out after {timeout:?} ({retries} retry attempt(s) failed)")]
    Timeout { timeout: Duration, retries: u32 },
    /// Failed to deserialize the cloned args for a retry attempt.
    #[error("invalid arguments on retry: {0}")]
    InvalidArgs(serde_json::Error),
}

/// Around-execute wrapper: wraps an inner `Tool`'s `call()` with
/// `tokio::time::timeout` and retries on timeout up to `max_retries`.
///
/// Uses `type Args = serde_json::Value` so retry attempts can re-deserialize
/// fresh args (the inner `Args` are moved into each attempt's future; on
/// timeout the future is dropped, so we reconstruct args from the cloneable
/// `Value`). This does NOT require `I::Args: Clone`.
pub struct TimeoutRetryTool<I: Tool> {
    inner: I,
    timeout: Duration,
    max_retries: u32,
}

impl<I: Tool> TimeoutRetryTool<I> {
    pub fn new(inner: I, timeout: Duration, max_retries: u32) -> Self {
        Self {
            inner,
            timeout,
            max_retries,
        }
    }

    /// Behavior-preserving wrapper: 300s timeout, 0 retries (no double-exec
    /// risk for mutating tools). The safety net fires only on genuinely hung
    /// tools.
    pub fn passthrough(inner: I) -> Self {
        Self::new(inner, Duration::from_secs(300), 0)
    }
}

impl<I: Tool> Tool for TimeoutRetryTool<I> {
    const NAME: &'static str = I::NAME;
    type Args = Value;
    type Output = I::Output;
    type Error = TimeoutRetryError<I::Error>;

    fn description(&self) -> String {
        self.inner.description()
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match error {
            TimeoutRetryError::Inner(e) => I::map_error(&self.inner, e),
            TimeoutRetryError::Timeout { timeout, retries } => ToolExecutionError::timeout(
                format!("tool timed out after {timeout:?} ({retries} retry attempt(s) failed)"),
            )
            .with_retryable(true)
            .with_model_feedback(format!(
                "tool timed out after {timeout:?}; {retries} retry attempt(s) failed"
            )),
            TimeoutRetryError::InvalidArgs(e) => {
                ToolExecutionError::invalid_args(e.to_string()).with_source(e)
            }
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let mut retries: u32 = 0;
        loop {
            let parsed = serde_json::from_value::<I::Args>(args.clone())
                .map_err(TimeoutRetryError::InvalidArgs)?;
            match tokio::time::timeout(self.timeout, self.inner.call(context, parsed)).await {
                Ok(res) => return res.map_err(TimeoutRetryError::Inner),
                Err(_) => {
                    if retries >= self.max_retries {
                        return Err(TimeoutRetryError::Timeout {
                            timeout: self.timeout,
                            retries,
                        });
                    }
                    retries += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline orchestrator — composes pre -> approval -> execute -> post.
// Testable unit; in production rig's runtime already composes the layers.
// ---------------------------------------------------------------------------

/// Composes the 3-stage pipeline around a `TimeoutRetryTool` for unit tests.
pub struct Pipeline {
    hooks: PipelineHooks,
    approval: std::sync::Arc<ApprovalChain>,
}

impl Pipeline {
    pub fn new(hooks: PipelineHooks, approval: std::sync::Arc<ApprovalChain>) -> Self {
        Self { hooks, approval }
    }

    /// Run the full pipeline: pre_execute -> approval -> execute -> post_execute.
    /// Returns the frozen outcome. `Ask` verdicts (needing real HITL) resolve to
    /// `Skipped` here — the live y/n prompt lives in `HitlHook`, not the orchestrator.
    pub async fn run<I: Tool>(
        &self,
        tool: &TimeoutRetryTool<I>,
        call: PipelineCall,
    ) -> PipelineOutcome {
        let tool_name = call.tool_name.clone();
        let role = call.role.clone().unwrap_or_default();

        // 1. pre_execute
        let mut args = call.args.clone();
        match self.hooks.pre_execute(&call) {
            PreAction::Run => {}
            PreAction::Skip(reason) => return PipelineOutcome::Skipped(reason),
            PreAction::Rewrite(new_args) => args = new_args,
        }

        // 2. approval
        let req = ApprovalRequest {
            tool_name: tool_name.clone(),
            args: args.clone(),
            role,
        };
        match self.approval.request(&req) {
            ApprovalVerdict::Allow => {}
            ApprovalVerdict::Deny => {
                return PipelineOutcome::Skipped("denied by approval".into());
            }
            ApprovalVerdict::Ask => {
                return PipelineOutcome::Skipped("needs HITL approval (Ask)".into());
            }
        }

        // 3. execute (TimeoutRetryTool wrapper)
        let mut ctx = ToolContext::new();
        let result = match tool.call(&mut ctx, args).await {
            Ok(output) => {
                let content = output
                    .into_tool_output()
                    .map(|o| o.as_text().unwrap_or("").to_string())
                    .unwrap_or_else(|e| e.to_string());
                PipelineResult {
                    tool_name,
                    content,
                    ok: true,
                }
            }
            Err(e) => PipelineResult {
                tool_name,
                content: e.to_string(),
                ok: false,
            },
        };

        // 4. post_execute -> frozen outcome
        match self.hooks.post_execute(&result) {
            PostAction::Keep => PipelineOutcome::Executed(result),
            PostAction::Rewrite(content) => {
                PipelineOutcome::Executed(PipelineResult { content, ..result })
            }
            PostAction::Stop => PipelineOutcome::Stopped("stopped by post-execute hook".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ---- test fixtures ----

    /// A tool that echoes its `who` field as output and records call count.
    struct EchoTool {
        calls: Arc<AtomicU32>,
    }
    impl Tool for EchoTool {
        const NAME: &'static str = "echo";
        type Args = Value;
        type Output = String;
        type Error = Infallible;
        fn description(&self) -> String {
            "echo who".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{"who":{"type":"string"}},"required":["who"]})
        }
        async fn call(
            &self,
            _ctx: &mut ToolContext,
            args: Self::Args,
        ) -> Result<String, Infallible> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let who = args.get("who").and_then(|v| v.as_str()).unwrap_or("?");
            Ok(format!("echo:{who}"))
        }
    }

    /// A tool that never completes (drives the timeout path).
    struct HangingTool;
    impl Tool for HangingTool {
        const NAME: &'static str = "hanging";
        type Args = Value;
        type Output = String;
        type Error = Infallible;
        fn description(&self) -> String {
            "never completes".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(
            &self,
            _ctx: &mut ToolContext,
            _args: Self::Args,
        ) -> Result<String, Infallible> {
            std::future::pending::<()>().await;
            Ok("unreachable".into())
        }
    }

    /// A tool that fails immediately with a typed error (verifies inner errors
    /// are NOT retried and propagate through `TimeoutRetryError::Inner`).
    #[derive(Debug, thiserror::Error)]
    #[error("boom")]
    struct Boom;
    struct FailTool;
    impl Tool for FailTool {
        const NAME: &'static str = "fail";
        type Args = Value;
        type Output = String;
        type Error = Boom;
        fn description(&self) -> String {
            "fails immediately".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _ctx: &mut ToolContext, _args: Self::Args) -> Result<String, Boom> {
            Err(Boom)
        }
    }

    /// Mock `ToolApproval` returning a fixed verdict.
    struct MockApproval {
        verdict: ApprovalVerdict,
    }
    impl ToolApproval for MockApproval {
        fn request(&self, _req: &ApprovalRequest) -> ApprovalVerdict {
            self.verdict
        }
    }
    fn chain(verdict: ApprovalVerdict) -> Arc<ApprovalChain> {
        let mut c = ApprovalChain::new();
        c.add(Box::new(MockApproval { verdict }));
        Arc::new(c)
    }
    fn empty_chain() -> Arc<ApprovalChain> {
        Arc::new(ApprovalChain::new())
    }

    fn call(name: &str, who: &str) -> PipelineCall {
        PipelineCall {
            tool_name: name.into(),
            args: json!({"who": who}),
            role: Some("builder".into()),
        }
    }

    // ==================== pre_execute ====================

    #[test]
    fn pre_execute_run_when_no_listeners() {
        let hooks = PipelineHooks::new();
        let action = hooks.pre_execute(&call("echo", "x"));
        assert_eq!(action, PreAction::Run);
    }

    #[test]
    fn pre_execute_skip_denies_call() {
        let mut hooks = PipelineHooks::new();
        hooks.add_pre(|_c| PreAction::Skip("denied by policy".into()));
        let action = hooks.pre_execute(&call("echo", "x"));
        assert_eq!(action, PreAction::Skip("denied by policy".into()));
    }

    #[test]
    fn pre_execute_rewrite_replaces_args() {
        let mut hooks = PipelineHooks::new();
        hooks.add_pre(|_c| PreAction::Rewrite(json!({"who": "rewritten"})));
        let action = hooks.pre_execute(&call("echo", "x"));
        match action {
            PreAction::Rewrite(v) => assert_eq!(v["who"], "rewritten"),
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn pre_execute_last_non_run_wins() {
        let mut hooks = PipelineHooks::new();
        hooks.add_pre(|_c| PreAction::Skip("first".into()));
        // A later listener overrides with Run -> tool proceeds.
        hooks.add_pre(|_c| PreAction::Run);
        let action = hooks.pre_execute(&call("echo", "x"));
        assert_eq!(action, PreAction::Run);
    }

    // ==================== post_execute ====================

    #[test]
    fn post_execute_keep_when_no_listeners() {
        let hooks = PipelineHooks::new();
        let res = PipelineResult {
            tool_name: "echo".into(),
            content: "ok".into(),
            ok: true,
        };
        assert_eq!(hooks.post_execute(&res), PostAction::Keep);
    }

    #[test]
    fn post_execute_rewrite_replaces_content() {
        let mut hooks = PipelineHooks::new();
        hooks.add_post(|_r| PostAction::Rewrite("replaced".into()));
        let res = PipelineResult {
            tool_name: "echo".into(),
            content: "original".into(),
            ok: true,
        };
        assert_eq!(
            hooks.post_execute(&res),
            PostAction::Rewrite("replaced".into())
        );
    }

    #[test]
    fn post_execute_stop_short_circuits() {
        let mut hooks = PipelineHooks::new();
        hooks.add_post(|_r| PostAction::Stop);
        // A later listener that would Keep should never run.
        hooks.add_post(|_r| PostAction::Keep);
        let res = PipelineResult {
            tool_name: "echo".into(),
            content: "x".into(),
            ok: true,
        };
        assert_eq!(hooks.post_execute(&res), PostAction::Stop);
    }

    // ==================== TimeoutRetryTool ====================

    #[tokio::test]
    async fn timeout_retry_succeeds_on_first_attempt() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::new(
            EchoTool {
                calls: calls.clone(),
            },
            Duration::from_secs(5),
            3,
        );
        let mut ctx = ToolContext::new();
        let out = tool
            .call(&mut ctx, json!({"who":"world"}))
            .await
            .expect("first attempt succeeds");
        assert_eq!(out, "echo:world");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no retries on success");
    }

    #[tokio::test]
    async fn timeout_retry_retries_then_errors_after_max() {
        let tool = TimeoutRetryTool::new(HangingTool, Duration::from_millis(50), 2);
        let mut ctx = ToolContext::new();
        let err = tool
            .call(&mut ctx, json!({}))
            .await
            .expect_err("hanging tool must time out");
        match err {
            TimeoutRetryError::Timeout { retries, .. } => {
                assert_eq!(retries, 2, "should retry exactly max_retries times");
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_retry_zero_retries_fails_on_first_timeout() {
        let tool = TimeoutRetryTool::new(HangingTool, Duration::from_millis(50), 0);
        let mut ctx = ToolContext::new();
        let err = tool
            .call(&mut ctx, json!({}))
            .await
            .expect_err("no retries");
        match err {
            TimeoutRetryError::Timeout { retries, .. } => assert_eq!(retries, 0),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inner_error_not_retried_propagates_immediately() {
        let tool = TimeoutRetryTool::new(FailTool, Duration::from_secs(5), 3);
        let mut ctx = ToolContext::new();
        let err = tool
            .call(&mut ctx, json!({}))
            .await
            .expect_err("fail tool errors");
        assert!(
            matches!(err, TimeoutRetryError::Inner(_)),
            "inner errors must not retry"
        );
    }

    // ==================== approval ====================

    #[tokio::test]
    async fn approval_deny_skips_execution() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let pipeline = Pipeline::new(PipelineHooks::new(), chain(ApprovalVerdict::Deny));
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        match outcome {
            PipelineOutcome::Skipped(reason) => {
                assert!(reason.contains("denied") || reason.contains("approval"));
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tool must NOT execute on Deny"
        );
    }

    #[tokio::test]
    async fn approval_ask_resolves_to_skipped_without_tui() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let pipeline = Pipeline::new(PipelineHooks::new(), chain(ApprovalVerdict::Ask));
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        assert!(
            matches!(outcome, PipelineOutcome::Skipped(_)),
            "Ask has no TUI here"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn approval_chain_empty_fails_closed() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let pipeline = Pipeline::new(PipelineHooks::new(), empty_chain());
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        assert!(
            matches!(outcome, PipelineOutcome::Skipped(_)),
            "empty chain must fail-closed (Deny)"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tool must NOT execute on fail-closed"
        );
    }

    #[tokio::test]
    async fn approval_chain_custom_deny_short_circuits_over_default_allow() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let mut chain = ApprovalChain::new();
        chain.add(Box::new(MockApproval {
            verdict: ApprovalVerdict::Allow,
        }));
        chain.add(Box::new(MockApproval {
            verdict: ApprovalVerdict::Deny,
        }));
        let pipeline = Pipeline::new(PipelineHooks::new(), Arc::new(chain));
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        assert!(
            matches!(outcome, PipelineOutcome::Skipped(_)),
            "custom Deny must short-circuit even when default says Allow"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tool must NOT execute when custom denies"
        );
    }

    // ==================== full pipeline integration ====================

    #[tokio::test]
    async fn happy_path_full_pipeline() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let pipeline = Pipeline::new(PipelineHooks::new(), chain(ApprovalVerdict::Allow));
        let outcome = pipeline.run(&tool, call("echo", "world")).await;
        match outcome {
            PipelineOutcome::Executed(res) => {
                assert_eq!(res.tool_name, "echo");
                assert!(res.ok, "result should be ok");
                assert_eq!(res.content, "echo:world");
            }
            other => panic!("expected Executed, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_deny_tool_not_executed() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let mut hooks = PipelineHooks::new();
        hooks.add_pre(|_c| PreAction::Skip("blocked by pre".into()));
        let pipeline = Pipeline::new(hooks, chain(ApprovalVerdict::Allow));
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        match outcome {
            PipelineOutcome::Skipped(reason) => assert_eq!(reason, "blocked by pre"),
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "tool must NOT execute when pre denies"
        );
    }

    #[tokio::test]
    async fn post_replace_changes_model_visible_result() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let mut hooks = PipelineHooks::new();
        hooks.add_post(|_r| PostAction::Rewrite("REPLACED".into()));
        let pipeline = Pipeline::new(hooks, chain(ApprovalVerdict::Allow));
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        match outcome {
            PipelineOutcome::Executed(res) => {
                assert_eq!(res.content, "REPLACED");
                assert!(res.ok);
            }
            other => panic!("expected Executed, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "tool DID execute; only result replaced"
        );
    }

    #[tokio::test]
    async fn pre_rewrite_changes_args_seen_by_tool() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let mut hooks = PipelineHooks::new();
        hooks.add_pre(|_c| PreAction::Rewrite(json!({"who":"rewritten"})));
        let pipeline = Pipeline::new(hooks, chain(ApprovalVerdict::Allow));
        let outcome = pipeline.run(&tool, call("echo", "original")).await;
        match outcome {
            PipelineOutcome::Executed(res) => assert_eq!(res.content, "echo:rewritten"),
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_stop_halts_pipeline() {
        let calls = Arc::new(AtomicU32::new(0));
        let tool = TimeoutRetryTool::passthrough(EchoTool {
            calls: calls.clone(),
        });
        let mut hooks = PipelineHooks::new();
        hooks.add_post(|_r| PostAction::Stop);
        let pipeline = Pipeline::new(hooks, chain(ApprovalVerdict::Allow));
        let outcome = pipeline.run(&tool, call("echo", "x")).await;
        assert!(matches!(outcome, PipelineOutcome::Stopped(_)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "tool executed before post Stop"
        );
    }
}
