use super::run_state::{Checkpoint, RunPhase, ToolExecution};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::{
    AssistantMessage, ContextBuilder, UserMessage,
    llm::LlmProvider,
    message::{Message, ToolResult},
    session::Session,
    tools::{TaskPlan, ToolRegistry},
};

#[derive(Clone)]
pub struct Agent {
    llm: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    context: ContextBuilder,
    limits: Option<(usize, Option<usize>)>,
    compactor: Option<super::compact::Compactor>,
    compact_background_margin: u64,
    journal: Option<crate::audit::Journal>,
    sandbox: Option<Arc<super::sandbox::Sandbox>>,
    subagents: Option<Arc<super::subagents::Subagents>>,
}

#[derive(Debug)]
pub enum AgentEvent {
    InputConsumed,
    SessionChanged {
        previous_user_messages: usize,
    },
    TextDelta(String),
    PlanUpdated(TaskPlan),
    Retrying(String),
    Compaction(String),
    Message(AssistantMessage),
    ContextTokens(Option<u64>),
    ToolStarted {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished {
        call_id: String,
        failed: bool,
        output: serde_json::Value,
    },
}

#[derive(Debug)]
pub(super) struct GenerationSuperseded;

impl std::fmt::Display for GenerationSuperseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Generation superseded by new input")
    }
}

impl std::error::Error for GenerationSuperseded {}

impl Agent {
    pub(crate) fn child_agent(&self) -> Self {
        let mut agent = self.clone();
        agent.subagents = None;
        agent.tools = self.tools.without(&[
            "spawn_agent",
            "send_message",
            "list_agents",
            "wait_agent",
            "stop_agent",
            "messages",
            "telegram_reply",
            "telegram_results",
            "wakeups",
            "webhooks",
        ]);
        agent.limits = Some((16, None));
        agent
    }

    pub(crate) fn with_subagents(mut self, manager: Arc<super::subagents::Subagents>) -> Self {
        self.tools = super::tools::subagents::register(self.tools, manager.clone());
        self.subagents = Some(manager);
        self
    }

    pub(super) fn subagents(&self) -> Option<&Arc<super::subagents::Subagents>> {
        self.subagents.as_ref()
    }

    pub(super) fn llm(&self) -> &dyn LlmProvider {
        self.llm.as_ref()
    }

    pub fn new(llm: Arc<dyn LlmProvider>, tools: ToolRegistry, context: ContextBuilder) -> Self {
        Self {
            llm,
            tools,
            context,
            limits: None,
            compactor: None,
            compact_background_margin: 0,
            journal: None,
            sandbox: None,
            subagents: None,
        }
    }

    pub(crate) fn with_compaction(mut self, llm: Arc<dyn LlmProvider>, threshold: u64) -> Self {
        self.compactor = Some(super::compact::Compactor { llm, threshold });
        self
    }

    pub(crate) fn with_background_compaction(mut self, margin: u64) -> Self {
        self.compact_background_margin = margin;
        self
    }

    pub(crate) fn with_limits(mut self, requests: usize, input_chars: usize) -> Self {
        self.limits = Some((requests, Some(input_chars)));
        self
    }

    pub(crate) fn with_sandbox(mut self, sandbox: Arc<super::sandbox::Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    pub(crate) fn with_journal(mut self, journal: crate::audit::Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    pub(super) async fn prepare_input(
        &self,
        session: &mut Session,
        input: &mut UserMessage,
    ) -> anyhow::Result<()> {
        if let Some(sandbox) = &self.sandbox {
            let catalog = sandbox.prepare_input(input).await?;
            session.initialize_skills_catalog(catalog);
        }
        Ok(())
    }

    pub async fn run(
        &self,
        session: &mut Session,
        input: UserMessage,
        progress: Option<&mpsc::Sender<AgentEvent>>,
        incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
    ) -> anyhow::Result<AssistantMessage> {
        self.run_inner(session, Some(input), progress, incoming, None, None)
            .await
    }

    pub(crate) async fn run_bounded(
        &self,
        session: &mut Session,
        input: UserMessage,
        calls: usize,
    ) -> anyhow::Result<AssistantMessage> {
        super::llm::REQUEST_BUDGET
            .scope(
                std::cell::Cell::new(calls),
                self.run_inner(session, Some(input), None, None, Some(calls), None),
            )
            .await
    }

    pub(crate) async fn run_durable(
        &self,
        session: &mut Session,
        input: UserMessage,
        progress: Option<&mpsc::Sender<AgentEvent>>,
        incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
        checkpoint: Option<&mut Checkpoint>,
    ) -> anyhow::Result<AssistantMessage> {
        self.run_inner(session, Some(input), progress, incoming, None, checkpoint)
            .await
    }

    pub(super) fn isolated(&self) -> bool {
        self.sandbox.is_some()
    }

    pub(super) async fn resume_durable(
        &self,
        session: &mut Session,
        cp: &mut Checkpoint,
    ) -> anyhow::Result<AssistantMessage> {
        anyhow::ensure!(
            cp.record.isolated == Some(self.isolated()),
            "Execution permissions changed or legacy run has no isolation provenance"
        );
        anyhow::ensure!(
            !matches!(cp.record.phase, RunPhase::Parked { .. }),
            "Parked run requires explicit retry"
        );
        if let RunPhase::ExecutingTool { call_id, replay } = cp.record.phase.clone() {
            let execution = cp
                .store
                .tool_execution(&cp.record.id, &call_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Missing interrupted tool intent"))?;
            anyhow::ensure!(
                execution.result.is_none()
                    && execution.replay == replay
                    && cp
                        .record
                        .pending_calls
                        .first()
                        .is_some_and(|call| call.call_id == call_id),
                "Inconsistent interrupted tool checkpoint"
            );
            if replay != super::tools::ReplayPolicy::Safe
                || self.tools.replay_policy(&execution.call.name)
                    != super::tools::ReplayPolicy::Safe
            {
                let output: super::message::ToolOutput = serde_json::json!({
                    "interrupted":true, "outcome":"unknown",
                    "error":"This action may have partially executed before the process stopped. Do not repeat it blindly. First verify the external state; ask the user for clarification if it cannot be verified."
                }).into();
                session.push(Message::ToolResult(ToolResult {
                    call_id,
                    content: output.clone(),
                }));
                cp.record.pending_calls.remove(0);
                let next = cp
                    .record
                    .pending_calls
                    .first()
                    .map_or(RunPhase::NeedModel, |call| RunPhase::NeedTool {
                        call_id: call.call_id.clone(),
                    });
                cp.save(
                    session,
                    next,
                    Some(ToolExecution {
                        result: Some(output),
                        ..execution
                    }),
                )
                .await?;
            }
        }
        if let RunPhase::NeedTool { call_id } | RunPhase::ExecutingTool { call_id, .. } =
            &cp.record.phase
        {
            anyhow::ensure!(
                cp.record
                    .pending_calls
                    .first()
                    .is_some_and(|c| &c.call_id == call_id),
                "Invalid pending tool order"
            );
        } else {
            anyhow::ensure!(
                cp.record.pending_calls.is_empty(),
                "Pending calls in non-tool phase"
            );
        }
        self.run_inner(session, None, None, None, None, Some(cp))
            .await
    }

    async fn run_inner(
        &self,
        session: &mut Session,
        input: Option<UserMessage>,
        progress: Option<&mpsc::Sender<AgentEvent>>,
        incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
        call_budget: Option<usize>,
        mut checkpoint: Option<&mut Checkpoint>,
    ) -> anyhow::Result<AssistantMessage> {
        if input.is_some()
            && let Some(cp) = checkpoint.as_deref_mut()
        {
            cp.record.isolated = Some(self.isolated());
        }
        let id = session.id().as_str();
        let trigger = if id.starts_with("companion:wakeup:") {
            "wakeup"
        } else if id.starts_with("companion:proactive:") {
            "proactive"
        } else if id.starts_with("cli:once:") {
            "one_shot"
        } else {
            "message"
        };
        let mut context = crate::audit::Context::new(id, trigger);
        if let Some(cp) = checkpoint.as_ref() {
            context = context.with_run_id(&cp.record.id);
        }
        let span = crate::audit::Span::start(
            self.journal.as_ref(),
            &context,
            "run",
            None,
            "agent",
            serde_json::json!({}),
        )
        .await?;
        let deadline = checkpoint.as_ref().and_then(|cp| cp.record.deadline_at);
        let result = {
            let work = crate::audit::scope(
                context,
                self.run_loop(
                    session,
                    input,
                    progress,
                    incoming,
                    call_budget,
                    checkpoint.as_deref_mut(),
                ),
            );
            if let Some(deadline) = deadline {
                match (deadline - chrono::Utc::now()).to_std() {
                    Ok(left) => tokio::time::timeout(left, work).await.unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "Wakeup execution deadline exceeded; tool outcomes may be unknown"
                        ))
                    }),
                    Err(_) => Err(anyhow::anyhow!("Wakeup execution deadline exceeded")),
                }
            } else {
                work.await
            }
        };
        span.finish(
            if result.is_ok() { "ok" } else { "failed" },
            serde_json::json!({"error": result.is_err()}),
        )
        .await?;
        if result.is_ok()
            && let Some(cp) = checkpoint
        {
            if let Ok(reply) = super::tools::telegram_reply::REPLY_CONTEXT.try_with(Clone::clone) {
                reply.hold_final_delivery(&cp.store, &cp.record.id).await?;
            }
            cp.save(session, RunPhase::Completed, None).await?;
        }
        result
    }

    async fn run_loop(
        &self,
        session: &mut Session,
        input: Option<UserMessage>,
        progress: Option<&mpsc::Sender<AgentEvent>>,
        mut incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
        call_budget: Option<usize>,
        mut checkpoint: Option<&mut Checkpoint>,
    ) -> anyhow::Result<AssistantMessage> {
        // Dropping this sender also stops children when the parent future is cancelled.
        let (_children, child_lifetime) = tokio::sync::watch::channel(false);
        let audit_context = crate::audit::Context::current_or(session.id().as_str(), "message");
        if let Some(mut input) = input {
            self.prepare_input(session, &mut input).await?;
            self.context.initialize_session(session)?;
            session.push(Message::User(input));
        }
        let tools = self.tools.list();
        let mut requests = 0;
        let mut overflow_retry_used = checkpoint
            .as_ref()
            .is_some_and(|cp| cp.record.overflow_retry_used);
        let mut overflow_compaction_from = checkpoint
            .as_ref()
            .and_then(|cp| cp.record.overflow_compaction_from);

        loop {
            let mut input_consumed = false;
            if let Some(cp) = checkpoint.as_deref_mut()
                && cp.record.pending_calls.is_empty()
            {
                for record in cp.store.steering(&cp.record).await? {
                    let message = record.request.message;
                    let mut input = UserMessage {
                        content: message.text,
                        files: message.files,
                        images: message.images,
                    };
                    self.prepare_input(session, &mut input).await?;
                    session.push(Message::User(input));
                    cp.record.submission_ids.push(record.id);
                    input_consumed = true;
                }
            }
            if let Some(incoming) = incoming.as_mut() {
                while let Ok(mut input) = incoming.try_recv() {
                    if let Some(sandbox) = &self.sandbox {
                        sandbox.prepare_input(&mut input).await?;
                    }
                    session.push(Message::User(input));
                    input_consumed = true;
                }
            }
            if input_consumed && let Some(tx) = progress {
                let _ = tx.send(AgentEvent::InputConsumed).await;
            }
            let messages = if let Some(cp) = checkpoint
                .as_ref()
                .filter(|cp| !cp.record.pending_calls.is_empty())
            {
                cp.record
                    .pending_calls
                    .iter()
                    .cloned()
                    .map(Message::ToolCall)
                    .collect::<Vec<_>>()
            } else {
                if let Some(from) = overflow_compaction_from {
                    anyhow::ensure!(
                        self.limits.is_none()
                            && call_budget.is_none()
                            && checkpoint
                                .as_ref()
                                .is_none_or(|cp| cp.record.model_limit.is_none()),
                        "Context overflow: automatic recovery unavailable for bounded runs; reduce input"
                    );
                    let compactor = self.compactor.as_ref().filter(|c| c.threshold != 0)
                        .ok_or_else(|| anyhow::anyhow!("Context overflow: compaction disabled; reduce input or start a new session"))?;
                    // An applied durable candidate can survive a crash before this checkpoint.
                    if session.compaction().map_or(0, |(through, _)| through) <= from {
                        if let Some(cp) = checkpoint.as_deref_mut() {
                            cp.save(session, RunPhase::NeedModel, None).await?;
                            Box::pin(compactor.force_durable(session, &self.context, &tools, cp))
                                .await?;
                        } else {
                            Box::pin(compactor.force(session, &self.context, &tools)).await?;
                        }
                    }
                    overflow_compaction_from = None;
                    if let Some(cp) = checkpoint.as_deref_mut() {
                        cp.record.overflow_compaction_from = None;
                        cp.save(session, RunPhase::NeedModel, None).await?;
                    }
                }
                if !overflow_retry_used
                    && self.limits.is_none()
                    && call_budget.is_none()
                    && checkpoint
                        .as_ref()
                        .is_none_or(|cp| cp.record.model_limit.is_none())
                    && let Some(compactor) = &self.compactor
                {
                    if let Some(cp) = checkpoint.as_deref_mut() {
                        cp.save(session, RunPhase::NeedModel, None).await?;
                        Box::pin(compactor.durable_boundary(
                            session,
                            &self.context,
                            &tools,
                            cp,
                            self.compact_background_margin,
                        ))
                        .await?;
                    } else {
                        Box::pin(compactor.compact_if_needed(
                            session,
                            &self.context,
                            &tools,
                            progress,
                        ))
                        .await?;
                    }
                }
                if let Some(cp) = checkpoint.as_deref_mut() {
                    if let Some(at) = cp.record.next_retry_at {
                        if let Ok(delay) = (at - chrono::Utc::now()).to_std() {
                            tokio::time::sleep(delay).await;
                        }
                        cp.record.next_retry_at = None;
                    }
                    anyhow::ensure!(
                        cp.record
                            .model_limit
                            .is_none_or(|limit| cp.record.model_attempts < limit),
                        "Wakeup request budget exhausted"
                    );
                    cp.record.model_attempts += 1;
                    cp.save(session, RunPhase::NeedModel, None).await?;
                }
                let context = self.context.build(session);
                if let Some(budget) = call_budget {
                    anyhow::ensure!(requests < budget, "Wakeup request budget exhausted");
                }
                if let Some((max_requests, max_chars)) = self.limits {
                    anyhow::ensure!(
                        requests < max_requests,
                        "Background request budget exhausted"
                    );
                    if let Some(max_chars) = max_chars {
                        let size = context.instructions.chars().count()
                            + serde_json::to_string(&context.messages)?.chars().count();
                        anyhow::ensure!(size <= max_chars, "Background input budget exhausted");
                    }
                }
                requests += 1;
                let (tx, rx) = mpsc::channel(100);
                let model_progress = if checkpoint.is_some() {
                    progress.map(|_| &tx)
                } else {
                    progress
                };
                // Keep the shared run future small enough for gateway callers' stacks.
                let generation = Box::pin(async {
                    tokio::select! {
                        biased;
                        _ = async {
                            if let Some(incoming) = incoming.as_ref() {
                                loop {
                                    // Leave input queued so stop/suspend can still save it during cleanup.
                                    if !incoming.is_empty() {
                                        return;
                                    }
                                    if incoming.is_closed() {
                                        break;
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                }
                            }
                            std::future::pending::<()>().await
                        } => Err(GenerationSuperseded.into()),
                        steering = async {
                            if let Some(cp) = checkpoint.as_ref() {
                                loop {
                                    if !cp.store.steering(&cp.record).await?.is_empty() {
                                        return Ok::<_, anyhow::Error>(());
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                }
                            }
                            std::future::pending().await
                        } => {
                            steering?;
                            Err(GenerationSuperseded.into())
                        },
                        result = async {
                            if checkpoint.is_some() {
                                self.llm.generate_attempt(&context, &tools, model_progress).await
                            } else {
                                self.llm.generate(&context, &tools, model_progress).await
                            }
                        } => result,
                    }
                });
                let generated = if let Some(cp) = checkpoint.as_ref() {
                    super::run_view::collect_generation(
                        &cp.store, &cp.record, progress, rx, generation,
                    )
                    .await
                } else {
                    generation.await
                };
                if generated
                    .as_ref()
                    .is_err_and(|error| error.is::<GenerationSuperseded>())
                {
                    continue;
                }
                // Input can win the race after the provider finishes, before its output is committed.
                if generated.is_ok()
                    && (if let Some(cp) = checkpoint.as_ref() {
                        !cp.store.steering(&cp.record).await?.is_empty()
                    } else {
                        false
                    } || incoming.as_ref().is_some_and(|rx| !rx.is_empty()))
                {
                    if let Some(cp) = checkpoint.as_ref() {
                        cp.store.clear_live(&cp.record.id).await?;
                    }
                    continue;
                }
                let (messages, input_tokens) = match generated {
                    Ok(output) => {
                        overflow_retry_used = false;
                        if let Some(cp) = checkpoint.as_deref_mut() {
                            cp.record.model_retries = 0;
                            cp.record.overflow_retry_used = false;
                        }
                        output
                    }
                    Err(error) => {
                        if error.is::<super::llm::ContextOverflow>() {
                            anyhow::ensure!(
                                !overflow_retry_used,
                                "Context overflow after one recovery attempt; reduce input or start a new session"
                            );
                            overflow_retry_used = true;
                            overflow_compaction_from =
                                Some(session.compaction().map_or(0, |(through, _)| through));
                            if let Some(cp) = checkpoint.as_deref_mut() {
                                cp.record.overflow_retry_used = true;
                                cp.record.overflow_compaction_from = overflow_compaction_from;
                                cp.record.model_retries = 0;
                                cp.record.next_retry_at = None;
                                cp.save(session, RunPhase::NeedModel, None).await?;
                            }
                            continue;
                        }
                        if let Some(cp) = checkpoint.as_deref_mut()
                            && let Some(retry) = error.downcast_ref::<super::llm::GenerationRetry>()
                            && cp.record.model_retries < retry.limit
                        {
                            let delay = retry.retry_after.unwrap_or_else(|| {
                                std::time::Duration::from_secs(
                                    1u64 << cp.record.model_retries.min(20),
                                )
                            });
                            cp.record.model_retries += 1;
                            cp.record.next_retry_at =
                                Some(chrono::Utc::now() + chrono::Duration::from_std(delay)?);
                            cp.save(session, RunPhase::NeedModel, None).await?;
                            if let Some(tx) = progress {
                                let _ = tx.try_send(AgentEvent::Retrying(format!(
                                    "Request failed. Retry {}/{} in {} sec",
                                    cp.record.model_retries,
                                    retry.limit,
                                    delay.as_secs()
                                )));
                            }
                            continue;
                        }
                        return Err(error);
                    }
                };
                session.input_tokens = input_tokens;
                if let Some(tx) = progress {
                    let _ = tx.try_send(AgentEvent::ContextTokens(input_tokens));
                }
                let has_tool_calls = messages
                    .iter()
                    .any(|message| matches!(message, Message::ToolCall(_)));

                anyhow::ensure!(
                    messages
                        .iter()
                        .all(|m| matches!(m, Message::Assistant(_) | Message::ToolCall(_))),
                    "Unexpected message from LLM"
                );
                let calls: Vec<_> = messages
                    .iter()
                    .filter_map(|m| {
                        if let Message::ToolCall(c) = m {
                            Some(c.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                let mut ids = std::collections::HashSet::new();
                for call in &calls {
                    anyhow::ensure!(
                    !call.call_id.is_empty()
                        && ids.insert(&call.call_id)
                        && !session.messages().iter().any(
                            |m| matches!(m, Message::ToolCall(old) if old.call_id == call.call_id)
                        ),
                    "Duplicate or empty tool call ID"
                );
                }
                for message in &messages {
                    session.push(message.clone());
                }
                if has_tool_calls && let Some(cp) = checkpoint.as_deref_mut() {
                    cp.record.pending_calls = calls.clone();
                    cp.save(
                        session,
                        RunPhase::NeedTool {
                            call_id: calls[0].call_id.clone(),
                        },
                        None,
                    )
                    .await?;
                }
                messages
            };
            let has_tool_calls = messages.iter().any(|m| matches!(m, Message::ToolCall(_)));
            for message in &messages {
                if has_tool_calls
                    && let Message::Assistant(message) = message
                    && !message.content.trim().is_empty()
                    && let Some(tx) = progress
                {
                    let _ = tx.try_send(AgentEvent::Message(message.clone()));
                }
            }

            let mut answer = None;

            for message in messages {
                match message {
                    Message::Assistant(message) => answer = Some(message),
                    Message::ToolCall(call) => {
                        let replay = self.tools.replay_policy(&call.name);
                        if let Some(cp) = checkpoint.as_deref_mut() {
                            cp.save(
                                session,
                                RunPhase::ExecutingTool {
                                    call_id: call.call_id.clone(),
                                    replay,
                                },
                                Some(ToolExecution {
                                    call: call.clone(),
                                    replay,
                                    result: None,
                                }),
                            )
                            .await?;
                        }
                        let name = if tools.iter().any(|tool| tool.name() == call.name) {
                            call.name.as_str()
                        } else {
                            "unknown_tool"
                        };
                        let span = crate::audit::Span::start(
                            self.journal.as_ref(), &audit_context, "tool", Some(&call.call_id), name,
                            serde_json::json!({"arguments_bytes": serde_json::to_vec(&call.arguments)?.len()}),
                        ).await?;
                        if let Some(tx) = progress {
                            let _ = tx.try_send(AgentEvent::ToolStarted {
                                call_id: call.call_id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            });
                        }

                        let identity = checkpoint
                            .as_ref()
                            .map(|cp| (cp.record.id.clone(), call.call_id.clone()));
                        let delivery = checkpoint
                            .as_ref()
                            .filter(|cp| {
                                cp.record.conversation_id.rsplit_once(':').is_some_and(
                                    |(interface, chat)| {
                                        interface.starts_with("telegram:")
                                            && chat.parse::<i64>().is_ok()
                                    },
                                )
                            })
                            .map(|cp| {
                                (
                                    cp.store.clone(),
                                    cp.record.id.clone(),
                                    cp.record.conversation_id.clone(),
                                )
                            });
                        let result = super::delivery::CURRENT_DELIVERY
                            .scope(
                                delivery,
                                super::run_state::CURRENT_TOOL.scope(
                                    identity,
                                    super::subagents::CURRENT_PARENT.scope(
                                        matches!(
                                            call.name.as_str(),
                                            "spawn_agent"
                                                | "send_message"
                                                | "list_agents"
                                                | "wait_agent"
                                                | "stop_agent"
                                        )
                                        .then(|| {
                                            super::subagents::ParentContext {
                                                session: session.clone(),
                                                finished: child_lifetime.clone(),
                                            }
                                        }),
                                        self.tools.execute(&call.name, call.arguments.clone()),
                                    ),
                                ),
                            )
                            .await;
                        if result
                            .as_ref()
                            .is_err_and(|error| error.is::<super::delivery::StorageFailure>())
                        {
                            return Err(result.unwrap_err());
                        }
                        let failed = result.is_err();
                        let output = match result {
                            Ok(output) => output,
                            Err(error) => serde_json::json!({
                                "error": format!("{error:#}")
                            })
                            .into(),
                        };

                        let shell_failed = call.name == "exec_command"
                            && !failed
                            && (output.output["timed_out"] == true
                                || output.output["exit_code"].as_i64() != Some(0));
                        let details = if call.name == "exec_command" && !failed {
                            serde_json::json!({
                                "exit_code": output.output["exit_code"].as_i64(),
                                "timed_out": output.output["timed_out"].as_bool(),
                                "stdout_bytes": output.output["stdout"].as_str().map(str::len),
                                "stderr_bytes": output.output["stderr"].as_str().map(str::len),
                            })
                        } else {
                            serde_json::json!({"error": failed})
                        };
                        // Preserve the actual tool result even if journaling fails afterwards.
                        session.push(Message::ToolResult(ToolResult {
                            call_id: call.call_id.clone(),
                            content: output.clone(),
                        }));
                        if let Some(cp) = checkpoint.as_deref_mut() {
                            cp.record.pending_calls.remove(0);
                            let phase = cp.record.pending_calls.first().map_or(
                                RunPhase::NeedModel,
                                |next| RunPhase::NeedTool {
                                    call_id: next.call_id.clone(),
                                },
                            );
                            cp.save(
                                session,
                                phase,
                                Some(ToolExecution {
                                    call: call.clone(),
                                    replay,
                                    result: Some(output.clone()),
                                }),
                            )
                            .await?;
                        }
                        span.finish(
                            if failed || shell_failed {
                                "failed"
                            } else {
                                "ok"
                            },
                            details,
                        )
                        .await?;

                        if !failed
                            && call.name == "update_plan"
                            && let Some(tx) = progress
                        {
                            let plan = TaskPlan::parse(call.arguments.clone())?;
                            let _ = tx.try_send(AgentEvent::PlanUpdated(plan));
                        }
                        if let Some(tx) = progress {
                            let _ = tx.try_send(AgentEvent::ToolFinished {
                                call_id: call.call_id.clone(),
                                failed,
                                output: output.output.clone(),
                            });
                        }
                    }
                    Message::User(_) | Message::ToolResult(_) => {
                        anyhow::bail!("Unexpected message from LLM");
                    }
                }
            }

            if !has_tool_calls {
                let answer = answer.ok_or_else(|| anyhow::anyhow!("LLM returned no answer"))?;
                return Ok(answer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        LlmContext,
        message::ToolCall,
        session::SessionId,
        tools::{Tool, UpdatePlanTool},
    };

    struct SteeringModel {
        contexts: mpsc::UnboundedSender<Vec<Message>>,
        cancelled: mpsc::UnboundedSender<()>,
        stream: bool,
    }

    struct PendingGeneration(mpsc::UnboundedSender<()>);

    impl Drop for PendingGeneration {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for SteeringModel {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            progress: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            self.contexts.send(context.messages.to_vec()).unwrap();
            if !matches!(context.messages.last(), Some(Message::User(u)) if u.content == "last correction")
            {
                let _pending = PendingGeneration(self.cancelled.clone());
                if self.stream {
                    progress
                        .unwrap()
                        .send(AgentEvent::TextDelta("obsolete draft".into()))
                        .await
                        .unwrap();
                }
                std::future::pending::<()>().await;
            }
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "final".into(),
                })],
                Some(7),
            ))
        }
    }

    async fn assert_steering_restarts(durable: bool, stream: bool) {
        use crate::agent::{
            inbox::{BusyMode, SubmissionRequest},
            session::SqliteSessionStore,
        };
        use crate::gateway::{IncomingMessage, InterfaceId};

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let dir = tempfile::tempdir().unwrap();
            let store = SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap();
            let session = Session::new(SessionId::new("test:steer"));
            let id = session.id().clone();
            let mut checkpoint = durable.then(|| Checkpoint::new(&store, &id, &session));
            if let Some(cp) = &mut checkpoint {
                cp.record.isolated = Some(false);
            }
            let (contexts, mut seen) = mpsc::unbounded_channel();
            let (cancelled, mut cancellations) = mpsc::unbounded_channel();
            let agent = Agent::new(
                Arc::new(SteeringModel {
                    contexts,
                    cancelled,
                    stream,
                }),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            );
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            let (progress, mut events) = mpsc::channel(100);
            let task = tokio::spawn(async move {
                let mut session = session;
                let result = agent
                    .run_durable(
                        &mut session,
                        UserMessage {
                            content: "initial".into(),
                            files: vec![],
                            images: vec![],
                        },
                        Some(&progress),
                        Some(&mut input_rx),
                        checkpoint.as_mut(),
                    )
                    .await;
                (result, session, checkpoint)
            });
            seen.recv().await.unwrap();
            if durable && stream {
                loop {
                    let view = store.conversation_view(id.as_str()).await.unwrap();
                    if view
                        .run
                        .as_ref()
                        .and_then(|v| v.live.current.as_ref())
                        .is_some_and(|p| p.text == "obsolete draft")
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            let file = crate::agent::FileAttachment::save(
                &dir.path().join("files"),
                "note.txt",
                None,
                b"correction attachment",
            )
            .unwrap();
            for text in ["first correction", "last correction"] {
                if durable {
                    let mut message =
                        IncomingMessage::new(InterfaceId("test".into()), "steer", text);
                    message.files.push(file.clone());
                    store
                        .submit(SubmissionRequest::new(&id, message, BusyMode::Steer))
                        .await
                        .unwrap();
                } else {
                    input_tx
                        .send(UserMessage {
                            content: text.into(),
                            files: vec![file.clone()],
                            images: vec![],
                        })
                        .unwrap();
                }
                let context = tokio::time::timeout(std::time::Duration::from_secs(1), seen.recv())
                    .await
                    .expect("new input must restart the pending model request")
                    .unwrap();
                assert!(!context.iter().any(|m| matches!(m, Message::Assistant(_))));
                let inputs: Vec<_> = context
                    .iter()
                    .filter_map(|m| {
                        if let Message::User(u) = m {
                            Some(u.content.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(
                    inputs,
                    if text == "first correction" {
                        vec!["initial", "first correction"]
                    } else {
                        vec!["initial", "first correction", "last correction"]
                    }
                );
                cancellations
                    .recv()
                    .await
                    .expect("superseded model future must be dropped");
            }
            let (result, session, checkpoint) = task.await.unwrap();
            assert_eq!(result.unwrap().content, "final");
            assert_eq!(session.messages().len(), 4);
            for message in &session.messages()[1..3] {
                let Message::User(user) = message else {
                    panic!("correction lost")
                };
                assert_eq!(user.files[0].path, file.path);
            }
            assert_eq!(session.input_tokens, Some(7));
            let mut consumed = 0;
            while let Ok(event) = events.try_recv() {
                if matches!(event, AgentEvent::InputConsumed) {
                    consumed += 1;
                }
            }
            assert_eq!(consumed, 2);
            if let Some(cp) = checkpoint {
                let view = store.run_view(&cp.record.id).await.unwrap();
                assert!(
                    view.live.interrupted.is_none(),
                    "superseded draft must be deleted"
                );
                assert!(view.live.current.as_ref().is_none_or(|p| p.text.is_empty()));
                assert_eq!(view.session.messages().len(), 4);
                assert_eq!(cp.record.model_attempts, 3);
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn steering_restarts_in_memory_before_tokens_and_during_stream() {
        for stream in [false, true] {
            assert_steering_restarts(false, stream).await;
        }
    }

    #[tokio::test]
    async fn steering_restarts_durable_before_tokens_and_during_stream() {
        for stream in [false, true] {
            assert_steering_restarts(true, stream).await;
        }
    }

    #[tokio::test]
    async fn steering_restarts_preserves_input_when_stopped_during_live_cleanup() {
        use crate::{
            agent::{AgentRunner, session::SqliteSessionStore},
            gateway::{IncomingMessage, InterfaceId},
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(SqliteSessionStore::open_async(directory.path().into()).await.unwrap());
            let (contexts, mut seen) = mpsc::unbounded_channel();
            let (cancelled, mut cancellations) = mpsc::unbounded_channel();
            let runner = Arc::new(AgentRunner::new(Arc::new(Agent::new(Arc::new(SteeringModel { contexts, cancelled, stream: false }), ToolRegistry::default(), ContextBuilder::new("test"))), store.clone()));
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let worker = runner.clone();
            let task = tokio::spawn(async move {
                worker.run_with_cancel(SessionId::new("test:stop"), IncomingMessage::new(InterfaceId("test".into()), "stop", "initial"), None, Some(stopped), Some(&mut input_rx)).await
            });
            seen.recv().await.unwrap();
            let locked = Arc::new(tokio::sync::Notify::new());
            let acquired = locked.clone();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let database = tokio::spawn(async move {
                store.with_connection(move |_| { acquired.notify_one(); let _ = released.recv(); Ok(()) }).await
            });
            locked.notified().await;
            input_tx.send(UserMessage { content: "accepted correction".into(), files: vec![], images: vec![] }).unwrap();
            cancellations.recv().await.unwrap();
            stop.send(()).unwrap();
            release.send(()).unwrap();
            database.await.unwrap().unwrap();
            assert_eq!(task.await.unwrap().unwrap_err().to_string(), "Stopped by user");
            let session = runner.load_session(SessionId::new("test:stop")).await.unwrap();
            assert_eq!(session.messages().len(), 2, "accepted input must survive cancellation during cleanup");
            assert!(matches!(&session.messages()[1], Message::User(u) if u.content == "accepted correction"));
        }).await.unwrap();
    }

    struct OverflowModel {
        calls: std::sync::atomic::AtomicUsize,
        summaries: std::sync::atomic::AtomicUsize,
        always: bool,
    }
    #[async_trait::async_trait]
    impl LlmProvider for OverflowModel {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            use std::sync::atomic::Ordering::SeqCst;
            if context
                .instructions
                .starts_with("You are a conversation summarizer")
            {
                self.summaries.fetch_add(1, SeqCst);
                return Ok((
                    vec![Message::Assistant(AssistantMessage {
                        content: "summary".into(),
                    })],
                    Some(42),
                ));
            }
            let n = self.calls.fetch_add(1, SeqCst);
            if n == 0 || self.always {
                return Err(super::super::llm::ContextOverflow.into());
            }
            assert!(
                matches!(&context.messages[0], Message::Assistant(m) if m.content.contains("summary"))
            );
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })],
                Some(7),
            ))
        }
    }
    #[tokio::test]
    async fn overflow_compacts_once_then_retries_once() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        for always in [false, true] {
            let model = Arc::new(OverflowModel {
                calls: AtomicUsize::new(0),
                summaries: AtomicUsize::new(0),
                always,
            });
            let agent = Agent::new(
                model.clone(),
                ToolRegistry::default(),
                ContextBuilder::new("rules"),
            )
            .with_compaction(model.clone(), 80000);
            let mut session = Session::new(SessionId::new("cli:overflow"));
            session.push(Message::User(UserMessage {
                content: "old ".repeat(1000),
                images: vec![],
                files: vec![],
            }));
            let result = agent
                .run(
                    &mut session,
                    UserMessage {
                        content: "next".into(),
                        images: vec![],
                        files: vec![],
                    },
                    None,
                    None,
                )
                .await;
            assert_eq!(result.is_ok(), !always, "{result:?}");
            assert_eq!(model.calls.load(SeqCst), 2);
            assert_eq!(model.summaries.load(SeqCst), 1);
            assert!(session.compaction().is_some());
            assert_eq!(session.messages().len(), if always { 2 } else { 3 });
            if !always {
                assert_eq!(session.input_tokens, Some(7));
            }
        }
    }

    fn overflow_history() -> Session {
        let mut session = Session::new(SessionId::new("cli:overflow"));
        session.push(Message::User(UserMessage {
            content: "old ".repeat(1000),
            images: vec![],
            files: vec![],
        }));
        session.push(Message::User(UserMessage {
            content: "next".into(),
            images: vec![],
            files: vec![],
        }));
        session
    }

    #[tokio::test]
    async fn overflow_respects_limits_and_attempts_oversized_groups() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        for mode in ["disabled", "bounded", "budget", "single", "oversized"] {
            let model = Arc::new(OverflowModel {
                calls: AtomicUsize::new(0),
                summaries: AtomicUsize::new(0),
                always: true,
            });
            let mut agent = Agent::new(
                model.clone(),
                ToolRegistry::default(),
                ContextBuilder::new("rules"),
            )
            .with_compaction(model.clone(), if mode == "disabled" { 0 } else { 80000 });
            if mode == "bounded" {
                agent = agent.with_limits(10, 100000);
            }
            let mut session = if mode == "single" {
                Session::new(SessionId::new("single"))
            } else {
                overflow_history()
            };
            if mode == "oversized" {
                session = Session::new(SessionId::new("oversized"));
                session.push(Message::User(UserMessage {
                    content: "x".repeat(125000),
                    images: vec![],
                    files: vec![],
                }));
            }
            let input = UserMessage {
                content: "latest".into(),
                images: vec![],
                files: vec![],
            };
            let result = if mode == "budget" {
                agent.run_bounded(&mut session, input, 10).await
            } else {
                agent.run(&mut session, input, None, None).await
            };
            assert!(result.is_err(), "{mode}");
            let summarized = mode == "oversized";
            assert_eq!(
                model.calls.load(SeqCst),
                if summarized { 2 } else { 1 },
                "{mode}"
            );
            assert_eq!(
                model.summaries.load(SeqCst),
                usize::from(summarized),
                "{mode}"
            );
            assert_eq!(session.compaction().is_some(), summarized, "{mode}");
        }
    }

    #[tokio::test]
    async fn overflow_durable_accounts_summary_separately() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let dir = tempfile::tempdir().unwrap();
        let store = super::super::session::SqliteSessionStore::open(dir.path().into()).unwrap();
        let mut session = overflow_history();
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        let model = Arc::new(OverflowModel {
            calls: AtomicUsize::new(0),
            summaries: AtomicUsize::new(0),
            always: false,
        });
        let agent = Agent::new(
            model.clone(),
            ToolRegistry::default(),
            ContextBuilder::new("rules"),
        )
        .with_compaction(model.clone(), 80000);
        agent
            .run_inner(&mut session, None, None, None, None, Some(&mut cp))
            .await
            .unwrap();
        assert_eq!(model.calls.load(SeqCst), 2);
        assert_eq!(model.summaries.load(SeqCst), 1);
        assert_eq!(cp.record.model_attempts, 2);
        assert!(!cp.record.overflow_retry_used);
        assert_eq!(session.input_tokens, Some(7));
        let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
        let payload: Vec<u8> = db
            .query_row("SELECT payload FROM compaction_jobs", [], |r| r.get(0))
            .unwrap();
        let job: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(job["known_input_tokens"], 42);
        assert_eq!(job["model_calls"], 1);
    }

    #[tokio::test]
    async fn overflow_preserves_steering_before_forced_compaction() {
        use super::super::{
            inbox::{BusyMode, SubmissionRequest},
            session::SqliteSessionStore,
        };
        use crate::gateway::{IncomingMessage, InterfaceId};
        use std::sync::atomic::AtomicUsize;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let mut session = overflow_history();
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.isolated = Some(false);
        cp.record.overflow_retry_used = true;
        cp.record.overflow_compaction_from = Some(0);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        let steer = store
            .submit(SubmissionRequest::new(
                session.id(),
                IncomingMessage::new(InterfaceId("test".into()), "chat", "new correction"),
                BusyMode::Steer,
            ))
            .await
            .unwrap();
        let model = Arc::new(OverflowModel {
            calls: AtomicUsize::new(1),
            summaries: AtomicUsize::new(0),
            always: false,
        });
        let agent = Agent::new(
            model.clone(),
            ToolRegistry::default(),
            ContextBuilder::new("rules"),
        )
        .with_compaction(model, 80000);
        agent
            .run_inner(&mut session, None, None, None, None, Some(&mut cp))
            .await
            .unwrap();
        assert!(cp.record.submission_ids.contains(&steer.id));
        assert!(
            session
                .messages()
                .iter()
                .any(|m| matches!(m,Message::User(u) if u.content=="new correction"))
        );
        let (_, saved) = store.load_run(&cp.record.id).await.unwrap().unwrap();
        assert!(
            saved
                .messages()
                .iter()
                .any(|m| matches!(m,Message::User(u) if u.content=="new correction"))
        );
    }

    #[tokio::test]
    async fn overflow_restart_preserves_retry_and_applied_summary() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        for pending in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let store = super::super::session::SqliteSessionStore::open(dir.path().into()).unwrap();
            let mut session = overflow_history();
            let mut cp = Checkpoint::new(&store, session.id(), &session);
            cp.record.overflow_retry_used = true;
            cp.record.overflow_compaction_from = Some(0);
            cp.record.model_attempts = 1;
            cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
            let model = Arc::new(OverflowModel {
                calls: AtomicUsize::new(0),
                summaries: AtomicUsize::new(0),
                always: true,
            });
            let agent = Agent::new(
                model.clone(),
                ToolRegistry::default(),
                ContextBuilder::new("rules"),
            )
            .with_compaction(model.clone(), 80000);
            agent
                .compactor
                .as_ref()
                .unwrap()
                .force_durable(&mut session, &agent.context, &[], &mut cp)
                .await
                .unwrap();
            if !pending {
                cp.record.overflow_compaction_from = None;
                cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
            }
            let id = cp.record.id.clone();
            drop(cp);
            drop(store);
            drop(session);
            let store = super::super::session::SqliteSessionStore::open(dir.path().into()).unwrap();
            let (record, mut session) = store.load_run(&id).await.unwrap().unwrap();
            let mut cp = Checkpoint::new(&store, session.id(), &session);
            cp.record = record;
            let error = agent
                .run_inner(&mut session, None, None, None, None, Some(&mut cp))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("after one recovery attempt"),
                "{error:#}"
            );
            assert_eq!(model.summaries.load(SeqCst), 1);
            assert_eq!(model.calls.load(SeqCst), 1);
            assert_eq!(cp.record.model_attempts, 2);
            assert!(cp.record.overflow_retry_used);
        }
    }

    #[tokio::test]
    async fn auto_compacts_between_tool_steps_without_changing_full_history() {
        use std::sync::Mutex;
        struct FlowLlm(Mutex<Vec<Vec<Message>>>);
        #[async_trait::async_trait]
        impl LlmProvider for FlowLlm {
            async fn generate(
                &self,
                context: &LlmContext<'_>,
                tools: &[&dyn Tool],
                progress: Option<&mpsc::Sender<AgentEvent>>,
            ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
                if context
                    .instructions
                    .starts_with("You are a conversation summarizer")
                {
                    assert!(tools.is_empty() && progress.is_none());
                    return Ok((
                        vec![Message::Assistant(AssistantMessage {
                            content: "Preserved user goal".into(),
                        })],
                        Some(2000),
                    ));
                }
                self.0.lock().unwrap().push(context.messages.to_vec());
                let reply = if context
                    .messages
                    .iter()
                    .any(|m| matches!(m, Message::ToolResult(_)))
                {
                    Message::Assistant(AssistantMessage {
                        content: "done".into(),
                    })
                } else {
                    Message::ToolCall(ToolCall {
                        call_id: "work".into(),
                        name: "large_output".into(),
                        arguments: serde_json::json!({}),
                    })
                };
                Ok((vec![reply], Some(9000)))
            }
        }
        struct LargeOutput;
        #[async_trait::async_trait]
        impl Tool for LargeOutput {
            fn name(&self) -> String {
                "large_output".into()
            }
            fn description(&self) -> String {
                "Fixture".into()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            async fn execute(
                &self,
                _: serde_json::Value,
            ) -> anyhow::Result<crate::agent::message::ToolOutput> {
                Ok(serde_json::json!("output ".repeat(900)).into())
            }
        }
        let llm = Arc::new(FlowLlm(Mutex::new(vec![])));
        let agent = Agent::new(
            llm.clone(),
            ToolRegistry::default().register(Arc::new(LargeOutput)),
            ContextBuilder::new("rules"),
        )
        .with_compaction(llm.clone(), 8000);
        let mut session = Session::new(SessionId::new("flow"));
        let (tx, mut rx) = mpsc::channel(32);
        let answer = agent
            .run(
                &mut session,
                UserMessage {
                    content: "user goal ".repeat(600),
                    images: vec![],
                    files: vec![],
                },
                Some(&tx),
                None,
            )
            .await
            .unwrap();
        assert_eq!(answer.content, "done");
        assert_eq!(session.messages().len(), 4);
        assert!(matches!(&session.messages()[0], Message::User(m) if m.content.len() == 6000));
        let seen = llm.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            matches!(&seen[1][0], Message::Assistant(m) if m.content.contains("Preserved user goal"))
        );
        assert!(matches!(&seen[1][1], Message::ToolCall(c) if c.call_id == "work"));
        assert!(matches!(&seen[1][2], Message::ToolResult(r) if r.call_id == "work"));
        let mut notices = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, AgentEvent::Compaction(_)) {
                notices += 1;
            }
        }
        assert_eq!(notices, 2);
    }

    struct AuditLlm {
        command: String,
        block_journal: Option<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for AuditLlm {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            if let Some(path) = &self.block_journal {
                std::fs::rename(path, path.with_extension("saved"))?;
                std::fs::create_dir(path)?;
            }
            let message = if context
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult(_)))
            {
                Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })
            } else {
                Message::ToolCall(ToolCall {
                    call_id: "audit-call".into(),
                    name: "exec_command".into(),
                    arguments: serde_json::json!({"cmd": self.command, "workdir": null, "timeout_ms": 1000}),
                })
            };
            Ok((vec![message], None))
        }
    }

    #[tokio::test]
    async fn audit_records_background_shell_failure_without_sensitive_text() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::audit::Journal::new(dir.path());
        let agent = Agent::new(
            Arc::new(AuditLlm {
                command: "printf SUPER_SECRET; exit 7".into(),
                block_journal: None,
            }),
            ToolRegistry::default().register(Arc::new(crate::agent::tools::ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_journal(journal);
        let mut session = Session::new(SessionId::new("companion:wakeup:audit"));
        agent
            .run_bounded(
                &mut session,
                UserMessage {
                    content: "PRIVATE_PROMPT".into(),
                    files: vec![],
                    images: vec![],
                },
                2,
            )
            .await
            .unwrap();
        let file = dir
            .path()
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        let text = std::fs::read_to_string(file).unwrap();
        assert!(!text.contains("SUPER_SECRET"));
        assert!(!text.contains("PRIVATE_PROMPT"));
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["event"], "run_started");
        assert_eq!(rows[1]["event"], "tool_started");
        assert_eq!(rows[2]["event"], "tool_finished");
        assert_eq!(rows[2]["status"], "failed");
        assert_eq!(rows[2]["details"]["exit_code"], 7);
        assert_eq!(rows[3]["event"], "run_finished");
        assert!(
            rows.iter()
                .all(|row| row["run_id"] == rows[0]["run_id"] && row["trigger"] == "wakeup")
        );
    }

    #[tokio::test]
    async fn audit_start_failure_prevents_shell_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        let marker = dir.path().join("must-not-exist");
        let agent = Agent::new(
            Arc::new(AuditLlm {
                command: format!("touch '{}'", marker.display()),
                block_journal: Some(file),
            }),
            ToolRegistry::default().register(Arc::new(crate::agent::tools::ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_journal(crate::audit::Journal::new(dir.path()));
        let mut session = Session::new(SessionId::new("audit"));
        let result = agent
            .run(
                &mut session,
                UserMessage {
                    content: "work".into(),
                    files: vec![],
                    images: vec![],
                },
                None,
                None,
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("journal"));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn audit_finish_failure_preserves_tool_result_and_prevents_followup_actions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        let saved = file.with_extension("saved");
        let agent = Agent::new(
            Arc::new(AuditLlm {
                command: format!(
                    "mv '{}' '{}'; mkdir '{}'",
                    file.display(),
                    saved.display(),
                    file.display()
                ),
                block_journal: None,
            }),
            ToolRegistry::default().register(Arc::new(crate::agent::tools::ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_journal(crate::audit::Journal::new(dir.path()));
        let mut session = Session::new(SessionId::new("audit"));
        let input = UserMessage {
            content: "work".into(),
            files: vec![],
            images: vec![],
        };
        assert!(
            agent
                .run(&mut session, input.clone(), None, None)
                .await
                .is_err()
        );
        assert!(session.messages().iter().any(|message| matches!(message,
            Message::ToolResult(result) if result.content.output["exit_code"] == 0)));
        assert_eq!(std::fs::read_to_string(&saved).unwrap().lines().count(), 2);
        std::fs::remove_dir(&file).unwrap();
        std::fs::rename(&saved, &file).unwrap();
        assert!(agent.run(&mut session, input, None, None).await.is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap().lines().count(), 2);
    }

    #[tokio::test]
    async fn audit_cancellation_does_not_invent_tool_or_run_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        let agent = Agent::new(
            Arc::new(AuditLlm {
                command: "sleep 10".into(),
                block_journal: None,
            }),
            ToolRegistry::default().register(Arc::new(crate::agent::tools::ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_journal(crate::audit::Journal::new(dir.path()));
        let task = tokio::spawn(async move {
            let mut session = Session::new(SessionId::new("audit"));
            agent
                .run(
                    &mut session,
                    UserMessage {
                        content: "work".into(),
                        files: vec![],
                        images: vec![],
                    },
                    None,
                    None,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if std::fs::read_to_string(&file).is_ok_and(|text| text.lines().count() == 2) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let text = std::fs::read_to_string(file).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(!text.contains("finished"));
    }

    struct PlanLlm;

    #[async_trait::async_trait]
    impl LlmProvider for PlanLlm {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            let count = context
                .messages
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .count();
            let message = if count < 2 {
                Message::ToolCall(ToolCall {
                    call_id: count.to_string(),
                    name: "update_plan".into(),
                    arguments: serde_json::json!({"plan": [{
                        "step": if count == 0 { "Inspect" } else { " " }, "status": "in_progress"
                    }]}),
                })
            } else {
                Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })
            };
            Ok((vec![message], None))
        }
    }

    #[tokio::test]
    async fn only_successful_plans_emit_events_and_calls_are_persisted() {
        let agent = Agent::new(
            Arc::new(PlanLlm),
            ToolRegistry::default().register(Arc::new(crate::agent::tools::UpdatePlanTool)),
            ContextBuilder::new("test"),
        );
        let mut session = Session::new(SessionId::new("plan"));
        let (tx, mut rx) = mpsc::channel(32);
        agent
            .run(
                &mut session,
                UserMessage {
                    files: Vec::new(),
                    content: "work".into(),
                    images: vec![],
                },
                Some(&tx),
                None,
            )
            .await
            .unwrap();
        let mut plans = vec![];
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::PlanUpdated(plan) = event {
                plans.push(plan);
            }
        }
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].plan[0].step, "Inspect");
        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored
                .messages()
                .iter()
                .filter(|m| matches!(m, Message::ToolCall(_)))
                .count(),
            2
        );
    }

    struct LongTaskLlm;

    #[tokio::test]
    async fn child_agents_allow_large_contexts_but_keep_the_request_budget() {
        let agent = Agent::new(
            Arc::new(LongTaskLlm),
            ToolRegistry::default().register(Arc::new(UpdatePlanTool)),
            ContextBuilder::new("x".repeat(64_001)),
        )
        .child_agent();
        let mut session = Session::new(SessionId::new("child-large-context"));
        let error = agent
            .run(
                &mut session,
                UserMessage {
                    files: vec![],
                    content: "work".into(),
                    images: vec![],
                },
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Background request budget exhausted");
        assert_eq!(
            session
                .messages()
                .iter()
                .filter(|message| matches!(message, Message::ToolResult(_)))
                .count(),
            16
        );
    }

    #[tokio::test]
    async fn wakeups_limit_calls_without_changing_foreground_agent() {
        let agent = Agent::new(
            Arc::new(LongTaskLlm),
            ToolRegistry::default().register(Arc::new(UpdatePlanTool)),
            ContextBuilder::new("test"),
        );
        let mut session = Session::new(SessionId::new("companion:wakeup:budget"));
        let error = agent
            .run_bounded(
                &mut session,
                UserMessage {
                    files: vec![],
                    content: "work".into(),
                    images: vec![],
                },
                2,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("budget exhausted"));
        assert_eq!(
            session
                .messages()
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn background_limits_stop_requests_and_oversized_contexts() {
        for (requests, chars, expected_results) in [(2, 10_000, 2), (2, 1, 0)] {
            let agent = Agent::new(
                Arc::new(LongTaskLlm),
                ToolRegistry::default().register(Arc::new(UpdatePlanTool)),
                ContextBuilder::new("test"),
            )
            .with_limits(requests, chars);
            let mut session = Session::new(SessionId::new("background"));
            let error = agent
                .run(
                    &mut session,
                    UserMessage {
                        files: Vec::new(),
                        content: "work".into(),
                        images: vec![],
                    },
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("budget exhausted"));
            assert_eq!(
                session
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, Message::ToolResult(_)))
                    .count(),
                expected_results
            );
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for LongTaskLlm {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            let completed = context
                .messages
                .iter()
                .filter(|message| matches!(message, Message::ToolResult(_)))
                .count();
            let message = if completed < 30 {
                Message::ToolCall(ToolCall {
                    call_id: completed.to_string(),
                    name: UpdatePlanTool.name(),
                    arguments: serde_json::json!({"plan": [{"step": "Work", "status": "in_progress"}]}),
                })
            } else {
                Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })
            };
            Ok((vec![message], None))
        }
    }

    #[tokio::test]
    async fn continues_past_old_request_limit_until_final_answer() {
        let agent = Agent::new(
            Arc::new(LongTaskLlm),
            ToolRegistry::default().register(Arc::new(UpdatePlanTool)),
            ContextBuilder::new("test"),
        );
        let mut session = Session::new(SessionId::new("long-task"));
        let answer = agent
            .run(
                &mut session,
                UserMessage {
                    files: Vec::new(),
                    content: "work".into(),
                    images: Vec::new(),
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(answer.content, "done");
        let results: Vec<_> = session
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 30);
        assert!(
            results
                .iter()
                .all(|result| result.content.output.get("error").is_none())
        );
    }
}
