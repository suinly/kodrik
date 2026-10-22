use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use log::{info, warn};
use tokio::sync::{mpsc, oneshot};

#[path = "telegram_spaces.rs"]
mod telegram_spaces;

#[path = "session_commands.rs"]
mod session_commands;
#[path = "session_picker.rs"]
mod session_picker;
#[path = "telegram_login.rs"]
mod telegram_login;

use crate::{
    Interface, MessageId,
    agent::{AgentEvent, AgentRunner, UserMessage, session::SessionId},
    gateway::{Event, IncomingMessage},
    i18n::Language,
};

#[derive(Clone)]
pub struct GatewayRunner {
    agent_runner: Arc<AgentRunner>,
    chat_runners: Option<HashMap<SessionId, Arc<AgentRunner>>>,
    telegram_spaces: Option<Arc<telegram_spaces::TelegramSpaces>>,
    user_spaces: Option<Arc<crate::users::UserSpaces>>,
    interfaces: Vec<Arc<dyn Interface>>,
    proactive: Option<Arc<super::proactive::Proactive>>,
    reminders: Vec<Arc<super::reminders::Reminders>>,
    wakeups: Vec<Arc<super::wakeups::Wakeups>>,
    restart: Option<Arc<super::restart::RestartCoordinator>>,
}

fn progress_text(language: Language, description: &str, elapsed: u64) -> String {
    format!(
        "{} — {}",
        inline_code(language, description),
        elapsed_text(language, elapsed)
    )
}

fn inline_code(language: Language, description: &str) -> String {
    // Code spans must stay on one line, even when tool arguments contain Markdown.
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let description = if description.is_empty() {
        language.pick("Working", "Работаю")
    } else {
        description.as_str()
    };
    let fence = "`".repeat(
        description
            .split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0)
            + 1,
    );
    format!("{fence} {description} {fence}")
}

fn with_plan(plan: Option<&str>, status: &str) -> String {
    match plan {
        Some(plan) => format!("{plan}<br><br>{status}"),
        None => status.to_owned(),
    }
}

fn elapsed_text(language: Language, elapsed: u64) -> String {
    if elapsed < 60 {
        format!("{elapsed} {}", language.pick("sec", "сек"))
    } else {
        format!(
            "{} {} {} {}",
            elapsed / 60,
            language.pick("min", "мин"),
            elapsed % 60,
            language.pick("sec", "сек")
        )
    }
}

fn service_progress(language: Language, message: &str) -> String {
    if language == Language::Ru {
        match message {
            "Compacting conversation context…" => return "Сжимаю контекст диалога…".into(),
            "Context compacted; continuing…" => return "Контекст сжат; продолжаю…".into(),
            _ => {}
        }
        if let Some(retry) = message.strip_prefix("Request failed. Retry ")
            && let Some((attempt, delay)) = retry.split_once(" in ")
            && let Some(seconds) = delay.strip_suffix(" sec")
        {
            return format!("Ошибка запроса. Повтор {attempt} через {seconds} сек");
        }
    }
    message.to_owned()
}

fn tool_description(name: &str, arguments: &serde_json::Value) -> String {
    let primary = match name {
        "exec_command" => "cmd",
        "view_image" => "path",
        "apply_patch" => "patch",
        _ => "",
    };
    let args = arguments
        .get(primary)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| arguments.to_string());
    let summary: String = args.chars().take(160).collect();
    let suffix = if args.chars().count() > 160 {
        "…"
    } else {
        ""
    };
    format!("{name}: {summary}{suffix}")
}

impl GatewayRunner {
    fn message_language(&self, message: &IncomingMessage) -> Language {
        self.interfaces
            .iter()
            .find(|interface| interface.id() == message.source.interface)
            .map(|interface| interface.language(&message.source.conversation_id))
            .unwrap_or_default()
    }

    pub fn new(agent_runner: Arc<AgentRunner>) -> Self {
        Self {
            agent_runner,
            chat_runners: None,
            telegram_spaces: None,
            user_spaces: None,
            interfaces: Vec::new(),
            proactive: None,
            reminders: Vec::new(),
            wakeups: Vec::new(),
            restart: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_chat_runners(
        mut self,
        runners: HashMap<SessionId, Arc<AgentRunner>>,
    ) -> Self {
        self.chat_runners = Some(runners);
        self
    }

    fn runner_for(&self, conversation: &SessionId) -> anyhow::Result<&Arc<AgentRunner>> {
        match &self.chat_runners {
            Some(runners) => runners
                .get(conversation)
                .ok_or_else(|| anyhow::anyhow!("No initialized space for this conversation")),
            None => Ok(&self.agent_runner),
        }
    }

    pub(crate) fn with_restart(mut self, restart: Arc<super::restart::RestartCoordinator>) -> Self {
        self.restart = Some(restart);
        self
    }

    pub fn add_interface<I>(mut self, interface: I) -> Self
    where
        I: Interface + 'static,
    {
        self.interfaces.push(Arc::new(interface));
        self
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let mut gateway = self.clone();
        let mut background = tokio::task::JoinSet::new();
        for wakeups in &self.wakeups {
            let wakeups = wakeups.clone();
            let interfaces = self.interfaces.clone();
            background.spawn(async move { wakeups.run(interfaces).await });
        }
        for reminders in &self.reminders {
            let reminders = reminders.clone();
            let interfaces = self.interfaces.clone();
            background.spawn(async move { reminders.run(interfaces).await });
        }
        if let Some(proactive) = &self.proactive {
            let proactive = proactive.clone();
            let interfaces = self.interfaces.clone();
            background.spawn(async move { proactive.run(interfaces).await });
        }
        if let Some(restart) = &self.restart {
            match restart.receipt() {
                Ok(Some(receipt)) if receipt.old_pid != std::process::id() => {
                    if let Some(interface) = self
                        .interfaces
                        .iter()
                        .find(|interface| interface.id().0 == receipt.interface)
                        .cloned()
                    {
                        let restart = restart.clone();
                        background.spawn(async move { restart.confirm(interface).await });
                    } else {
                        log::warn!("Restart confirmation interface is unavailable");
                        restart.quarantine_receipt(Some(&receipt.request_id))?;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    log::warn!("Invalid restart receipt: {error:#}");
                    restart.quarantine_receipt(None)?;
                }
            }
        }
        let (tx, mut rx) = mpsc::channel(100);

        let mut interface_tasks = tokio::task::JoinSet::new();
        let mut conversation_tasks = tokio::task::JoinSet::new();
        let mut recovery_tasks = tokio::task::JoinSet::new();
        let mut shutdown = self.agent_runner.shutdown_receiver();
        let mut suspending = false;
        let recoveries: Vec<_> = if let Some(runners) = &self.chat_runners {
            runners
                .iter()
                .map(|(id, r)| (id.clone(), r.clone()))
                .collect()
        } else {
            self.agent_runner
                .unfinished_conversations()
                .await?
                .into_iter()
                .map(|id| (id, self.agent_runner.clone()))
                .collect()
        };
        if let Some(runners) = &self.chat_runners {
            for id in self.agent_runner.unfinished_conversations().await? {
                if id.as_str().starts_with("telegram:") && !runners.contains_key(&id) {
                    log::warn!(
                        "Recovery blocked for {}: original conversation is not mapped to an authorized space",
                        id.as_str()
                    );
                }
            }
        }
        for (id, runner) in recoveries {
            let Some((interface_id, chat)) = id.as_str().rsplit_once(':') else {
                continue;
            };
            if !interface_id.starts_with("telegram:") {
                continue;
            }
            let Some(interface) = self
                .interfaces
                .iter()
                .find(|i| i.id().0 == interface_id && i.allows_recipient(chat))
                .cloned()
            else {
                log::warn!(
                    "Recovery blocked for {}: source interface or recipient unavailable",
                    id.as_str()
                );
                continue;
            };
            let chat = chat.to_owned();
            let delivery_gateway = self.clone();
            recovery_tasks.spawn(async move {
                let reply = crate::agent::tools::telegram_reply::ReplyContext::new(
                    interface.clone(),
                    chat.clone(),
                    None,
                );
                let report = crate::agent::tools::telegram_reply::REPLY_CONTEXT
                    .scope(
                        reply.clone(),
                        runner.recover(crate::agent::recovery::RecoveryTarget::Conversation(id)),
                    )
                    .await?;
                for (id, reason) in report.blocked {
                    log::warn!("Recovery blocked for {id}: {reason}");
                }
                if runner.has_inbox() {
                    drop(reply.take_final_delivery().await);
                    delivery_gateway
                        .flush_deliveries(
                            &runner,
                            interface.as_ref(),
                            &format!("{}:{}", interface.id(), chat),
                        )
                        .await?;
                } else {
                    for (_, answer) in report.completed {
                        if reply.delivered().await.is_none() {
                            interface.send(&chat, &answer.content).await?;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            });
        }
        let mut restart = self.restart.as_ref().map(|restart| restart.subscribe());

        for interface in &self.interfaces {
            let tx = tx.clone();
            let interface = Arc::clone(interface);
            let id = interface.id();

            interface_tasks.spawn(async move { interface.run(tx).await });
            info!("interface {id} started");
        }

        drop(tx);

        let mut conversations = HashMap::new();
        let mut delivery_tick = tokio::time::interval(Duration::from_secs(5));
        delivery_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *shutdown.borrow() {
                suspending = true;
                break;
            }
            tokio::select! {
                _=delivery_tick.tick()=>{
                    let jobs=if let Some(runners)=&gateway.chat_runners {runners.iter().map(|(id,runner)|(id.clone(),runner.clone())).collect::<Vec<_>>()} else {gateway.agent_runner.unfinished_conversations().await?.into_iter().map(|id|(id,gateway.agent_runner.clone())).collect()};
                    for (id,runner) in jobs {
                        let Some((interface,chat))=id.as_str().rsplit_once(':') else {continue};
                        let Some(interface)=gateway.interfaces.iter().find(|i|i.id().0==interface&&i.allows_recipient(chat)).cloned() else {continue};
                        let delivery_gateway=gateway.clone();
                        recovery_tasks.spawn(async move {delivery_gateway.flush_deliveries(&runner,interface.as_ref(),id.as_str()).await});
                    }
                },
                result = interface_tasks.join_next(), if !interface_tasks.is_empty() => {
                    result.unwrap()??;
                }
                result = conversation_tasks.join_next(), if !conversation_tasks.is_empty() => {
                    result.unwrap()??;
                }
                _ = wait_for_restart(&mut restart) => {suspending=true;break;},
                _ = shutdown.changed() => {suspending=true;break;},
                result = recovery_tasks.join_next(), if !recovery_tasks.is_empty() => {
                    if let Some(result)=result && let Err(error)=result? {log::warn!("Recovery worker failed: {error:#}");}
                },
                event = rx.recv() => {
                    let Some(Event::Message(message)) = event else { break };
                    if Self::is_login_request(&message) {
                        gateway.handle_session_command(&message).await?;
                        message.acknowledge();
                        continue;
                    }
                    if !gateway.allows_message(&message)? {
                        message.acknowledge();
                        continue;
                    }
                    if gateway.telegram_spaces.is_some() && message.guest_query_id.is_some() {
                        gateway.prepare_telegram_guest(&message).await?;
                    } else if gateway.telegram_spaces.is_some() && gateway.prepare_telegram_chat(
                        &message.source.interface.0, message.source.conversation_id.parse()?
                    ).await? {
                        let reminders = gateway.reminders.last().unwrap().clone();
                        let interfaces = gateway.interfaces.clone();
                        background.spawn(async move { reminders.run(interfaces).await });
                        let wakeups = gateway.wakeups.last().unwrap().clone();
                        let interfaces = gateway.interfaces.clone();
                        background.spawn(async move { wakeups.run(interfaces).await });
                    }
                    if gateway.handle_session_command(&message).await? {
                        message.acknowledge();
                        continue;
                    }
                    let admission_id=message.session_id();
                    let admission_runner=gateway.runner_for(&admission_id)?;
                    let control=message.guest_query_id.is_none() && message.images.is_empty() && message.files.is_empty() && matches!(message.text.trim(),"/stop"|"/new");
                    if admission_runner.has_inbox() {
                        let mode=if control || message.is_group || message.guest_query_id.is_some() {crate::agent::inbox::BusyMode::FollowUp}else{crate::agent::inbox::BusyMode::Steer};
                        match admission_runner.submit(crate::agent::inbox::SubmissionRequest::new(&admission_id,message.clone(),mode)).await {
                            Ok(admitted)=>{message.acknowledge();if admitted.duplicate {continue;}},
                            Err(error) if error.is::<crate::agent::inbox::AdmissionRejected>()=>{
                                gateway.reject_input(&message,&error.to_string()).await;
                                continue;
                            },
                            Err(error)=>return Err(error),
                        }
                    }
                    if message.guest_query_id.is_none() && message.images.is_empty() && message.files.is_empty() && matches!(message.text.trim(), "/stop" | "/new") {
                        let id=SessionId::new(format!("{}:{}",message.source.interface,message.source.conversation_id));
                        gateway.runner_for(&id)?.cancel_conversation(&id).await?;
                        message.acknowledge();
                    }
                    if !admission_runner.has_inbox() {message.acknowledge();}
                    if message.guest_query_id.is_none() {
                        if let Some(proactive) = &self.proactive { proactive.note_activity(); }
                    }
                    let key = (message.source.interface.0.clone(), message.session_id().as_str().to_owned());
                    let tx = conversations.entry(key).or_insert_with(|| {
                        let (tx, rx) = mpsc::channel(100);
                        let gateway = gateway.clone();
                        conversation_tasks.spawn(async move { gateway.run_conversation(rx).await });
                        tx
                    });
                    tx.send(message).await?;
                }
            }
        }

        let stopping_started = tokio::time::Instant::now();
        info!("Gateway stopping: suspending work and closing interfaces");
        if suspending {
            gateway.agent_runner.suspend();
            if let Some(runners) = &gateway.chat_runners {
                for runner in runners.values() {
                    runner.suspend();
                }
            }
        }
        drop(conversations);
        interface_tasks.abort_all();
        while let Some(result) = interface_tasks.join_next().await {
            if let Err(error) = result
                && !error.is_cancelled()
            {
                return Err(error.into());
            }
        }
        drop(background);
        info!("Gateway shutdown: waiting for conversation and recovery workers");
        while let Some(result) = conversation_tasks.join_next().await {
            result??;
        }
        while let Some(result) = recovery_tasks.join_next().await {
            if let Err(error) = result? {
                log::warn!("Recovery worker failed: {error:#}");
            }
        }
        info!("Gateway shutdown: waiting for learning reviews");
        gateway.agent_runner.finish_learning().await;
        if let Some(runners) = &gateway.chat_runners {
            for runner in runners.values() {
                runner.finish_learning().await;
            }
        }

        info!(
            "Gateway workers stopped after {:?}",
            stopping_started.elapsed()
        );
        Ok(())
    }

    async fn flush_deliveries(
        &self,
        runner: &AgentRunner,
        interface: &dyn Interface,
        conversation: &str,
    ) -> anyhow::Result<()> {
        let Some((_, chat)) = conversation.rsplit_once(':') else {
            return Ok(());
        };
        if !interface.allows_recipient(chat) {
            return Ok(());
        }
        let source = crate::gateway::MessageSource {
            interface: interface.id(),
            conversation_id: chat.into(),
        };
        for (id, text) in runner.deliver_pending(interface, conversation).await? {
            self.complete_restart(&source, &id, &text);
        }
        Ok(())
    }

    async fn reject_input(&self, message: &IncomingMessage, reason: &str) {
        if let Some(interface) = self
            .interfaces
            .iter()
            .find(|i| i.id() == message.source.interface)
        {
            let language = interface.language(&message.source.conversation_id);
            let reason = if language == Language::Ru {
                match reason {
                    "Request ID payload conflict" => "Конфликт содержимого запроса",
                    "Session is busy in another conversation" => "Диалог занят в другом чате",
                    "Inbox queue is full" => "Очередь сообщений заполнена",
                    "Conversation is busy" | "Session is busy" => "Диалог занят",
                    reason => reason,
                }
            } else {
                reason
            };
            let text = format!(
                "{}: {reason}",
                language.pick("Request not accepted", "Запрос не принят")
            );
            let result = if let Some(query) = &message.guest_query_id {
                interface.answer_guest(query, &text).await
            } else {
                interface.send(&message.source.conversation_id, &text).await
            };
            if let Err(error) = result {
                log::warn!("Admission rejection could not be delivered: {error:#}");
            }
        }
        message.acknowledge();
    }

    async fn run_conversation(
        &self,
        mut rx: mpsc::Receiver<IncomingMessage>,
    ) -> anyhow::Result<()> {
        let mut pending = VecDeque::new();
        let mut receiving = true;
        loop {
            if *self.agent_runner.shutdown_receiver().borrow() {
                return Ok(());
            }
            let mut message = match pending.pop_front() {
                Some(message) => message,
                None => match rx.recv().await {
                    Some(message) => message,
                    None => return Ok(()),
                },
            };
            if !self.allows_message(&message)? {
                let id = message.session_id();
                self.runner_for(&id)?
                    .cancel_queued_input(&id, &message.request_id)
                    .await?;
                message.acknowledge();
                continue;
            }
            let id = message.session_id();
            let durable = self.runner_for(&id)?.has_inbox();
            if durable
                && self
                    .runner_for(&id)?
                    .submission_finished(&id, &message.request_id)
                    .await?
            {
                continue;
            }
            if durable
                && !matches!(message.text.trim(), "/stop" | "/new")
                && let Some(next) = self.runner_for(&id)?.next_input(&id).await?
                && next.request_id != message.request_id
            {
                pending.push_front(message);
                message = next;
            }

            if message.guest_query_id.is_none()
                && message.images.is_empty()
                && message.files.is_empty()
                && matches!(message.text.trim(), "/new" | "/stop")
            {
                self.handle_event(Event::Message(message), None, None)
                    .await?;
                continue;
            }
            let (cancel, cancelled) = oneshot::channel();
            let interface_id = message.source.interface.0.clone();
            let conversation_id = message.source.conversation_id.clone();
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            let mut stopped = false;
            if !durable && !message.is_group && message.guest_query_id.is_none() {
                for message in pending.drain(..) {
                    input_tx.send(UserMessage {
                        files: message.files,
                        content: message.text,
                        images: message.images,
                    })?;
                }
            }
            {
                let response = self.handle_event(
                    Event::Message(message),
                    Some(cancelled),
                    Some(&mut input_rx),
                );
                tokio::pin!(response);
                loop {
                    tokio::select! {
                        result = &mut response => {
                            result?;
                            break;
                        }
                        message = rx.recv(), if receiving => {
                            let Some(message) = message else {
                                receiving = false;
                                continue;
                            };
                            if message.guest_query_id.is_none() && message.images.is_empty() && message.files.is_empty() && matches!(message.text.trim(), "/new" | "/stop") {
                                let _ = cancel.send(());
                                stopped = true;
                                response.await?;
                                if !durable && !pending.is_empty() {
                                    let id = SessionId::new(format!("{interface_id}:{conversation_id}"));
                                    self.runner_for(&id)?.observe_group_messages(&id, pending.drain(..).collect()).await?;
                                }
                                if message.text.trim() == "/new" {
                                    self.handle_event(Event::Message(message), None, None).await?;
                                } else if durable {
                                    self.runner_for(&id)?.apply_control(&id,&message).await?;
                                }
                                break;
                            }
                            // A group participant may be talking to someone else, not steering this run.
                            if durable || message.is_group || message.guest_query_id.is_some() {
                                pending.push_back(message);
                            } else {
                                input_tx.send(UserMessage { files: message.files, content: message.text, images: message.images })?;
                            }
                        }
                    }
                }
            }
            if stopped {
                continue;
            }
            // Input arriving after the agent's final check continues only after saving.
            while let Ok(input) = input_rx.try_recv() {
                let mut message = IncomingMessage::new(
                    crate::gateway::InterfaceId(interface_id.clone()),
                    conversation_id.clone(),
                    input.content,
                );
                message.images = input.images;
                message.files = input.files;
                pending.push_back(message);
            }
        }
    }

    fn is_login_request(message: &IncomingMessage) -> bool {
        message.source.interface.0.starts_with("telegram:")
            && !message.is_group
            && message.guest_query_id.is_none()
            && message.images.is_empty()
            && message.files.is_empty()
            && message
                .source
                .conversation_id
                .parse::<u64>()
                .is_ok_and(|user| user > 0)
            && (message
                .callback
                .as_ref()
                .is_some_and(|callback| callback.data.starts_with("wa:"))
                || message.text.split_whitespace().next() == Some("/start")
                    && message
                        .text
                        .split_whitespace()
                        .nth(1)
                        .is_some_and(|payload| payload.starts_with("login_")))
    }

    fn allows_message(&self, message: &IncomingMessage) -> anyhow::Result<bool> {
        let Some(spaces) = &self.telegram_spaces else {
            return Ok(true);
        };
        if message.is_group {
            return Ok(false);
        }
        if message.guest_query_id.is_some() {
            return message
                .guest_user_id
                .map_or(Ok(false), |user| spaces.users.allowed_user(user));
        }
        let Ok(user) = message.source.conversation_id.parse::<u64>() else {
            return Ok(false);
        };
        spaces.users.allowed_user(user)
    }

    async fn handle_event(
        &self,
        event: Event,
        cancel: Option<oneshot::Receiver<()>>,
        incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
    ) -> anyhow::Result<()> {
        let Event::Message(message) = &event;
        if Self::is_login_request(message) {
            self.handle_session_command(message).await?;
            message.acknowledge();
            return Ok(());
        }
        if !self.allows_message(message)? {
            let id = message.session_id();
            if let Ok(runner) = self.runner_for(&id) {
                runner.cancel_queued_input(&id, &message.request_id).await?;
            }
            message.acknowledge();
            return Ok(());
        }
        let context = crate::audit::Context::new(message.session_id().as_str(), "message");
        let result = if let Some(cancel) = cancel {
            let (agent_cancel, agent_cancelled) = oneshot::channel();
            let (delivery_cancel, delivery_cancelled) = tokio::sync::watch::channel(false);
            crate::audit::scope(context, async {
                let response = self.handle_event_inner(
                    event,
                    Some(agent_cancelled),
                    incoming,
                    Some(delivery_cancelled),
                );
                tokio::pin!(response);
                tokio::select! {
                    result = &mut response => result,
                    _ = async move {
                        let _ = cancel.await;
                        let _ = agent_cancel.send(());
                        delivery_cancel.send_replace(true);
                    } => response.await,
                }
            })
            .await
        } else {
            crate::audit::scope(
                context,
                self.handle_event_inner(event, None, incoming, None),
            )
            .await
        };
        match result {
            Err(error)
                if error.is::<DeliveryCancelled>()
                    || error.is::<crate::agent::recovery::Suspended>() =>
            {
                Ok(())
            }
            result => result,
        }
    }

    async fn handle_event_inner(
        &self,
        event: Event,
        mut cancel: Option<oneshot::Receiver<()>>,
        incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
        delivery_cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> anyhow::Result<()> {
        match event {
            Event::Message(message) => {
                if self.handle_session_command(&message).await? {
                    message.acknowledge();
                    return Ok(());
                }
                let interface = self
                    .interfaces
                    .iter()
                    .find(|interface| interface.id() == message.source.interface)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Interface not found: {}", message.source.interface)
                    })?;

                let conversation_id = message.source.conversation_id.clone();
                let language = interface.language(&conversation_id);
                let reply_context = crate::agent::tools::telegram_reply::ReplyContext::new(
                    interface.clone(),
                    conversation_id.clone(),
                    message.guest_query_id.clone(),
                );
                let source = crate::gateway::MessageSource {
                    interface: crate::gateway::InterfaceId(message.source.interface.0.clone()),
                    conversation_id: conversation_id.clone(),
                };

                let session_id = message.session_id();
                let agent_runner = self.runner_for(&session_id)?;
                let request_id = message.request_id.clone();
                let delivery_session = session_id.clone();

                if let Some(query) = message.guest_query_id.clone() {
                    // Bind model attempts and background work to this caller over the shared store.
                    let guest_runner = if self.telegram_spaces.is_some() {
                        self.build_telegram_guest_runner(&message).await?
                    } else {
                        agent_runner.clone()
                    };
                    // A guest has exactly one response slot; no chat actions, progress,
                    // group participation decision or normal sendMessage is allowed.
                    let response = crate::agent::tools::telegram_reply::REPLY_CONTEXT
                        .scope(reply_context.clone(), async {
                            let mut shutdown = self.agent_runner.shutdown_receiver();
                            let response = guest_runner
                                .run_with_cancel(session_id, message, None, cancel, None);
                            tokio::pin!(response);
                            tokio::select! {
                                result = &mut response => result,
                                    _ = async { let _ = shutdown.wait_for(|stopped| *stopped).await; } => {
                                    guest_runner.suspend();
                                    response.await
                                }
                            }
                        })
                        .await;
                    if response
                        .as_ref()
                        .is_err_and(|error| error.is::<crate::agent::recovery::Suspended>())
                    {
                        return Err(crate::agent::recovery::Suspended.into());
                    }
                    if reply_context.guest_consumed().await {
                        guest_runner.finish_learning().await;
                        return Ok(());
                    }
                    let text = match response {
                        Ok(response) => response.content,
                        Err(error) => format!(
                            "{}: {error:#}",
                            language.pick("Response interrupted", "Ответ прерван")
                        ),
                    };
                    if let Err(error) = interface.answer_guest(&query, &text).await {
                        log::warn!("Telegram guest reply failed: {error:#}");
                    }
                    guest_runner.finish_learning().await;
                    return Ok(());
                }

                if message.images.is_empty()
                    && message.files.is_empty()
                    && message.text.trim() == "/new"
                {
                    let confirmation = match agent_runner.apply_control(&session_id, &message).await
                    {
                        Ok(_) => language
                            .pick("New session created.", "Новый диалог создан.")
                            .to_owned(),
                        Err(error) => format!(
                            "{}: {error:#}",
                            language.pick("Could not create session", "Не удалось создать диалог")
                        ),
                    };
                    if let Err(error) = interface.send(&conversation_id, &confirmation).await {
                        warn!("Session changed but confirmation could not be sent: {error}");
                    }
                    return Ok(());
                }

                if message.images.is_empty()
                    && message.files.is_empty()
                    && message.text.trim() == "/stop"
                {
                    agent_runner.apply_control(&session_id, &message).await?;
                    if let Err(error) = interface
                        .send(
                            &conversation_id,
                            language.pick("The agent is not running.", "Агент не запущен."),
                        )
                        .await
                    {
                        warn!("Could not send stop confirmation: {error}");
                    }
                    return Ok(());
                }

                if message.is_group
                    && !agent_runner.has_inbox()
                    && !agent_runner
                        .should_reply_to_group(&session_id, &message, &mut cancel)
                        .await?
                {
                    return Ok(());
                }

                let started = tokio::time::Instant::now();
                let (tx, rx) = mpsc::channel(32);
                let (response, (status_id, status_text, plan, preview_id, used_tool)) = tokio::join!(
                    crate::agent::tools::telegram_reply::REPLY_CONTEXT.scope(
                        reply_context.clone(),
                        agent_runner.run_with_cancel(
                            session_id,
                            message,
                            Some(tx),
                            cancel,
                            incoming
                        ),
                    ),
                    Self::show_progress(interface.as_ref(), &conversation_id, rx),
                );

                if response.as_ref().is_err_and(|e| {
                    e.is::<crate::agent::recovery::Suspended>()
                        || e.is::<crate::agent::inbox::Observed>()
                }) {
                    return Ok(());
                }
                let completed = response.is_ok();
                let text = match response {
                    Ok(response) => response.content,
                    Err(error) if error.to_string() == "Stopped by user" => {
                        language.pick("Agent stopped.", "Агент остановлен.").into()
                    }
                    Err(error) => format!(
                        "{}: {error:#}",
                        language.pick("Response interrupted", "Ответ прерван")
                    ),
                };
                if completed && let Some(id) = reply_context.delivered().await {
                    for id in [status_id, preview_id].into_iter().flatten() {
                        if let Err(error) = interface.delete(&conversation_id, &id).await {
                            warn!("Could not remove reply progress: {error:#}");
                        }
                    }
                    self.complete_restart(&source, &id, &text);
                    return Ok(());
                }
                if let Some(plan) = plan {
                    let outcome = if completed && used_tool {
                        language.pick("Done", "Готово")
                    } else if completed {
                        ""
                    } else if text == language.pick("Agent stopped.", "Агент остановлен.")
                    {
                        language.pick("Stopped", "Остановлено")
                    } else {
                        language.pick("Error", "Ошибка")
                    };
                    let done = if outcome.is_empty() {
                        plan.clone()
                    } else {
                        with_plan(
                            Some(&plan),
                            &format!(
                                "{outcome} — {}",
                                elapsed_text(language, started.elapsed().as_secs())
                            ),
                        )
                    };
                    let edited = if let Some(id) = &status_id {
                        match interface.edit(&conversation_id, id, &done).await {
                            Ok(()) => true,
                            Err(error) => {
                                warn!("Failed to finalize plan: {error}");
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if !edited && let Err(error) = interface.send(&conversation_id, &done).await {
                        warn!("Failed to send final plan: {error}");
                    }
                    let id = if completed
                        && agent_runner.has_inbox()
                        && source.interface.0.starts_with("telegram:")
                    {
                        agent_runner
                            .deliver_final(
                                interface.as_ref(),
                                &delivery_session,
                                &request_id,
                                preview_id.as_ref(),
                                &reply_context,
                            )
                            .await?
                    } else {
                        Some(
                            deliver_reply(
                                interface.as_ref(),
                                &conversation_id,
                                preview_id.as_ref(),
                                &text,
                                delivery_cancel.as_ref(),
                            )
                            .await?,
                        )
                    };
                    if completed && let Some(id) = id {
                        self.complete_restart(&source, &id, &text);
                    }
                    return Ok(());
                }
                if completed {
                    let elapsed = started.elapsed();
                    if used_tool && elapsed > Duration::from_secs(15) {
                        let done = format!(
                            "{} — {}",
                            language.pick("Done", "Готово"),
                            elapsed_text(language, elapsed.as_secs())
                        );
                        let edited = if let Some(id) = &status_id {
                            match interface.edit(&conversation_id, id, &done).await {
                                Ok(()) => true,
                                Err(error) => {
                                    warn!("Failed to finalize progress message: {error}");
                                    false
                                }
                            }
                        } else {
                            false
                        };
                        if !edited && let Err(error) = interface.send(&conversation_id, &done).await
                        {
                            warn!("Failed to send completion status: {error}");
                        }
                    } else if let Some(id) = &status_id
                        && let Err(error) = interface.delete(&conversation_id, id).await
                    {
                        warn!("Failed to delete progress message: {error}");
                    }
                    let id = if completed
                        && agent_runner.has_inbox()
                        && source.interface.0.starts_with("telegram:")
                    {
                        agent_runner
                            .deliver_final(
                                interface.as_ref(),
                                &delivery_session,
                                &request_id,
                                preview_id.as_ref(),
                                &reply_context,
                            )
                            .await?
                    } else {
                        Some(
                            deliver_reply(
                                interface.as_ref(),
                                &conversation_id,
                                preview_id.as_ref(),
                                &text,
                                delivery_cancel.as_ref(),
                            )
                            .await?,
                        )
                    };
                    if let Some(id) = id {
                        self.complete_restart(&source, &id, &text);
                    }
                    return Ok(());
                }
                if let Some(id) = preview_id {
                    match deliver_reply(
                        interface.as_ref(),
                        &conversation_id,
                        Some(&id),
                        &text,
                        delivery_cancel.as_ref(),
                    )
                    .await
                    {
                        Ok(_) => return Ok(()),
                        Err(error) if error.is::<DeliveryCancelled>() => return Err(error),
                        Err(_) => {}
                    }
                    if let Err(error) = interface.delete(&conversation_id, &id).await {
                        warn!("Failed to remove partial reply: {error}");
                    }
                }
                if let Some(id) = status_id {
                    if text == status_text {
                        return Ok(());
                    }
                    match deliver_reply(
                        interface.as_ref(),
                        &conversation_id,
                        Some(&id),
                        &text,
                        delivery_cancel.as_ref(),
                    )
                    .await
                    {
                        Ok(_) => return Ok(()),
                        Err(error) if error.is::<DeliveryCancelled>() => return Err(error),
                        Err(error) => warn!("Failed to finalize response message: {error}"),
                    }
                }
                deliver_reply(
                    interface.as_ref(),
                    &conversation_id,
                    None,
                    &text,
                    delivery_cancel.as_ref(),
                )
                .await?;
            }
        }

        Ok(())
    }

    fn complete_restart(
        &self,
        source: &crate::gateway::MessageSource,
        message_id: &MessageId,
        text: &str,
    ) {
        let Some(restart) = &self.restart else {
            return;
        };
        if let Err(error) = restart.complete_response(source, message_id, text) {
            warn!("Cannot prepare cooperative restart: {error:#}");
        }
    }

    async fn show_progress(
        interface: &dyn Interface,
        conversation_id: &str,
        mut rx: mpsc::Receiver<AgentEvent>,
    ) -> (
        Option<MessageId>,
        String,
        Option<String>,
        Option<MessageId>,
        bool,
    ) {
        let language = interface.language(conversation_id);
        let mut status_id = None;
        let mut preview_id = None;
        let mut preview = String::new();
        let mut preview_sent = String::new();
        let mut plan = None;
        let mut current_tool = None;
        let mut used_tool = false;
        let mut description = String::new();
        let mut status_text = String::new();
        let mut status_state = None;
        let mut status_needed = false;
        let mut message_since_tool = false;
        let started = tokio::time::Instant::now();
        let mut typing = tokio::time::interval(Duration::from_secs(4));
        typing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previews = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(2),
            Duration::from_secs(2),
        );
        previews.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut progress_disabled = false;

        loop {
            let finished = tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(AgentEvent::InputConsumed) => {
                            if progress_disabled {
                                continue;
                            }
                            for id in [status_id.take(), preview_id.take()].into_iter().flatten() {
                                if let Err(error) = interface.delete(conversation_id, &id).await {
                                    warn!("Failed to remove progress after new input: {error}");
                                }
                            }
                            status_state = None;
                            preview.clear();
                            preview_sent.clear();
                            false
                        }
                        // A reply belongs in its own message; previewing it in the status
                        // makes the chat jump when that status is removed or shortened.
                        Some(AgentEvent::SessionChanged { .. }) => {
                            if let Err(error) = interface.send(conversation_id, language.pick("New topic — started a new session.", "Новая тема — начат новый диалог.")).await {
                                log::warn!("Failed to announce session change: {error}");
                            }
                            continue;
                        }
                        Some(AgentEvent::TextDelta(delta)) => {
                            preview.push_str(&delta);
                            if preview.trim().is_empty() || preview_id.is_some() {
                                continue;
                            }
                            false
                        }
                        Some(AgentEvent::PlanUpdated(update)) => {
                            let rendered = update.render();
                            let rendered = if language == Language::Ru { rendered.replacen("Plan ·", "План ·", 1) } else { rendered };
                            plan = Some(rendered.replace('\n', "<br>"));
                            status_needed = true;
                            false
                        }
                        Some(AgentEvent::Retrying(message) | AgentEvent::Compaction(message)) => {
                            description = service_progress(language, &message);
                            status_needed = true;
                            message_since_tool = false;
                            current_tool = None;
                            false
                        }
                        Some(AgentEvent::Message(message)) => {
                            description = message.content.split_whitespace().collect::<Vec<_>>().join(" ");
                            description = description.trim_end_matches(['.', '…']).trim_end().to_owned();
                            message_since_tool = !description.is_empty();
                            if !status_needed {
                                continue;
                            }
                            false
                        }
                        Some(AgentEvent::ContextTokens(_)) => continue,
                        Some(AgentEvent::ToolStarted { name, call_id, arguments }) => {
                            used_tool = true;
                            if !progress_disabled && let Some(id) = preview_id.take()
                                && let Err(error) = interface.delete(conversation_id, &id).await
                            {
                                warn!("Failed to remove tool preamble: {error}");
                            }
                            preview.clear();
                            preview_sent.clear();
                            status_needed = true;
                            let tool = tool_description(&name, &arguments);
                            if !message_since_tool {
                                description = tool.clone();
                            }
                            current_tool = Some(tool);
                            message_since_tool = false;
                            info!("Tool {name} started ({call_id})");
                            false
                        }
                        Some(AgentEvent::ToolFinished { call_id, failed, .. }) => {
                            info!("Tool finished ({call_id}), failed={failed}");
                            continue;
                        }
                        None => true,
                    }
                }
                _ = typing.tick() => {
                    if !progress_disabled && let Err(error) = interface.typing(conversation_id).await {
                        warn!("Failed to send typing status: {error}");
                    }
                    continue;
                }
                _ = previews.tick(), if interface.stream_replies() && !progress_disabled && !preview.trim().is_empty() && preview != preview_sent => false,
            };

            if progress_disabled {
                if finished {
                    break;
                }
                continue;
            }

            if interface.stream_replies()
                && !finished
                && !preview.trim().is_empty()
                && preview != preview_sent
            {
                // Telegram messages have a 4096-character limit. Keep the visible
                // preview bounded; the completed reply follows the normal path.
                let text: String = preview.chars().take(4000).collect();
                if text != preview_sent {
                    let result = if let Some(id) = &preview_id {
                        interface
                            .edit(conversation_id, id, &text)
                            .await
                            .map(|_| None)
                    } else {
                        interface.send(conversation_id, &text).await.map(Some)
                    };
                    match result {
                        Ok(id) => {
                            if let Some(id) = id {
                                preview_id = Some(id);
                            }
                            preview_sent = text;
                        }
                        Err(error) => {
                            warn!(
                                "Failed to stream reply; disabling progress for this run: {error}"
                            );
                            progress_disabled = true;
                        }
                    }
                }
            }
            if progress_disabled {
                if finished {
                    break;
                }
                continue;
            }
            if !status_needed {
                if finished {
                    break;
                }
                continue;
            }
            let mut status = progress_text(language, &description, started.elapsed().as_secs());
            if let Some(tool) = &current_tool
                && tool != &description
            {
                status.push_str(&format!("<br><br>{}", inline_code(language, tool)));
            }
            let text = with_plan(plan.as_deref(), &status);
            let state = (plan.clone(), description.clone(), current_tool.clone());
            if status_state.as_ref() == Some(&state) {
                if finished {
                    break;
                }
                continue;
            }
            if let Some(id) = &status_id {
                if let Err(error) = interface.edit(conversation_id, id, &text).await {
                    warn!("Failed to edit progress message: {error}");
                    progress_disabled = true;
                    if finished {
                        break;
                    }
                    continue;
                }
            } else {
                match interface.send_status(conversation_id, &text).await {
                    Ok(id) => status_id = Some(id),
                    Err(error) => {
                        warn!("Failed to send progress message: {error}");
                        progress_disabled = true;
                        if finished {
                            break;
                        }
                        continue;
                    }
                }
            }
            status_text = text;
            status_state = Some(state);
            if finished {
                break;
            }
        }

        (status_id, status_text, plan, preview_id, used_tool)
    }
}

#[derive(Debug)]
struct DeliveryCancelled;
impl std::fmt::Display for DeliveryCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Delivery cancelled by user")
    }
}
impl std::error::Error for DeliveryCancelled {}

async fn deliver_reply(
    interface: &dyn Interface,
    conversation_id: &str,
    id: Option<&MessageId>,
    text: &str,
    cancelled: Option<&tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<MessageId> {
    loop {
        let result = match id {
            Some(id) => interface
                .edit(conversation_id, id, text)
                .await
                .map(|()| id.clone()),
            None => interface.send(conversation_id, text).await,
        };
        let error = match result {
            Ok(id) => return Ok(id),
            Err(error) => error,
        };
        let retry_after = error
            .downcast_ref::<crate::interfaces::SendRejection>()
            .filter(|rejection| rejection.error_code == 429)
            .and_then(|rejection| rejection.retry_after);
        let Some(deadline) = retry_after.and_then(|seconds| {
            tokio::time::Instant::now().checked_add(Duration::from_secs(seconds.max(1)))
        }) else {
            return Err(error);
        };
        warn!("Final reply rate limited; waiting for Telegram retry_after");
        if let Some(cancelled) = cancelled {
            let mut cancelled = cancelled.clone();
            tokio::select! {
                biased;
                _ = cancelled.wait_for(|stopped| *stopped) => return Err(DeliveryCancelled.into()),
                _ = tokio::time::sleep_until(deadline) => {},
            }
        } else {
            tokio::time::sleep_until(deadline).await;
        }
    }
}

async fn wait_for_restart(restart: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    match restart {
        Some(restart) => {
            let _ = restart.wait_for(|requested| *requested).await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::AssistantMessage, gateway::InterfaceId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScopeLlm;

    struct GroupInterface(mpsc::UnboundedSender<String>);

    #[async_trait::async_trait]
    impl Interface for GroupInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("test".into())
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, text: &str) -> anyhow::Result<MessageId> {
            self.0.send(text.into())?;
            Ok(MessageId("1".into()))
        }
        async fn typing(&self, _: &str) -> anyhow::Result<()> {
            self.0.send("<typing>".into())?;
            Ok(())
        }
    }

    fn group_message(text: &str) -> IncomingMessage {
        let mut message = IncomingMessage::new(InterfaceId("test".into()), "group", text);
        message.is_group = true;
        message
    }

    struct GuestInterface(mpsc::UnboundedSender<(String, String)>);

    #[async_trait::async_trait]
    impl Interface for GuestInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            anyhow::bail!("ordinary send in guest chat")
        }
        async fn answer_guest(&self, query: &str, text: &str) -> anyhow::Result<MessageId> {
            anyhow::ensure!(query != "revoked", "Telegram guest access denied");
            self.0.send((query.into(), text.into()))?;
            Ok(MessageId("inline".into()))
        }
    }

    #[tokio::test]
    async fn denied_guest_delivery_does_not_stop_later_queries() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        let directory = tempfile::tempdir().unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ScopeLlm),
                ToolRegistry::default(),
                ContextBuilder::new("guest"),
            )),
            Arc::new(JsonSessionStore::new(directory.path())),
        ));
        let (tx, mut sent) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner).add_interface(GuestInterface(tx));
        let (queue, inputs) = mpsc::channel(2);
        for query in ["revoked", "live"] {
            let mut message =
                IncomingMessage::new(InterfaceId("telegram:test".into()), "99", "hello");
            message.guest_query_id = Some(query.into());
            queue.send(message).await.unwrap();
        }
        drop(queue);
        gateway.run_conversation(inputs).await.unwrap();
        assert_eq!(sent.try_recv().unwrap(), ("live".into(), "guest".into()));
        assert!(sent.try_recv().is_err());
    }

    #[tokio::test]
    async fn guest_reply_uses_one_query_and_separate_chat_session() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        let directory = tempfile::tempdir().unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ScopeLlm),
                ToolRegistry::default(),
                ContextBuilder::new("guest"),
            )),
            Arc::new(JsonSessionStore::new(directory.path())),
        ));
        let (tx, mut sent) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner).add_interface(GuestInterface(tx));
        let mut message =
            IncomingMessage::new(InterfaceId("telegram:test".into()), "-42", "@bot hello");
        message.is_group = true;
        message.guest_query_id = Some("query".into());
        gateway
            .handle_event(Event::Message(message), None, None)
            .await
            .unwrap();
        assert_eq!(sent.try_recv().unwrap(), ("query".into(), "guest".into()));
        assert!(sent.try_recv().is_err());
        assert!(
            directory
                .path()
                .join("telegram:test:-42:guest.json")
                .exists()
        );
        assert!(!directory.path().join("telegram:test:-42.json").exists());
    }

    #[tokio::test]
    async fn durable_guest_queries_keep_separate_response_slots() {
        use crate::agent::{
            Agent, ContextBuilder,
            inbox::{BusyMode, SubmissionRequest},
            session::SqliteSessionStore,
            tools::ToolRegistry,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ScopeLlm),
                ToolRegistry::default(),
                ContextBuilder::new("guest"),
            )),
            store.clone(),
        ));
        let (tx, mut sent) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner).add_interface(GuestInterface(tx));
        let id = SessionId::new("telegram:test:-42:guest");
        let mut messages = vec![];
        for query in ["one", "two"] {
            let mut message =
                IncomingMessage::new(InterfaceId("telegram:test".into()), "-42", "hello");
            message.is_group = true;
            message.guest_query_id = Some(query.into());
            store
                .submit(SubmissionRequest::new(
                    &id,
                    message.clone(),
                    BusyMode::FollowUp,
                ))
                .await
                .unwrap();
            messages.push(message);
        }
        for message in messages {
            gateway
                .handle_event(Event::Message(message), None, None)
                .await
                .unwrap();
        }
        assert_eq!(sent.try_recv().unwrap(), ("one".into(), "guest".into()));
        assert_eq!(sent.try_recv().unwrap(), ("two".into(), "guest".into()));
        assert!(sent.try_recv().is_err());
    }
    struct RichReplyLlm;

    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for RichReplyLlm {
        async fn generate(
            &self,
            context: &crate::agent::LlmContext<'_>,
            tools: &[&dyn crate::agent::tools::Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            use crate::agent::message::{Message, ToolCall};
            assert!(tools.iter().any(|tool| tool.name() == "telegram_reply"));
            let reply = if context
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult(_)))
            {
                Message::Assistant(AssistantMessage {
                    content: "final text".into(),
                })
            } else {
                Message::ToolCall(ToolCall {
                    call_id: "send".into(),
                    name: "telegram_reply".into(),
                    arguments: serde_json::json!({"message":{"type":"post","title":"Report","body":"Contents"}}),
                })
            };
            Ok((vec![reply], None))
        }
    }

    struct RichInterface {
        sent: mpsc::UnboundedSender<(String, String, String)>,
        refusal: Option<bool>,
    }

    #[async_trait::async_trait]
    impl Interface for RichInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
            self.sent.send(("text".into(), chat.into(), text.into()))?;
            Ok(MessageId("progress".into()))
        }
        async fn answer_guest(&self, query: &str, text: &str) -> anyhow::Result<MessageId> {
            self.sent
                .send(("guest_text".into(), query.into(), text.into()))?;
            Ok(MessageId("inline".into()))
        }
        async fn send_content(
            &self,
            chat: &str,
            message: &crate::OutgoingMessage,
        ) -> anyhow::Result<MessageId> {
            let crate::OutgoingMessage::Post { body, .. } = message else {
                panic!("unexpected payload")
            };
            self.sent
                .send(("content".into(), chat.into(), body.clone()))?;
            match self.refusal {
                Some(true) => Err(crate::interfaces::SendRejection {
                    description: "rejected".into(),
                    error_code: 400,
                    retry_after: None,
                }
                .into()),
                Some(false) => anyhow::bail!("connection lost"),
                None => Ok(MessageId("42".into())),
            }
        }
        async fn answer_guest_content(
            &self,
            query: &str,
            message: &crate::OutgoingMessage,
        ) -> anyhow::Result<MessageId> {
            self.send_content(query, message).await
        }
    }

    #[tokio::test]
    async fn explicit_replies_suppress_duplicates_and_guest_fallback_requires_an_unused_slot() {
        use crate::agent::{
            Agent, ContextBuilder,
            session::JsonSessionStore,
            tools::{TelegramReplyTool, ToolRegistry},
        };
        for (guest, refusal) in [
            (false, None),
            (true, None),
            (true, Some(true)),
            (true, Some(false)),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let restart = Arc::new(crate::gateway::restart::RestartCoordinator::new(
                directory.path().join("restart"),
            ));
            if !guest {
                restart.request(std::process::id()).unwrap();
            }
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(RichReplyLlm),
                    ToolRegistry::default().register(Arc::new(TelegramReplyTool::new(None))),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(directory.path())),
            ));
            let (sent, mut received) = mpsc::unbounded_channel();
            let gateway = GatewayRunner::new(runner)
                .with_restart(restart.clone())
                .add_interface(RichInterface { sent, refusal });
            let mut message =
                IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "send a report");
            message.guest_query_id = guest.then(|| "guest-query".into());
            gateway
                .handle_event(Event::Message(message), None, None)
                .await
                .unwrap();
            let mut sent = Vec::new();
            while let Ok(item) = received.try_recv() {
                sent.push(item);
            }
            let content: Vec<_> = sent
                .iter()
                .filter(|(kind, _, _)| kind == "content")
                .collect();
            assert_eq!(content.len(), 1, "{sent:?}");
            assert_eq!(content[0].1, if guest { "guest-query" } else { "42" });
            let finals: Vec<_> = sent
                .iter()
                .filter(|(_, _, text)| text == "final text")
                .collect();
            assert_eq!(finals.len(), usize::from(refusal == Some(true)), "{sent:?}");
            if guest {
                assert!(!sent.iter().any(|(kind, _, _)| kind == "text"));
            } else {
                let receipt = restart
                    .receipt()
                    .unwrap()
                    .expect("explicit reply must complete a pending restart");
                assert_eq!(receipt.message_id, "42");
            }
        }
    }

    struct GroupDecision {
        reply: &'static str,
        requests: mpsc::UnboundedSender<serde_json::Value>,
    }

    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for GroupDecision {
        async fn generate(
            &self,
            context: &crate::agent::LlmContext<'_>,
            tools: &[&dyn crate::agent::tools::Tool],
            progress: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            use crate::agent::message::Message;
            assert!(tools.is_empty());
            assert!(progress.is_none());
            let Message::User(input) = &context.messages[0] else {
                panic!()
            };
            let request: serde_json::Value = serde_json::from_str(&input.content)?;
            let reply = if self.reply == "initial_only" {
                if request["state"]["incoming_message"] == "initial" {
                    r#"{"choice":"reply"}"#
                } else {
                    r#"{"choice":"silent"}"#
                }
            } else {
                self.reply
            };
            self.requests.send(request)?;
            if self.reply == "wait" {
                std::future::pending::<()>().await;
            }
            anyhow::ensure!(self.reply != "error", "decision unavailable");
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: reply.into(),
                })],
                None,
            ))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn group_decisions_preserve_history_and_attachments_without_visible_activity() {
        use crate::agent::{
            Agent, ContextBuilder, message::Message, session::JsonSessionStore,
            system_one::SystemOne, tools::ToolRegistry,
        };
        for choice in [
            r#"{"choice":"silent"}"#,
            r#"{"choice":"uncertain"}"#,
            "invalid",
            "error",
            "wait",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (requests, mut decisions) = mpsc::unbounded_channel();
            let runner = Arc::new(
                AgentRunner::new(
                    Arc::new(Agent::new(
                        Arc::new(ScopeLlm),
                        ToolRegistry::default(),
                        ContextBuilder::new("answer"),
                    )),
                    Arc::new(JsonSessionStore::new(directory.path())),
                )
                .with_group_decider(SystemOne::new(Arc::new(GroupDecision {
                    reply: choice,
                    requests,
                }))),
            );
            let (updates, mut replies) = mpsc::unbounded_channel();
            let gateway = GatewayRunner::new(runner.clone()).add_interface(GroupInterface(updates));
            let mut first = group_message("Саша, посмотри файл");
            first.telegram_group = Some(
                serde_json::from_value(serde_json::json!({
                    "chat_id":-42,"name":"Group","bot_username":"@TestBot",
                    "participants":{"user:11":{"name":"Alice"}},
                }))
                .unwrap(),
            );
            first.files.push(
                crate::agent::FileAttachment::save(
                    &directory.path().join("attachments"),
                    "report.pdf",
                    None,
                    b"%PDF-1.7\ntest",
                )
                .unwrap(),
            );
            first.images.push(
                crate::agent::Image::from_bytes(&crate::agent::image::tests::sample(
                    image::ImageFormat::Png,
                ))
                .unwrap(),
            );
            gateway
                .handle_event(Event::Message(first), None, None)
                .await
                .unwrap();
            gateway
                .handle_event(
                    Event::Message(group_message("Это всё ещё вопрос Саше")),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(
                replies.try_recv().is_err(),
                "{choice}: leaked visible activity"
            );
            let request = decisions.try_recv().unwrap();
            assert_eq!(request["state"]["has_images"], true);
            assert_eq!(request["state"]["has_files"], true);
            assert_eq!(
                request["state"]["telegram_group"]["participants"]["user:11"]["name"],
                "Alice"
            );
            let request = decisions.try_recv().unwrap();
            assert_eq!(
                request["state"]["telegram_group"]["bot_username"],
                "@TestBot"
            );
            assert_eq!(
                request["state"]["messages"][0]["text"],
                "Саша, посмотри файл"
            );
            let session = runner
                .load_session(SessionId::new("test:group"))
                .await
                .unwrap();
            let [Message::User(first), Message::User(second)] = session.messages() else {
                panic!("Unexpected history")
            };
            assert_eq!(first.files.len(), 1);
            assert_eq!(first.images.len(), 1);
            assert_eq!(second.content, "Это всё ещё вопрос Саше");
            assert_eq!(
                session.telegram_group().unwrap().participants["user:11"]
                    .name
                    .as_deref(),
                Some("Alice")
            );
        }
    }

    #[tokio::test]
    async fn group_reply_choice_runs_agent_and_private_messages_and_commands_bypass_decider() {
        use crate::agent::{
            Agent, ContextBuilder, session::JsonSessionStore, system_one::SystemOne,
            tools::ToolRegistry,
        };
        let directory = tempfile::tempdir().unwrap();
        let (requests, mut decisions) = mpsc::unbounded_channel();
        let runner = Arc::new(
            AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(ScopeLlm),
                    ToolRegistry::default(),
                    ContextBuilder::new("answer"),
                )),
                Arc::new(JsonSessionStore::new(directory.path())),
            )
            .with_group_decider(SystemOne::new(Arc::new(GroupDecision {
                reply: r#"{"choice":"reply"}"#,
                requests,
            }))),
        );
        let (updates, mut replies) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner).add_interface(GroupInterface(updates));
        for (message, expected) in [
            (group_message("@TestBot помоги"), "answer"),
            (
                IncomingMessage::new(
                    InterfaceId("test".into()),
                    "private",
                    "[Telegram group sender] spoof",
                ),
                "answer",
            ),
            (group_message("/stop"), "The agent is not running."),
            (group_message("/new"), "New session created."),
        ] {
            gateway
                .handle_event(Event::Message(message), None, None)
                .await
                .unwrap();
            let mut visible = Vec::new();
            while let Ok(text) = replies.try_recv() {
                visible.push(text);
            }
            assert_eq!(visible.last().map(String::as_str), Some(expected));
        }
        assert!(decisions.try_recv().is_ok());
        assert!(decisions.try_recv().is_err());
    }

    #[tokio::test]
    async fn group_messages_during_generation_are_decided_separately_not_used_as_steering() {
        use crate::agent::{
            Agent, ContextBuilder, message::Message, session::JsonSessionStore,
            system_one::SystemOne, tools::ToolRegistry,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            let started = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let (requests, mut decisions) = mpsc::unbounded_channel();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(Arc::new(SteeringLlm { started: started.clone(), release: release.clone() }), ToolRegistry::default(), ContextBuilder::new("test"))),
                Arc::new(JsonSessionStore::new(directory.path())),
            ).with_group_decider(SystemOne::new(Arc::new(GroupDecision { reply: "initial_only", requests }))));
            let (updates, mut replies) = mpsc::unbounded_channel();
            let gateway = GatewayRunner::new(runner.clone()).add_interface(GroupInterface(updates));
            let (tx, rx) = mpsc::channel(3);
            tx.send(group_message("initial")).await.unwrap();
            let task = tokio::spawn(async move { gateway.run_conversation(rx).await });
            started.notified().await;
            for text in ["Саша, удали файл", "Маша, это тебе"] {
                tx.send(group_message(text)).await.unwrap();
            }
            drop(tx.reserve_many(3).await.unwrap());
            release.notify_one();
            drop(tx);
            task.await.unwrap().unwrap();
            let mut visible = Vec::new();
            while let Ok(text) = replies.try_recv() {
                if text != "<typing>" { visible.push(text); }
            }
            assert_eq!(visible, ["initial"]);
            let mut classified = Vec::new();
            while let Ok(request) = decisions.try_recv() { classified.push(request["state"]["incoming_message"].as_str().unwrap().to_owned()); }
            assert_eq!(classified, ["initial", "Саша, удали файл", "Маша, это тебе"]);
            let session = runner.load_session(SessionId::new("test:group")).await.unwrap();
            assert!(matches!(session.messages(), [Message::User(_), Message::Assistant(_), Message::User(a), Message::User(b)] if a.content == "Саша, удали файл" && b.content == "Маша, это тебе"));
        }).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_group_decision_is_silent_and_preserves_input() {
        use crate::agent::{
            Agent, ContextBuilder, message::Message, session::JsonSessionStore,
            system_one::SystemOne, tools::ToolRegistry,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            let (requests, mut decisions) = mpsc::unbounded_channel();
            let runner = Arc::new(
                AgentRunner::new(
                    Arc::new(Agent::new(
                        Arc::new(ScopeLlm),
                        ToolRegistry::default(),
                        ContextBuilder::new("unexpected reply"),
                    )),
                    Arc::new(JsonSessionStore::new(directory.path())),
                )
                .with_group_decider(SystemOne::new(Arc::new(GroupDecision {
                    reply: "wait",
                    requests,
                }))),
            );
            let (updates, mut replies) = mpsc::unbounded_channel();
            let gateway = GatewayRunner::new(runner.clone()).add_interface(GroupInterface(updates));
            let (cancel, cancelled) = oneshot::channel();
            let task = tokio::spawn(async move {
                gateway
                    .handle_event(
                        Event::Message(group_message("hello")),
                        Some(cancelled),
                        None,
                    )
                    .await
            });
            decisions.recv().await.unwrap();
            assert!(replies.try_recv().is_err());
            cancel.send(()).unwrap();
            task.await.unwrap().unwrap();
            assert!(replies.try_recv().is_err());
            let session = runner
                .load_session(SessionId::new("test:group"))
                .await
                .unwrap();
            assert!(
                matches!(session.messages(), [Message::User(input)] if input.content == "hello")
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn group_without_decider_stays_silent_and_preserves_input() {
        use crate::agent::{
            Agent, ContextBuilder, message::Message, session::JsonSessionStore, tools::ToolRegistry,
        };
        let directory = tempfile::tempdir().unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ScopeLlm),
                ToolRegistry::default(),
                ContextBuilder::new("unexpected reply"),
            )),
            Arc::new(JsonSessionStore::new(directory.path())),
        ));
        let (updates, mut replies) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner.clone()).add_interface(GroupInterface(updates));
        gateway
            .handle_event(Event::Message(group_message("Саша, посмотри")), None, None)
            .await
            .unwrap();
        assert!(
            replies.try_recv().is_err(),
            "silence includes typing and progress"
        );
        let session = runner
            .load_session(SessionId::new("test:group"))
            .await
            .unwrap();
        assert!(
            matches!(session.messages(), [Message::User(input)] if input.content == "Саша, посмотри")
        );
    }

    #[tokio::test]
    async fn group_stop_and_new_preserve_queued_messages_in_old_session_without_executing() {
        use crate::agent::{
            Agent, ContextBuilder,
            message::Message,
            session::{JsonSessionStore, SessionStore},
            system_one::SystemOne,
            tools::ToolRegistry,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            for command in ["/stop", "/new"] {
                let directory = tempfile::tempdir().unwrap();
                let store = Arc::new(JsonSessionStore::new(directory.path()));
                let started = Arc::new(tokio::sync::Notify::new());
                let (requests, mut decisions) = mpsc::unbounded_channel();
                let runner = Arc::new(
                    AgentRunner::new(
                        Arc::new(Agent::new(
                            Arc::new(SteeringLlm {
                                started: started.clone(),
                                release: Arc::default(),
                            }),
                            ToolRegistry::default(),
                            ContextBuilder::new("test"),
                        )),
                        store.clone(),
                    )
                    .with_group_decider(SystemOne::new(Arc::new(
                        GroupDecision {
                            reply: r#"{"choice":"reply"}"#,
                            requests,
                        },
                    ))),
                );
                let id = SessionId::new("test:group");
                let old = runner.load_session(id.clone()).await.unwrap().id().clone();
                let (updates, _replies) = mpsc::unbounded_channel();
                let gateway =
                    GatewayRunner::new(runner.clone()).add_interface(GroupInterface(updates));
                let (tx, rx) = mpsc::channel(3);
                tx.send(group_message("initial")).await.unwrap();
                let task = tokio::spawn(async move { gateway.run_conversation(rx).await });
                started.notified().await;
                let mut queued = group_message("Саша, посмотри файл");
                queued.files.push(
                    crate::agent::FileAttachment::save(
                        &directory.path().join("attachments"),
                        "note.txt",
                        None,
                        b"queued data",
                    )
                    .unwrap(),
                );
                tx.send(queued).await.unwrap();
                drop(tx.reserve_many(3).await.unwrap());
                tx.send(group_message(command)).await.unwrap();
                drop(tx);
                task.await.unwrap().unwrap();
                let session = store.load(&old).await.unwrap().unwrap();
                let inputs: Vec<_> = session
                    .messages()
                    .iter()
                    .filter_map(|m| {
                        if let Message::User(u) = m {
                            Some(u)
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(inputs.len(), 2, "{command} lost queued input");
                assert_eq!(inputs[1].content, "Саша, посмотри файл");
                assert_eq!(inputs[1].files.len(), 1);
                assert!(decisions.try_recv().is_ok());
                assert!(
                    decisions.try_recv().is_err(),
                    "cancelled queued input was classified"
                );
                let active = runner.load_session(id).await.unwrap();
                assert_eq!(active.id() == &old, command == "/stop");
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn group_decision_context_is_bounded_and_keeps_recent_speakers() {
        use crate::agent::{
            Agent, ContextBuilder, UserMessage,
            message::Message,
            session::{JsonSessionStore, Session, SessionStore},
            system_one::SystemOne,
            tools::ToolRegistry,
        };
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonSessionStore::new(directory.path()));
        let id = SessionId::new("test:group");
        let mut session = Session::new(id.clone());
        for content in [
            "OLD_SENTINEL".into(),
            "Длинное сообщение ".repeat(10_000),
            "Саша: да".into(),
        ] {
            session.push(Message::User(UserMessage {
                content,
                images: vec![],
                files: vec![],
            }));
        }
        store.save(&session).await.unwrap();
        let (requests, mut decisions) = mpsc::unbounded_channel();
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ScopeLlm),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store,
        )
        .with_group_decider(SystemOne::new(Arc::new(GroupDecision {
            reply: r#"{"choice":"silent"}"#,
            requests,
        })));
        runner
            .should_reply_to_group(&id, &group_message("Маша: поняла"), &mut None)
            .await
            .unwrap();
        let request = decisions.try_recv().unwrap();
        assert!(request.to_string().len() < 70_000);
        assert!(!request.to_string().contains("OLD_SENTINEL"));
        assert_eq!(
            request["state"]["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["text"],
            "Саша: да"
        );
        assert_eq!(request["state"]["incoming_message"], "Маша: поняла");
    }

    #[tokio::test]
    async fn group_observed_files_are_available_to_later_sandbox_tools() {
        use crate::agent::{
            Agent, ContextBuilder, sandbox::Sandbox, session::JsonSessionStore, tools::ToolRegistry,
        };
        let directory = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(directory.path().to_owned()));
        let runner = AgentRunner::new(
            Arc::new(
                Agent::new(
                    Arc::new(ScopeLlm),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )
                .with_sandbox(sandbox.clone()),
            ),
            Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
        );
        let mut message = group_message("Саша, вот файл");
        message.files.push(
            crate::agent::FileAttachment::save(
                &directory.path().join("attachments"),
                "note.txt",
                None,
                b"observed data",
            )
            .unwrap(),
        );
        let id = SessionId::new("test:group");
        runner
            .should_reply_to_group(&id, &message, &mut None)
            .await
            .unwrap();
        let result = sandbox
            .tool("exec_command")
            .execute(serde_json::json!({"cmd":"cat /attachments/*/note.txt"}))
            .await
            .unwrap();
        assert_eq!(result.output["stdout"], "observed data");
        let session = runner.load_session(id).await.unwrap();
        let crate::agent::message::Message::User(input) = &session.messages()[0] else {
            panic!()
        };
        assert!(
            input
                .content
                .contains("[Attachment in virtual filesystem: /attachments/")
        );
    }

    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for ScopeLlm {
        async fn generate(
            &self,
            context: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            Ok((
                vec![crate::agent::message::Message::Assistant(
                    AssistantMessage {
                        content: context.instructions.split('\n').next().unwrap().to_owned(),
                    },
                )],
                None,
            ))
        }
    }

    #[tokio::test]
    async fn chat_routing_never_falls_back_to_owner_and_new_is_scoped() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        let root = tempfile::tempdir().unwrap();
        let runner = |name| {
            Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(ScopeLlm),
                    ToolRegistry::default(),
                    ContextBuilder::new(name),
                )),
                Arc::new(JsonSessionStore::new(root.path().join(name))),
            ))
        };
        let owner = runner("PRIVATE_OWNER");
        let first = runner("FIRST_GROUP");
        let second = runner("SECOND_GROUP");
        let (updates, mut replies) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(owner.clone())
            .with_chat_runners(HashMap::from([
                (SessionId::new("test:42"), owner.clone()),
                (SessionId::new("test:-1"), first.clone()),
                (SessionId::new("test:-2"), second.clone()),
            ]))
            .add_interface(RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: None,
            });
        for (chat, expected) in [
            ("42", "PRIVATE_OWNER"),
            ("-1", "FIRST_GROUP"),
            ("-2", "SECOND_GROUP"),
        ] {
            gateway
                .handle_event(
                    Event::Message(IncomingMessage::new(
                        InterfaceId("test".into()),
                        chat,
                        "hello",
                    )),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(replies.recv().await.unwrap(), expected);
        }
        let old = first.load_session(SessionId::new("test:-1")).await.unwrap();
        gateway
            .handle_event(
                Event::Message(IncomingMessage::new(
                    InterfaceId("test".into()),
                    "-1",
                    "/new",
                )),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(replies.recv().await.unwrap(), "New session created.");
        assert_ne!(
            first
                .load_session(SessionId::new("test:-1"))
                .await
                .unwrap()
                .id(),
            old.id()
        );
        assert_eq!(
            second
                .load_session(SessionId::new("test:-2"))
                .await
                .unwrap()
                .messages()
                .len(),
            2
        );
        assert_eq!(
            owner
                .load_session(SessionId::new("test:42"))
                .await
                .unwrap()
                .messages()
                .len(),
            2
        );
        assert!(
            gateway
                .handle_event(
                    Event::Message(IncomingMessage::new(
                        InterfaceId("test".into()),
                        "unknown",
                        "read private"
                    )),
                    None,
                    None
                )
                .await
                .is_err()
        );
        assert!(replies.try_recv().is_err());
    }

    #[test]
    fn russian_progress_localizes_service_text_and_preserves_tool_arguments() {
        assert_eq!(
            progress_text(Language::Ru, "", 65),
            "` Работаю ` — 1 мин 5 сек"
        );
        assert_eq!(
            progress_text(Language::Ru, "exec_command: echo `pwd`\nnext", 2),
            "`` exec_command: echo `pwd` next `` — 2 сек"
        );
        assert_eq!(
            service_progress(Language::Ru, "Request failed. Retry 2/3 in 4 sec"),
            "Ошибка запроса. Повтор 2/3 через 4 сек"
        );
        assert_eq!(
            service_progress(Language::Ru, "Compacting conversation context…"),
            "Сжимаю контекст диалога…"
        );
        assert_eq!(
            service_progress(Language::En, "Context compacted; continuing…"),
            "Context compacted; continuing…"
        );
    }

    #[test]
    fn progress_format_wraps_description_and_formats_minutes() {
        for (description, elapsed, expected) in [
            ("Проверяю файлы.", 59, "` Проверяю файлы. ` — 59 sec"),
            (
                "Проверяю файлы...",
                60,
                "` Проверяю файлы... ` — 1 min 0 sec",
            ),
            ("Проверяю файлы…", 90, "` Проверяю файлы… ` — 1 min 30 sec"),
            (
                "Шаг 1. Проверяю файлы.",
                120,
                "` Шаг 1. Проверяю файлы. ` — 2 min 0 sec",
            ),
            ("", 0, "` Working ` — 0 sec"),
            (
                "apply_patch: *** Begin Patch\n\n# Heading\n**bold**\n```md\ntext\n```\n*** End Patch",
                3,
                "```` apply_patch: *** Begin Patch # Heading **bold** ```md text ``` *** End Patch ```` — 3 sec",
            ),
            (
                "exec_command: echo `pwd`",
                3,
                "`` exec_command: echo `pwd` `` — 3 sec",
            ),
            (
                "exec_command: длинная команда…",
                3,
                "` exec_command: длинная команда… ` — 3 sec",
            ),
        ] {
            assert_eq!(progress_text(Language::En, description, elapsed), expected);
        }
    }

    #[test]
    fn tool_summary_is_bounded() {
        let description =
            tool_description("exec_command", &serde_json::json!({"cmd": "x".repeat(300)}));
        assert!(description.starts_with("exec_command: xxx"));
        assert!(description.ends_with("…"));
        assert!(description.chars().count() <= 180);
        assert_eq!(
            tool_description(
                "exec_command",
                &serde_json::json!({"cmd": "echo `pwd` ```"})
            ),
            "exec_command: echo `pwd` ```"
        );
    }

    #[test]
    fn tool_summary_uses_primary_argument_and_preserves_unknown_arguments() {
        for (name, args, expected) in [
            (
                "exec_command",
                serde_json::json!({"cmd": "cargo test", "timeout_ms": 1000}),
                "exec_command: cargo test",
            ),
            (
                "view_image",
                serde_json::json!({"path": "image.png"}),
                "view_image: image.png",
            ),
            (
                "custom",
                serde_json::json!({"count": 3}),
                "custom: {\"count\":3}",
            ),
            ("custom", serde_json::json!({}), "custom: {}"),
        ] {
            assert_eq!(tool_description(name, &args), expected);
        }
    }

    struct RecordingInterface {
        updates: mpsc::UnboundedSender<String>,
        sends: Arc<AtomicUsize>,
        incoming: Option<tokio::sync::Mutex<mpsc::Receiver<Event>>>,
    }

    #[async_trait::async_trait]
    impl Interface for RecordingInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("test".into())
        }

        async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
            if let Some(incoming) = &self.incoming {
                let mut rx = incoming.lock().await;
                while let Some(event) = rx.recv().await {
                    tx.send(event).await?;
                }
            }
            Ok(())
        }

        async fn send(&self, _: &str, message: &str) -> anyhow::Result<MessageId> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            self.updates.send(message.into())?;
            Ok(MessageId("status".into()))
        }

        async fn delete(&self, _: &str, id: &MessageId) -> anyhow::Result<()> {
            assert_eq!(id.0, "status");
            self.updates.send("<deleted>".into())?;
            Ok(())
        }

        async fn edit(&self, _: &str, id: &MessageId, message: &str) -> anyhow::Result<()> {
            assert_eq!(id.0, "status");
            self.updates.send(message.into())?;
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stream_bursts_are_coalesced_without_losing_final_preview() {
        let (updates, mut messages) = mpsc::unbounded_channel();
        let interface = RecordingInterface {
            updates,
            sends: Arc::default(),
            incoming: None,
        };
        let (tx, rx) = mpsc::channel(128);
        let task =
            tokio::spawn(async move { GatewayRunner::show_progress(&interface, "chat", rx).await });
        tx.send(AgentEvent::TextDelta("first".into()))
            .await
            .unwrap();
        assert_eq!(messages.recv().await.unwrap(), "first");
        for _ in 0..100 {
            tx.send(AgentEvent::TextDelta("x".into())).await.unwrap();
        }
        tokio::task::yield_now().await;
        assert!(messages.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            messages.recv().await.unwrap(),
            format!("first{}", "x".repeat(100))
        );
        drop(tx);
        task.await.unwrap();
        assert!(messages.try_recv().is_err());
    }

    #[tokio::test]
    async fn inbox_overflow_rejects_one_message_without_stopping_gateway() {
        use crate::agent::{
            Agent, ContextBuilder,
            inbox::{BusyMode, MAX_QUEUED_SUBMISSIONS, SubmissionRequest},
            session::SqliteSessionStore,
            tools::ToolRegistry,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                SqliteSessionStore::open_async(dir.path().into())
                    .await
                    .unwrap(),
            );
            let id = SessionId::new("test:full");
            for _ in 0..MAX_QUEUED_SUBMISSIONS {
                store
                    .submit(SubmissionRequest::new(
                        &id,
                        IncomingMessage::new(InterfaceId("test".into()), "full", "pending"),
                        BusyMode::FollowUp,
                    ))
                    .await
                    .unwrap();
            }
            store
                .with_connection(|db| {
                    db.execute(
                        "INSERT INTO inbox_controls VALUES('test:full','paused')",
                        [],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(ScopeLlm),
                    ToolRegistry::default(),
                    ContextBuilder::new("ok"),
                )),
                store,
            ));
            let (updates, mut sent) = mpsc::unbounded_channel();
            let (tx, rx) = mpsc::channel(4);
            let gateway = GatewayRunner::new(runner.clone()).add_interface(RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: Some(tokio::sync::Mutex::new(rx)),
            });
            let task = tokio::spawn(async move { gateway.run().await });
            tx.send(Event::Message(IncomingMessage::new(
                InterfaceId("test".into()),
                "full",
                "overflow",
            )))
            .await
            .unwrap();
            assert!(sent.recv().await.unwrap().contains("Inbox queue is full"));
            tx.send(Event::Message(IncomingMessage::new(
                InterfaceId("test".into()),
                "other",
                "hello",
            )))
            .await
            .unwrap();
            while sent.recv().await.unwrap() != "ok" {}
            runner.suspend();
            task.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }

    struct RefusingInterface {
        attempts: AtomicUsize,
        mode: &'static str,
    }

    #[async_trait::async_trait]
    impl Interface for RefusingInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("test".into())
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.mode == "network" {
                anyhow::bail!("outcome unknown");
            }
            if attempt == 0 || self.mode == "forbidden" || (self.mode == "twice" && attempt == 1) {
                return Err(crate::interfaces::SendRejection {
                    description: "refused".into(),
                    error_code: if self.mode == "forbidden" { 403 } else { 429 },
                    retry_after: if self.mode == "forbidden" {
                        None
                    } else {
                        Some(120)
                    },
                }
                .into());
            }
            Ok(MessageId("reply".into()))
        }
    }

    #[tokio::test]
    async fn rejected_progress_is_not_repeated_for_each_delta() {
        let interface = RefusingInterface {
            attempts: 0.into(),
            mode: "rate",
        };
        let (tx, rx) = mpsc::channel(128);
        for _ in 0..100 {
            tx.send(AgentEvent::TextDelta("x".into())).await.unwrap();
        }
        drop(tx);
        GatewayRunner::show_progress(&interface, "chat", rx).await;
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn final_reply_waits_for_confirmed_rate_limit_but_never_retries_unknown_delivery() {
        for mode in ["rate", "network", "forbidden"] {
            let interface = RefusingInterface {
                attempts: 0.into(),
                mode,
            };
            let start = tokio::time::Instant::now();
            let result = deliver_reply(&interface, "chat", None, "complete reply", None).await;
            assert_eq!(result.is_ok(), mode == "rate");
            assert_eq!(
                interface.attempts.load(Ordering::SeqCst),
                if mode == "rate" { 2 } else { 1 }
            );
            assert_eq!(
                start.elapsed(),
                if mode == "rate" {
                    Duration::from_secs(120)
                } else {
                    Duration::ZERO
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_rate_limit_wait_before_retry() {
        let interface = Arc::new(RefusingInterface {
            attempts: 0.into(),
            mode: "rate",
        });
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let sending = interface.clone();
        let task = tokio::spawn(async move {
            deliver_reply(sending.as_ref(), "chat", None, "reply", Some(&cancelled)).await
        });
        tokio::task::yield_now().await;
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 1);
        cancel.send_replace(true);
        assert!(task.await.unwrap().unwrap_err().is::<DeliveryCancelled>());
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn new_session_command_interrupts_final_reply_cooldown() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        let directory = tempfile::tempdir().unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(SteeringLlm {
                    started: Arc::default(),
                    release: Arc::default(),
                }),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(directory.path())),
        ));
        let conversation = SessionId::new("test:chat");
        let original = runner.load_session(conversation.clone()).await.unwrap();
        let interface = Arc::new(RefusingInterface {
            attempts: 0.into(),
            mode: "twice",
        });
        let mut gateway = GatewayRunner::new(runner.clone());
        gateway.interfaces.push(interface.clone());
        let (tx, rx) = mpsc::channel(8);
        tx.send(IncomingMessage::new(
            InterfaceId("test".into()),
            "chat",
            "hello",
        ))
        .await
        .unwrap();
        let task = tokio::spawn(async move { gateway.run_conversation(rx).await });
        while interface.attempts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        tx.send(IncomingMessage::new(
            InterfaceId("test".into()),
            "chat",
            "/new",
        ))
        .await
        .unwrap();
        tx.send(IncomingMessage::new(
            InterfaceId("test".into()),
            "chat",
            "after new",
        ))
        .await
        .unwrap();
        drop(tx);
        task.await.unwrap().unwrap();
        let session = runner.load_session(conversation).await.unwrap();
        assert_ne!(session.id(), original.id());
        assert!(session.messages().iter().any(|message| matches!(message, crate::agent::message::Message::User(user) if user.content == "after new")));
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_is_visible_before_any_tool_call() {
        let (updates, mut messages) = mpsc::unbounded_channel();
        let interface = RecordingInterface {
            updates,
            sends: Arc::default(),
            incoming: None,
        };
        let (tx, rx) = mpsc::channel(32);
        tx.send(AgentEvent::Retrying("Retry 1/3".into()))
            .await
            .unwrap();
        drop(tx);
        let (id, _, _, _, _) = GatewayRunner::show_progress(&interface, "chat", rx).await;
        assert!(id.is_some());
        assert!(messages.recv().await.unwrap().contains("Retry 1/3"));
    }

    #[tokio::test(start_paused = true)]
    async fn consumed_input_recreates_progress_with_unchanged_plan_and_tool() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (updates, mut received) = mpsc::unbounded_channel();
            let sends = Arc::new(AtomicUsize::new(0));
            let interface = RecordingInterface {
                updates,
                sends: sends.clone(),
                incoming: None,
            };
            let (tx, rx) = mpsc::channel(8);
            let task =
                tokio::spawn(
                    async move { GatewayRunner::show_progress(&interface, "chat", rx).await },
                );
            tx.send(AgentEvent::PlanUpdated(
                crate::agent::tools::TaskPlan::parse(serde_json::json!({
                    "plan":[{"step":"Inspect","status":"in_progress"}]
                }))
                .unwrap(),
            ))
            .await
            .unwrap();
            received.recv().await.unwrap();
            tx.send(AgentEvent::ToolStarted {
                call_id: "cmd".into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"cmd":"cargo test"}),
            })
            .await
            .unwrap();
            let status = received.recv().await.unwrap();
            for _ in 0..2 {
                tx.send(AgentEvent::InputConsumed).await.unwrap();
                assert_eq!(received.recv().await.unwrap(), "<deleted>");
                assert_eq!(received.recv().await.unwrap(), status);
            }
            assert_eq!(sends.load(Ordering::SeqCst), 3);
            drop(tx);
            assert!(task.await.unwrap().0.is_some());
            assert!(received.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn consumed_input_starts_fresh_preview_without_creating_status() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (updates, mut received) = mpsc::unbounded_channel();
            let sends = Arc::new(AtomicUsize::new(0));
            let interface = RecordingInterface {
                updates,
                sends: sends.clone(),
                incoming: None,
            };
            let (tx, rx) = mpsc::channel(8);
            let task =
                tokio::spawn(
                    async move { GatewayRunner::show_progress(&interface, "chat", rx).await },
                );
            tx.send(AgentEvent::InputConsumed).await.unwrap();
            tx.send(AgentEvent::TextDelta("Old answer".into()))
                .await
                .unwrap();
            assert_eq!(received.recv().await.unwrap(), "Old answer");
            tx.send(AgentEvent::InputConsumed).await.unwrap();
            assert_eq!(received.recv().await.unwrap(), "<deleted>");
            tx.send(AgentEvent::TextDelta("New answer".into()))
                .await
                .unwrap();
            assert_eq!(received.recv().await.unwrap(), "New answer");
            drop(tx);
            let (status_id, _, _, preview_id, _) = task.await.unwrap();
            assert!(status_id.is_none());
            assert!(preview_id.is_some());
            assert_eq!(sends.load(Ordering::SeqCst), 2);
            assert!(received.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn plan_and_command_share_one_message() {
        let (updates, mut received) = mpsc::unbounded_channel();
        let sends = Arc::new(AtomicUsize::new(0));
        let interface = RecordingInterface {
            updates,
            sends: sends.clone(),
            incoming: None,
        };
        let (tx, rx) = mpsc::channel(8);
        let task =
            tokio::spawn(async move { GatewayRunner::show_progress(&interface, "chat", rx).await });
        tx.send(AgentEvent::PlanUpdated(
            crate::agent::tools::TaskPlan::parse(serde_json::json!({
                "plan":[{"step":"Inspect","status":"in_progress"}]
            }))
            .unwrap(),
        ))
        .await
        .unwrap();
        received.recv().await.unwrap();
        tx.send(AgentEvent::Message(AssistantMessage {
            content: "Проверяю проект".into(),
        }))
        .await
        .unwrap();
        received.recv().await.unwrap();
        tx.send(AgentEvent::ToolStarted {
            call_id: "cmd".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd":"cargo test"}),
        })
        .await
        .unwrap();
        let combined = tokio::time::timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(combined.contains("Inspect"));
        assert!(combined.contains("cargo test"));
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn plan_updates_edit_one_message() {
        let (updates, mut received) = mpsc::unbounded_channel();
        let sends = Arc::new(AtomicUsize::new(0));
        let interface = RecordingInterface {
            updates,
            sends: sends.clone(),
            incoming: None,
        };
        let (tx, rx) = mpsc::channel(8);
        for status in ["in_progress", "completed"] {
            tx.send(AgentEvent::PlanUpdated(
                crate::agent::tools::TaskPlan::parse(serde_json::json!({
                    "plan": [
                        {"step": "Inspect", "status": status},
                        {"step": "Verify", "status": "pending"}
                    ],
                    "explanation": "Checking the implementation"
                }))
                .unwrap(),
            ))
            .await
            .unwrap();
        }
        drop(tx);
        GatewayRunner::show_progress(&interface, "1", rx).await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        let mut texts = Vec::new();
        while let Ok(text) = received.try_recv() {
            texts.push(text);
        }
        assert!(!texts.is_empty());
        assert!(
            texts
                .last()
                .unwrap()
                .contains("Plan · 1/2<br>✓ Inspect<br>○ Verify<br><br>Checking the implementation")
        );
    }

    #[tokio::test]
    async fn progress_replaces_description_and_updates_elapsed_time() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (updates, mut messages) = mpsc::unbounded_channel();
            let interface = RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: None,
            };
            let (tx, rx) = mpsc::channel(32);
            let progress =
                tokio::spawn(
                    async move { GatewayRunner::show_progress(&interface, "chat", rx).await },
                );
            tx.send(AgentEvent::Message(AssistantMessage {
                content: "До инструмента".into(),
            }))
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), messages.recv())
                    .await
                    .is_err()
            );
            tx.send(AgentEvent::ToolStarted {
                call_id: "call".into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"command": "secret"}),
            })
            .await
            .unwrap();
            assert!(
                messages
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("` До инструмента ` — ")
            );
            for (description, expected) in [
                (" Проверяю\n файлы. ", "` Проверяю файлы ` — "),
                ("Запускаю тесты", "` Запускаю тесты ` — "),
                (" \n ", "` Working ` — "),
            ] {
                tx.send(AgentEvent::Message(AssistantMessage {
                    content: description.into(),
                }))
                .await
                .unwrap();
                assert!(messages.recv().await.unwrap().starts_with(expected));
            }
            tx.send(AgentEvent::ToolFinished {
                call_id: "call".into(),
                failed: false,
                output: serde_json::json!("tool output"),
            })
            .await
            .unwrap();
            tx.send(AgentEvent::ContextTokens(Some(100))).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), messages.recv())
                    .await
                    .is_err()
            );
            drop(tx);
            assert_eq!(progress.await.unwrap().0, Some(MessageId("status".into())));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tool_without_commentary_shows_name_and_arguments() {
        let (updates, mut messages) = mpsc::unbounded_channel();
        let interface = RecordingInterface {
            updates,
            sends: Arc::default(),
            incoming: None,
        };
        let (tx, rx) = mpsc::channel(32);
        let progress =
            tokio::spawn(async move { GatewayRunner::show_progress(&interface, "chat", rx).await });
        tx.send(AgentEvent::ToolStarted {
            call_id: "one".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd": "pwd"}),
        })
        .await
        .unwrap();
        assert!(
            messages
                .recv()
                .await
                .unwrap()
                .starts_with("` exec_command: pwd ` — ")
        );
        tx.send(AgentEvent::ToolStarted {
            call_id: "two".into(),
            name: "view_image".into(),
            arguments: serde_json::json!({"path": "test.png"}),
        })
        .await
        .unwrap();
        assert!(
            messages
                .recv()
                .await
                .unwrap()
                .starts_with("` view_image: test.png ` — ")
        );
        drop(tx);
        progress.await.unwrap();
    }

    #[tokio::test]
    async fn reply_deltas_stream_separately_from_status() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (updates, mut messages) = mpsc::unbounded_channel();
            let interface = RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: None,
            };
            let (tx, rx) = mpsc::channel(32);
            let progress =
                tokio::spawn(
                    async move { GatewayRunner::show_progress(&interface, "chat", rx).await },
                );
            tx.send(AgentEvent::TextDelta("Большой ответ\n".repeat(100)))
                .await
                .unwrap();
            assert_eq!(
                messages.recv().await.unwrap(),
                "Большой ответ\n".repeat(100)
            );
            tx.send(AgentEvent::ToolStarted {
                call_id: "call".into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"cmd": "pwd"}),
            })
            .await
            .unwrap();
            assert_eq!(messages.recv().await.unwrap(), "<deleted>");
            assert!(
                messages
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("` exec_command: pwd ` — ")
            );
            tx.send(AgentEvent::TextDelta("Финальный ответ".into()))
                .await
                .unwrap();
            assert_eq!(messages.recv().await.unwrap(), "Финальный ответ");
            drop(tx);
            progress.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn whitespace_deltas_do_not_create_blank_preview() {
        let (updates, mut messages) = mpsc::unbounded_channel();
        let interface = RecordingInterface {
            updates,
            sends: Arc::default(),
            incoming: None,
        };
        let (tx, rx) = mpsc::channel(32);
        let progress =
            tokio::spawn(async move { GatewayRunner::show_progress(&interface, "chat", rx).await });
        tx.send(AgentEvent::TextDelta(" \n".into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(messages.try_recv().is_err());
        tx.send(AgentEvent::TextDelta("Ответ".into()))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(300), messages.recv())
                .await
                .expect("first visible delta should stream without waiting for the timer")
                .unwrap(),
            " \nОтвет"
        );
        drop(tx);
        progress.await.unwrap();
    }

    #[tokio::test]
    async fn tool_updates_follow_changes_without_elapsed_ticks() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (updates, mut messages) = mpsc::unbounded_channel();
            let interface = RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: None,
            };
            let (tx, rx) = mpsc::channel(32);
            let progress =
                tokio::spawn(
                    async move { GatewayRunner::show_progress(&interface, "chat", rx).await },
                );
            for cmd in ["first", "second", "latest"] {
                tx.send(AgentEvent::ToolStarted {
                    call_id: cmd.into(),
                    name: "exec_command".into(),
                    arguments: serde_json::json!({"cmd": cmd}),
                })
                .await
                .unwrap();
                let update = tokio::time::timeout(Duration::from_millis(300), messages.recv())
                    .await
                    .expect("tool change must update status")
                    .unwrap();
                assert!(update.starts_with(&format!("` exec_command: {cmd} ` — ")));
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(1200), messages.recv())
                    .await
                    .is_err(),
                "elapsed time alone must not edit the status"
            );
            tx.send(AgentEvent::PlanUpdated(
                crate::agent::tools::TaskPlan::parse(serde_json::json!({
                    "plan":[{"step":"Check","status":"in_progress"}]
                }))
                .unwrap(),
            ))
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(300), messages.recv())
                    .await
                    .expect("plan change must update status")
                    .is_some()
            );
            drop(tx);
            progress.await.unwrap();
        })
        .await
        .unwrap();
    }

    struct PlanLlm {
        outcome: &'static str,
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for PlanLlm {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            progress: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            progress
                .unwrap()
                .send(AgentEvent::PlanUpdated(
                    crate::agent::tools::TaskPlan::parse(serde_json::json!({
                        "plan":[{"step":"Preserved step","status":"in_progress"}]
                    }))
                    .unwrap(),
                ))
                .await
                .unwrap();
            self.started.notify_one();
            match self.outcome {
                "cancel" => std::future::pending::<()>().await,
                "error" => anyhow::bail!("test error"),
                _ => {}
            }
            Ok((
                vec![crate::agent::message::Message::Assistant(
                    AssistantMessage {
                        content: "final answer".into(),
                    },
                )],
                None,
            ))
        }
    }

    #[tokio::test]
    async fn delivered_response_completes_matching_restart() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        use crate::gateway::restart::RestartCoordinator;

        let directory = tempfile::tempdir().unwrap();
        let restart = Arc::new(RestartCoordinator::new(directory.path().into()));
        restart.request(std::process::id()).unwrap();
        let mut shutdown = restart.subscribe();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(PlanLlm {
                    outcome: "success",
                    started: Arc::new(tokio::sync::Notify::new()),
                }),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
        ));
        let (updates, mut messages) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner)
            .with_restart(restart.clone())
            .add_interface(RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: None,
            });

        gateway
            .handle_event(
                Event::Message(IncomingMessage::new(
                    InterfaceId("test".into()),
                    "chat",
                    "update",
                )),
                None,
                None,
            )
            .await
            .unwrap();

        let mut delivered = Vec::new();
        while let Ok(message) = messages.try_recv() {
            delivered.push(message);
        }
        assert_eq!(delivered.last().unwrap(), "final answer");
        assert!(*shutdown.borrow_and_update());
        let receipt = restart.receipt().unwrap().unwrap();
        assert_eq!(receipt.conversation_id, "chat");
        assert_eq!(receipt.message_id, "status");
        assert_eq!(receipt.original_text, "final answer");
    }

    #[tokio::test]
    async fn matching_restart_stops_interface_and_exits_gateway() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        use crate::gateway::restart::RestartCoordinator;

        let directory = tempfile::tempdir().unwrap();
        let restart = Arc::new(RestartCoordinator::new(directory.path().into()));
        restart.request(std::process::id()).unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(PlanLlm {
                    outcome: "success",
                    started: Arc::new(tokio::sync::Notify::new()),
                }),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
        ));
        let (events, incoming) = mpsc::channel(1);
        let (updates, _messages) = mpsc::unbounded_channel();
        let gateway = GatewayRunner::new(runner)
            .with_restart(restart)
            .add_interface(RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: Some(tokio::sync::Mutex::new(incoming)),
            });

        let task = tokio::spawn(async move { gateway.run().await });
        events
            .send(Event::Message(IncomingMessage::new(
                InterfaceId("test".into()),
                "chat",
                "update",
            )))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("gateway ignored cooperative shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn new_gateway_confirms_receipt_with_a_new_message() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        use crate::gateway::restart::RestartCoordinator;

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("restart-receipt.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "request_id": "request",
                "old_pid": std::process::id() + 1,
                "interface": "test",
                "conversation_id": "chat",
                "message_id": "status",
                "original_text": "Update complete",
                "created_at": "2026-10-05T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
        let restart = Arc::new(RestartCoordinator::new(directory.path().into()));
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(PlanLlm {
                    outcome: "success",
                    started: Arc::new(tokio::sync::Notify::new()),
                }),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
        ));
        let (events, incoming) = mpsc::channel(1);
        let (updates, mut messages) = mpsc::unbounded_channel();
        let sends = Arc::new(AtomicUsize::new(0));
        let gateway = GatewayRunner::new(runner)
            .with_restart(restart.clone())
            .add_interface(RecordingInterface {
                updates,
                sends: sends.clone(),
                incoming: Some(tokio::sync::Mutex::new(incoming)),
            });

        let task = tokio::spawn(async move { gateway.run().await });
        let confirmation = tokio::time::timeout(Duration::from_secs(1), messages.recv())
            .await
            .expect("restart confirmation was not delivered")
            .unwrap();
        assert!(confirmation.starts_with("Gateway restarted successfully."));
        assert_eq!(sends.load(Ordering::Relaxed), 1);
        assert!(restart.receipt().unwrap().is_none());
        drop(events);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn finalization_preserves_plan_for_success_error_and_cancellation() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        for (outcome, label, reply) in [
            ("success", "Preserved step", "final answer"),
            ("error", "Error", "Response interrupted: test error"),
            ("cancel", "Stopped", "Agent stopped."),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let started = Arc::new(tokio::sync::Notify::new());
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(PlanLlm {
                        outcome,
                        started: started.clone(),
                    }),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(dir.path())),
            ));
            let (updates, mut messages) = mpsc::unbounded_channel();
            let sends = Arc::new(AtomicUsize::new(0));
            let gateway = GatewayRunner::new(runner).add_interface(RecordingInterface {
                updates,
                sends: sends.clone(),
                incoming: None,
            });
            let (cancel, cancelled) = oneshot::channel();
            let task = tokio::spawn(async move {
                gateway
                    .handle_event(
                        Event::Message(IncomingMessage::new(
                            InterfaceId("test".into()),
                            "chat",
                            "work",
                        )),
                        Some(cancelled),
                        None,
                    )
                    .await
            });
            started.notified().await;
            if outcome == "cancel" {
                cancel.send(()).unwrap();
            }
            task.await.unwrap().unwrap();
            let mut texts = Vec::new();
            while let Ok(text) = messages.try_recv() {
                texts.push(text);
            }
            assert_eq!(
                sends.load(Ordering::SeqCst),
                2,
                "one progress message and one answer"
            );
            assert!(!texts.iter().any(|t| t == "<deleted>"));
            let final_plan = &texts[texts.len() - 2];
            assert!(final_plan.contains("Preserved step"));
            assert!(final_plan.contains(label));
            if outcome == "success" {
                assert!(!final_plan.contains("Done — "));
            }
            assert_eq!(texts.last().unwrap(), reply);
        }
    }

    struct SteeringLlm {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for SteeringLlm {
        async fn generate(
            &self,
            context: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            use crate::agent::message::Message;
            let inputs: Vec<_> = context
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User(user) => Some(user.content.as_str()),
                    _ => None,
                })
                .collect();
            if inputs == ["initial"] {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: inputs.join(" + "),
                })],
                None,
            ))
        }
    }

    #[tokio::test]
    async fn messages_during_generation_steer_before_final_reply() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        tokio::time::timeout(Duration::from_secs(5), async {
            let release = Arc::new(tokio::sync::Notify::new());
            let started = Arc::new(tokio::sync::Notify::new());
            let directory = tempfile::tempdir().unwrap();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(SteeringLlm {
                        started: started.clone(),
                        release: release.clone(),
                    }),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(directory.path())),
            ));
            let (updates, mut replies) = mpsc::unbounded_channel();
            let gateway = GatewayRunner::new(runner.clone()).add_interface(RecordingInterface {
                updates,
                sends: Arc::default(),
                incoming: None,
            });
            let (tx, rx) = mpsc::channel(3);
            tx.send(IncomingMessage::new(
                InterfaceId("test".into()),
                "chat",
                "initial",
            ))
            .await
            .unwrap();
            let task = tokio::spawn(async move { gateway.run_conversation(rx).await });
            started.notified().await;
            let file = crate::agent::FileAttachment::save(
                &directory.path().join("attachments"),
                "report.pdf",
                None,
                b"%PDF-1.7\ntest",
            )
            .unwrap();
            for text in ["first correction", "second correction"] {
                let mut message = IncomingMessage::new(InterfaceId("test".into()), "chat", text);
                message.files.push(file.clone());
                tx.send(message).await.unwrap();
            }
            // Wait until the gateway has received every input, while generation is blocked.
            drop(tx.reserve_many(3).await.unwrap());
            release.notify_one();
            drop(tx);
            assert_eq!(
                replies.recv().await.unwrap(),
                "initial + first correction + second correction"
            );
            task.await.unwrap().unwrap();
            assert!(replies.try_recv().is_err());
            let session = runner
                .load_session(SessionId::new("test:chat"))
                .await
                .unwrap();
            let inputs: Vec<_> = session
                .messages()
                .iter()
                .filter_map(|message| match message {
                    crate::agent::message::Message::User(user) => Some(user.content.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(inputs, ["initial", "first correction", "second correction"]);
            let files: Vec<_> = session
                .messages()
                .iter()
                .filter_map(|message| match message {
                    crate::agent::message::Message::User(user) if !user.files.is_empty() => {
                        Some(&user.files[0].path)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(files, [&file.path, &file.path]);
        })
        .await
        .unwrap();
    }

    struct PausedSaveStore {
        inner: crate::agent::session::JsonSessionStore,
        saving: tokio::sync::Notify,
        release: tokio::sync::Notify,
        pause: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::agent::session::SessionStore for PausedSaveStore {
        async fn active_id(&self, id: &SessionId) -> anyhow::Result<SessionId> {
            self.inner.active_id(id).await
        }
        async fn new_session(
            &self,
            id: &SessionId,
        ) -> anyhow::Result<crate::agent::session::Session> {
            self.inner.new_session(id).await
        }
        async fn list(&self) -> anyhow::Result<Vec<crate::agent::session::Session>> {
            self.inner.list().await
        }
        async fn select(
            &self,
            conversation: &SessionId,
            id: &SessionId,
        ) -> anyhow::Result<crate::agent::session::Session> {
            self.inner.select(conversation, id).await
        }
        async fn load(
            &self,
            id: &SessionId,
        ) -> anyhow::Result<Option<crate::agent::session::Session>> {
            self.inner.load(id).await
        }
        async fn delete(&self, id: &SessionId) -> anyhow::Result<()> {
            self.inner.delete(id).await
        }
        async fn save(&self, session: &crate::agent::session::Session) -> anyhow::Result<()> {
            if self.pause.swap(false, Ordering::SeqCst) {
                self.saving.notify_one();
                self.release.notified().await;
            }
            self.inner.save(session).await
        }
    }

    #[tokio::test]
    async fn messages_during_save_continue_in_order_after_session_is_saved() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        tokio::time::timeout(Duration::from_secs(5), async {
            for command in [None, Some("/stop"), Some("/new")] {
                let release = Arc::new(tokio::sync::Notify::new());
                release.notify_one();
                let directory = tempfile::tempdir().unwrap();
                let store = Arc::new(PausedSaveStore {
                    inner: JsonSessionStore::new(directory.path()),
                    saving: Default::default(),
                    release: Default::default(),
                    pause: true.into(),
                });
                let runner = Arc::new(AgentRunner::new(
                    Arc::new(Agent::new(
                        Arc::new(SteeringLlm {
                            started: Default::default(),
                            release,
                        }),
                        ToolRegistry::default(),
                        ContextBuilder::new("test"),
                    )),
                    store.clone(),
                ));
                let (updates, mut replies) = mpsc::unbounded_channel();
                let gateway =
                    GatewayRunner::new(runner.clone()).add_interface(RecordingInterface {
                        updates,
                        sends: Arc::default(),
                        incoming: None,
                    });
                let (tx, rx) = mpsc::channel(2);
                tx.send(IncomingMessage::new(
                    InterfaceId("test".into()),
                    "chat",
                    "initial",
                ))
                .await
                .unwrap();
                let task = tokio::spawn(async move { gateway.run_conversation(rx).await });
                store.saving.notified().await;
                let file = crate::agent::FileAttachment::save(
                    &directory.path().join("attachments"),
                    "report.pdf",
                    None,
                    b"%PDF-1.7\ntest",
                )
                .unwrap();
                for text in ["first correction", "second correction"] {
                    let mut message =
                        IncomingMessage::new(InterfaceId("test".into()), "chat", text);
                    message.files.push(file.clone());
                    tx.send(message).await.unwrap();
                }
                drop(tx.reserve_many(2).await.unwrap());
                if let Some(command) = command {
                    tx.send(IncomingMessage::new(
                        InterfaceId("test".into()),
                        "chat",
                        command,
                    ))
                    .await
                    .unwrap();
                    drop(tx.reserve_many(2).await.unwrap());
                }
                store.release.notify_one();
                drop(tx);
                assert_eq!(replies.recv().await.unwrap(), "initial");
                if command.is_none() {
                    assert_eq!(
                        replies.recv().await.unwrap(),
                        "initial + first correction + second correction"
                    );
                } else if command == Some("/new") {
                    assert_eq!(replies.recv().await.unwrap(), "New session created.");
                }
                task.await.unwrap().unwrap();
                assert!(replies.try_recv().is_err());
                let session = runner
                    .load_session(SessionId::new("test:chat"))
                    .await
                    .unwrap();
                let inputs: Vec<_> = session
                    .messages()
                    .iter()
                    .filter_map(|message| match message {
                        crate::agent::message::Message::User(user) => Some(user.content.as_str()),
                        _ => None,
                    })
                    .collect();
                let expected = match command {
                    None => vec!["initial", "first correction", "second correction"],
                    Some("/new") => vec![],
                    _ => vec!["initial"],
                };
                assert_eq!(inputs, expected);
                if command.is_none() {
                    let files: Vec<_> = session
                        .messages()
                        .iter()
                        .filter_map(|message| match message {
                            crate::agent::message::Message::User(user)
                                if !user.files.is_empty() =>
                            {
                                Some(&user.files[0].path)
                            }
                            _ => None,
                        })
                        .collect();
                    assert_eq!(files, [&file.path, &file.path]);
                }
            }
        })
        .await
        .unwrap();
    }

    struct StreamingLlm {
        finish: Arc<tokio::sync::Notify>,
        fail: bool,
        tool: bool,
    }

    #[tokio::test]
    async fn commands_interrupt_running_agent_and_new_switches_session() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};

        tokio::time::timeout(Duration::from_secs(15), async {
            for command in ["/stop", "/new"] {
                let finish = Arc::new(tokio::sync::Notify::new());
                let agent = Agent::new(
                    Arc::new(StreamingLlm {
                        finish: finish.clone(),
                        fail: false,
                        tool: true,
                    }),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                );
                let directory = tempfile::tempdir().unwrap();
                let runner = Arc::new(AgentRunner::new(
                    Arc::new(agent),
                    Arc::new(JsonSessionStore::new(directory.path())),
                ));
                let conversation = SessionId::new("test:chat");
                let original = runner.load_session(conversation.clone()).await.unwrap();
                let (updates, mut messages) = mpsc::unbounded_channel();
                let (tx, rx) = mpsc::channel(10);
                let gateway =
                    GatewayRunner::new(runner.clone()).add_interface(RecordingInterface {
                        updates,
                        sends: Arc::default(),
                        incoming: Some(tokio::sync::Mutex::new(rx)),
                    });
                let task = tokio::spawn(async move { gateway.run().await });
                let event = |text| {
                    Event::Message(IncomingMessage::new(
                        InterfaceId("test".into()),
                        "chat",
                        text,
                    ))
                };
                tx.send(event("/stop")).await.unwrap();
                assert_eq!(messages.recv().await.unwrap(), "The agent is not running.");
                tx.send(event("hello")).await.unwrap();
                while !messages
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("` exec_command: pwd ` — ")
                {}

                // A command in another chat must not stop this chat's active request.
                tx.send(Event::Message(IncomingMessage::new(
                    InterfaceId("test".into()),
                    "other",
                    "/stop",
                )))
                .await
                .unwrap();
                loop {
                    let text = messages.recv().await.unwrap();
                    if text == "The agent is not running." {
                        break;
                    }
                    assert!(text == "Часть " || text == "Часть ответа");
                }
                tx.send(event("queued request")).await.unwrap();
                tx.send(event(command)).await.unwrap();
                assert_eq!(messages.recv().await.unwrap(), "Agent stopped.");
                if command == "/new" {
                    assert_eq!(messages.recv().await.unwrap(), "New session created.");
                }
                let session = runner.load_session(conversation.clone()).await.unwrap();
                if command == "/new" {
                    assert_ne!(session.id(), original.id());
                    assert!(session.messages().is_empty());
                } else {
                    assert_eq!(session.id(), original.id());
                    assert_eq!(session.messages().len(), 2);
                }
                assert_eq!(
                    runner
                        .load_session(SessionId::new("test:other"))
                        .await
                        .unwrap()
                        .id(),
                    &SessionId::new("test:other")
                );

                // An idle command is a barrier: queued work must have been discarded.
                tx.send(event("/stop")).await.unwrap();
                assert_eq!(messages.recv().await.unwrap(), "The agent is not running.");
                tx.send(event("after command")).await.unwrap();
                while !messages
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("` exec_command: pwd ` — ")
                {}
                finish.notify_one();
                loop {
                    let text = messages.recv().await.unwrap();
                    if text == "<deleted>" {
                        break;
                    }
                    assert!(text == "Часть " || text == "Часть ответа");
                }
                assert_eq!(messages.recv().await.unwrap(), "Полный ответ");
                drop(tx);
                task.await.unwrap().unwrap();
                let session = runner.load_session(conversation).await.unwrap();
                assert_eq!(
                    session.messages().len(),
                    if command == "/new" { 2 } else { 4 }
                );
            }
        })
        .await
        .unwrap();
    }

    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for StreamingLlm {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            progress: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            if self.tool {
                progress
                    .unwrap()
                    .send(AgentEvent::ToolStarted {
                        call_id: "call".into(),
                        name: "exec_command".into(),
                        arguments: serde_json::json!({"cmd": "pwd"}),
                    })
                    .await?;
            }
            for delta in ["Часть ", "ответа"] {
                progress
                    .unwrap()
                    .send(AgentEvent::TextDelta(delta.into()))
                    .await?;
            }
            self.finish.notified().await;
            anyhow::ensure!(!self.fail, "stream disconnected");
            Ok((
                vec![crate::agent::message::Message::Assistant(
                    AssistantMessage {
                        content: "Полный ответ".into(),
                    },
                )],
                Some(42),
            ))
        }
    }

    #[tokio::test]
    async fn completed_status_precedes_separate_reply_and_errors_are_visible() {
        use crate::agent::{Agent, ContextBuilder, session::JsonSessionStore, tools::ToolRegistry};
        use crate::gateway::IncomingMessage;

        tokio::time::timeout(Duration::from_secs(45), async {
            for (fail, slow, tool) in [
                (false, false, true),
                (false, true, true),
                (true, false, true),
                (false, false, false),
                (false, true, false),
            ] {
                let finish = Arc::new(tokio::sync::Notify::new());
                let llm = StreamingLlm {
                    finish: finish.clone(),
                    fail,
                    tool,
                };
                let agent = Agent::new(
                    Arc::new(llm),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                );
                let directory = tempfile::tempdir().unwrap();
                let runner = Arc::new(AgentRunner::new(
                    Arc::new(agent),
                    Arc::new(JsonSessionStore::new(directory.path())),
                ));
                let (updates, mut messages) = mpsc::unbounded_channel();
                let sends = Arc::new(AtomicUsize::new(0));
                let gateway = GatewayRunner::new(runner).add_interface(RecordingInterface {
                    updates,
                    sends: sends.clone(),
                    incoming: None,
                });
                let request = tokio::spawn(async move {
                    gateway
                        .handle_event(
                            Event::Message(IncomingMessage::new(
                                InterfaceId("test".into()),
                                "chat",
                                "hello",
                            )),
                            None,
                            None,
                        )
                        .await
                });
                loop {
                    let next = messages.recv().await.unwrap();
                    if next == "Часть ответа" {
                        break;
                    }
                    assert!(
                        next == "Часть " || (tool && next.starts_with("` exec_command: pwd ` — ")),
                        "unexpected stream update: {next}"
                    );
                }
                assert!(!request.is_finished());
                if slow {
                    tokio::time::sleep(Duration::from_secs(16)).await;
                }
                finish.notify_one();
                request.await.unwrap().unwrap();
                let final_text = loop {
                    let text = messages.recv().await.unwrap();
                    if !text.starts_with("` exec_command: pwd ` — ")
                        && text != "Часть ответа"
                        && text != "Часть "
                    {
                        break text;
                    }
                };
                if fail {
                    assert!(final_text.contains("Response interrupted: stream disconnected"));
                } else if !tool {
                    assert_eq!(final_text, "Полный ответ");
                } else {
                    if slow {
                        assert!(final_text.starts_with("Done — "));
                    } else {
                        assert_eq!(final_text, "<deleted>");
                    }
                    assert_eq!(messages.recv().await.unwrap(), "Полный ответ");
                }
                assert!(sends.load(Ordering::Relaxed) >= 1);
                assert!(messages.try_recv().is_err());
            }
        })
        .await
        .unwrap();
    }
}
