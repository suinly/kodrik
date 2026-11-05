use super::*;
use crate::remote::protocol::{ChatCommand, RemoteChatSnapshot};

const HISTORY_BYTES: usize = 1024 * 1024;

pub(super) fn group_starts(session: &Session) -> Vec<usize> {
    use crate::agent::message::Message;
    let mut starts = Vec::new();
    let mut assistant = false;
    for (index, message) in session.messages().iter().enumerate() {
        let user = matches!(message, Message::User(_));
        if user || !assistant {
            starts.push(index);
        }
        assistant = !user;
    }
    starts
}

pub(super) fn history_group(session: &Session, start: usize, end: usize) -> Value {
    let mut rows = super::super::chat_stream::history_range(session, start, end);
    let row = rows.first_mut().expect("Nonempty message group");
    if row["role"] == "user" {
        row["user_turn"] = json!(
            session.messages()[..start]
                .iter()
                .filter(|message| matches!(message, crate::agent::message::Message::User(_)))
                .count()
        );
    }
    let parts = row["parts"].as_array_mut().unwrap();
    for part in parts {
        if let Some(url) = part["url"]
            .as_str()
            .and_then(|url| url.strip_prefix("../api/chat/"))
        {
            part["url"] = json!(format!("./api/remote/chat/{url}"));
        }
    }
    rows.remove(0)
}

fn bound_value(value: &mut Value, max: usize) {
    if serde_json::to_vec(value).is_ok_and(|v| v.len() > max) {
        *value = json!({"truncated":true,"message":"Full result remains in server history"});
    }
}

fn bound_control_text(value: &mut Value) {
    if let Some(text) = value.as_str().filter(|text| text.len() > 4096) {
        *value = json!(format!("{}…", text.chars().take(1024).collect::<String>()));
    }
}

pub(super) fn project_group(row: &mut Value, id: &str) {
    let index = row["id"]
        .as_str()
        .and_then(|id| id.rsplit(':').next())
        .unwrap_or("0");
    let reference = format!("./api/remote/chat/sessions/{id}/history/{index}");
    if let Some(parts) = row["parts"].as_array_mut() {
        for part in parts {
            if let Some(url) = part["url"]
                .as_str()
                .and_then(|url| url.strip_prefix("../api/chat/"))
            {
                part["url"] = json!(format!("./api/remote/chat/{url}"));
            }
            if serde_json::to_vec(part).is_ok_and(|bytes| bytes.len() > 16 * 1024) {
                let mut projection = json!({"truncated":true,"reference":reference});
                if part["result_message"].is_u64() {
                    projection["result_message"] = part["result_message"].clone();
                }
                for key in [
                    "type",
                    "toolCallId",
                    "toolName",
                    "state",
                    "mediaType",
                    "filename",
                ] {
                    if let Some(value) = part
                        .get(key)
                        .filter(|value| value.as_str().is_some_and(|s| s.len() <= 256))
                    {
                        projection[key] = value.clone();
                    }
                }
                if let Some(text) = part["text"].as_str() {
                    projection["text"] = json!(text.chars().take(2048).collect::<String>());
                }
                *part = projection;
            }
        }
    }
    if serde_json::to_vec(row).is_ok_and(|bytes| bytes.len() > 64 * 1024) {
        row["parts"] = json!([{"type":"text","text":"Message group truncated","truncated":true,"reference":reference}]);
        row.as_object_mut().unwrap().remove("versions");
    }
}

impl Chat {
    pub(super) async fn decorate_origins(
        &self,
        id: &str,
        session: &Session,
        snapshot: &mut Value,
    ) -> anyhow::Result<()> {
        let calls: Vec<_> = snapshot["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|row| row["parts"].as_array().into_iter().flatten())
            .filter(|part| {
                part["type"] == "dynamic-tool"
                    && part["toolName"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("local__"))
            })
            .filter_map(|part| {
                Some((
                    part["toolCallId"].as_str()?.to_owned(),
                    part["toolName"].as_str()?.to_owned(),
                ))
            })
            .collect();
        let origins = self
            .sessions
            .remote_history_origins(&SessionId::new(id), calls)
            .await?;
        for row in snapshot["messages"].as_array_mut().into_iter().flatten() {
            let mut files = Vec::new();
            for part in row["parts"].as_array_mut().into_iter().flatten() {
                if let Some(origin) = part["toolCallId"]
                    .as_str()
                    .and_then(|call| origins.get(call))
                {
                    part["origin"] = origin.clone();
                    if part["type"] == "dynamic-tool"
                        && let Some(index) = part["result_message"].as_u64()
                    {
                        let saved =
                            session
                                .messages()
                                .get(index as usize)
                                .and_then(|item| match item {
                                    crate::agent::message::Message::ToolResult(result) => {
                                        Some(result)
                                    }
                                    _ => None,
                                });
                        let call = session
                            .messages()
                            .iter()
                            .take(index as usize)
                            .rev()
                            .find_map(|item| match item {
                                crate::agent::message::Message::ToolCall(call)
                                    if part["toolCallId"] == call.call_id =>
                                {
                                    Some(call.clone())
                                }
                                _ => None,
                            });
                        if let (Some(result), Some(call)) = (saved, call)
                            && let Some(file) = self
                                .sessions
                                .remote_saved_file(
                                    &SessionId::new(id),
                                    call,
                                    result.content.output.clone(),
                                )
                                .await?
                        {
                            part["attachment_saved"] = json!(true);
                            files.push(json!({"type":"file","toolCallId":part["toolCallId"],"origin":origin,"filename":file.filename,"mediaType":file.mime_type,"url":format!("../api/chat/sessions/{id}/attachments/{index}/tool_files/0")}));
                        }
                    }
                }
            }
            if let Some(parts) = row["parts"].as_array_mut() {
                parts.extend(files)
            }
        }
        Ok(())
    }

    pub(super) async fn create_conversation(&self) -> Result<Value, Response> {
        let session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        self.sessions.save(&session).await.map_err(internal)?;
        Ok(
            json!({"id":session.id().as_str(), "title":"Новый диалог", "title_default":true, "busy":false, "updated_at":null}),
        )
    }

    pub(super) async fn stop_conversation(&self, id: String) -> Result<(), Response> {
        let chat = self.clone();
        tokio::spawn(async move {
            let _runs = chat.runs.lock().await;
            chat.session(&id).await?;
            chat.stop(&id).await.map_err(internal)
        })
        .await
        .map_err(|e| internal(e.into()))?
    }

    pub(crate) async fn remote_snapshot(
        &self,
        id: &SessionId,
    ) -> Result<RemoteChatSnapshot, Response> {
        self.remote_page(id, None).await
    }

    pub(crate) async fn remote_history(
        &self,
        id: &SessionId,
        cursor: &str,
    ) -> Result<RemoteChatSnapshot, Response> {
        let (session, index) = cursor
            .rsplit_once(':')
            .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Invalid history cursor"))?;
        if session != id.as_str() {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Cursor belongs to another session",
            ));
        }
        let end = index
            .parse()
            .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid history cursor"))?;
        self.remote_page(id, Some(end)).await
    }

    async fn remote_page(
        &self,
        id: &SessionId,
        end: Option<usize>,
    ) -> Result<RemoteChatSnapshot, Response> {
        let session = self.session(id.as_str()).await?;
        if session.webhook_instruction.is_some() {
            return Err(error(StatusCode::NOT_FOUND, "Conversation not found"));
        }
        let view = self
            .sessions
            .conversation_view(id.as_str())
            .await
            .map_err(internal)?;
        let starts = group_starts(&view.session);
        let end = end.unwrap_or(starts.len());
        if end > starts.len() {
            return Err(error(StatusCode::BAD_REQUEST, "Invalid history cursor"));
        }
        let mut messages = Vec::new();
        let mut start = end;
        let mut remaining = HISTORY_BYTES - 64 * 1024;
        while start > 0 && messages.len() < 200 {
            let group_end = starts
                .get(start)
                .copied()
                .unwrap_or(view.session.messages().len());
            let mut row = history_group(&view.session, starts[start - 1], group_end);
            project_group(&mut row, id.as_str());
            let size = serde_json::to_vec(&row)
                .map_err(|e| internal(e.into()))?
                .len();
            if size + 1 > remaining {
                break;
            }
            remaining -= size + 1;
            messages.push(row);
            start -= 1;
        }
        messages.reverse();
        let mut decorated = json!({"messages":messages});
        self.decorate_versions(id.as_str(), &mut decorated)
            .await
            .map_err(internal)?;
        self.decorate_origins(id.as_str(), &view.session, &mut decorated)
            .await
            .map_err(internal)?;
        let mut messages: Vec<Value> = decorated["messages"]
            .as_array_mut()
            .unwrap()
            .drain(..)
            .collect();
        for row in &mut messages {
            project_group(row, id.as_str());
        }
        // Branch metadata counts against the same wire budget as message content.
        while serde_json::to_vec(&messages)
            .map_err(|e| internal(e.into()))?
            .len()
            > HISTORY_BYTES - 64 * 1024
        {
            messages.remove(0);
            start += 1;
        }
        let versions = Value::Null;
        let mut run = json!({"busy":view.busy,"blocked_reason":view.blocked_reason,
            "clear_completed_plan":!view.queued.is_empty(),
            "model":self.runner.model_identity(),
            "compaction":view.run.as_ref().and_then(|r| r.compaction.as_ref()),
            "phase":view.run.as_ref().map(|r| &r.run.phase),
            "live":view.run.as_ref().map(|r| &r.live),
            "remote_operations":view.run.as_ref().map(|r| &r.remote_operations),
            "command_approvals":self.sessions.pending_command_approvals(id.as_str()).await.map_err(internal)?,
            "command_audit_mode":self.sessions.command_audit_mode(id.as_str()).await.map_err(internal)?});
        // Output budgets must never erase busy/phase or silently discard pending controls.
        for field in ["live", "remote_operations", "compaction"] {
            bound_value(&mut run[field], 32 * 1024);
        }
        for field in ["model", "blocked_reason"] {
            bound_control_text(&mut run[field]);
        }
        if let Some(phase) = run["phase"].as_object_mut() {
            for value in phase.values_mut() {
                if let Some(fields) = value.as_object_mut() {
                    for value in fields.values_mut() {
                        bound_control_text(value);
                    }
                }
            }
        }
        let approvals = run["command_approvals"].as_array_mut().unwrap();
        let count = approvals.len();
        approvals.truncate(1); // The TUI reviews one exact invocation at a time.
        let unavailable = approvals
            .first()
            .filter(|approval| {
                serde_json::to_vec(approval).is_ok_and(|bytes| bytes.len() > 512 * 1024)
            })
            .map(|approval| json!({"id":approval["id"],"tool_name":approval["tool_name"]}));
        if unavailable.is_some() {
            approvals.clear();
        }
        run["pending_approval_count"] = json!(count);
        run["approval_unavailable"] = json!(unavailable);
        let environment = match self.runner.environments() {
            Some(service) => service.status(id).await,
            None => None,
        };
        Ok(RemoteChatSnapshot {
            session_id: id.clone(),
            revision: view.revision,
            environment_revision: self.environment_revision(),
            messages,
            previous_cursor: (start > 0).then(|| format!("{}:{start}", id.as_str())),
            run: Some(run),
            versions,
            environment,
        })
    }

    pub(super) async fn remote_sessions(&self) -> Result<Value, Response> {
        management::list_sessions(
            self,
            management::Search {
                q: String::new(),
                limit: 100,
                offset: 0,
                webhooks: false,
            },
        )
        .await
    }

    pub(super) async fn remote_command(
        &self,
        id: &SessionId,
        command: ChatCommand,
    ) -> Result<Value, Response> {
        if !matches!(command, ChatCommand::New) {
            let session = self.session(id.as_str()).await?;
            if session.webhook_instruction.is_some() {
                return Err(error(StatusCode::NOT_FOUND, "Conversation not found"));
            }
        }
        match command {
            ChatCommand::SubmissionStatus { request_id } => {
                let request = request_id.to_string();
                let mut submission = self
                    .sessions
                    .submission(web_conversation(id.as_str()).as_str(), &request)
                    .await
                    .map_err(internal)?;
                if submission.is_none() {
                    let original = id.as_str().to_owned();
                    let branch = request.clone();
                    let belongs = self
                        .sessions
                        .with_connection(move |db| {
                            let root = crate::agent::session::branches::root(db, &original)?;
                            Ok(crate::agent::session::branches::read_branches(db)?
                                .iter()
                                .any(|candidate| candidate.id == branch && candidate.root == root))
                        })
                        .await
                        .map_err(internal)?;
                    if belongs {
                        submission = self
                            .sessions
                            .submission(&request, &request)
                            .await
                            .map_err(internal)?;
                    }
                }
                Ok(submission.map_or(
                    Value::Null,
                    |record| json!({"status":record.status,"session_id":record.session_id}),
                ))
            }
            ChatCommand::Send { request_id, text } => {
                let input = Input {
                    id: id.as_str().into(),
                    request_id: request_id.to_string(),
                    text,
                };
                tokio::spawn(admit_message(self.clone(), input, vec![], true))
                    .await
                    .map_err(|e| internal(e.into()))?
            }
            ChatCommand::New => self.create_conversation().await,
            ChatCommand::Stop => {
                self.stop_conversation(id.as_str().into()).await?;
                Ok(Value::Null)
            }
            ChatCommand::Select { session_id } => {
                let session = self.session(session_id.as_str()).await?;
                if session.webhook_instruction.is_some() {
                    return Err(StatusCode::NOT_FOUND.into_response());
                }
                // Native selection is local to its TUI, not the browser's branch selector.
                Ok(json!({"id":session_id}))
            }
            ChatCommand::SelectVersion {
                session_id,
                user_turn,
            } => {
                let session = self.session(id.as_str()).await?;
                let index = session
                    .messages()
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| matches!(m, crate::agent::message::Message::User(_)))
                    .nth(user_turn)
                    .map(|(index, _)| index)
                    .ok_or_else(|| error(StatusCode::BAD_REQUEST, "User turn not found"))?;
                let branches = self
                    .sessions
                    .with_connection(|db| crate::agent::session::branches::read_branches(db))
                    .await
                    .map_err(internal)?;
                let versions = crate::agent::session::branches::message_versions(
                    id.as_str(),
                    index,
                    &branches,
                );
                if !versions
                    .is_some_and(|v| v.ids.iter().any(|version| version == session_id.as_str()))
                {
                    return Err(error(StatusCode::BAD_REQUEST, "Version not found"));
                }
                let selected = self.session(session_id.as_str()).await?;
                if selected.webhook_instruction.is_some() {
                    return Err(StatusCode::NOT_FOUND.into_response());
                }
                Ok(json!({"id":session_id}))
            }
            ChatCommand::Edit {
                request_id,
                user_turn,
                text,
            } => {
                let session = self.session(id.as_str()).await?;
                let index = session
                    .messages()
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| matches!(m, crate::agent::message::Message::User(_)))
                    .nth(user_turn)
                    .map(|(i, _)| i)
                    .ok_or_else(|| error(StatusCode::BAD_REQUEST, "User turn not found"))?;
                management::edit_message(
                    self.clone(),
                    id.as_str().into(),
                    index,
                    management::Edit {
                        request_id: request_id.to_string(),
                        text,
                    },
                    false,
                )
                .await
            }
            ChatCommand::DecideApproval { approval_id, allow } => {
                self.decide_approval(id.as_str().into(), approval_id, allow)
                    .await?;
                Ok(Value::Null)
            }
            ChatCommand::TrustApproval { approval_id } => {
                self.sessions
                    .disable_command_audit_for_approval(id.as_str(), &approval_id)
                    .await
                    .map_err(internal)?;
                Ok(Value::Null)
            }
            ChatCommand::SetAuditMode { mode } => {
                let mode = serde_json::from_value(json!(mode))
                    .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid audit mode"))?;
                self.set_audit_mode(id.as_str().into(), mode).await?;
                Ok(Value::Null)
            }
        }
    }
}
