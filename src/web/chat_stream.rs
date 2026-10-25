use crate::agent::{message::Message, session::Session};
use serde_json::{Value, json};

pub(super) fn history(session: &Session) -> Vec<Value> {
    let mut messages: Vec<Value> = vec![];
    for (index, message) in session.messages().iter().enumerate() {
        if let Message::User(user) = message {
            let mut text = json!({"type":"text", "text":user.content});
            if session.is_webhook_message(index) {
                text["format"] = json!("code");
            }
            let mut parts = vec![text];
            for (file_index, file) in user.files.iter().enumerate() {
                parts.push(json!({"type":"file", "filename":file.filename, "mediaType":file.mime_type,
                    "url":format!("../api/chat/sessions/{}/attachments/{index}/files/{file_index}", session.id().as_str())}));
            }
            for (image_index, image) in user.images.iter().enumerate() {
                let mime = image
                    .data_url
                    .strip_prefix("data:")
                    .and_then(|s| s.split(';').next())
                    .unwrap_or("image/png");
                parts.push(json!({"type":"file", "filename":format!("Фото {}",image_index + 1), "mediaType":mime,
                    "url":format!("../api/chat/sessions/{}/attachments/{index}/images/{image_index}", session.id().as_str())}));
            }
            messages.push(
                json!({"id":format!("{}:{index}", session.id().as_str()), "role":"user",
                "parts":parts,"created_at":session.message_time(index)}),
            );
            continue;
        }
        if messages.last().is_none_or(|m| m["role"] != "assistant") {
            messages.push(json!({"id":format!("{}:{index}", session.id().as_str()), "role":"assistant", "parts":[], "created_at":session.message_time(index)}));
        }
        if matches!(message, Message::Assistant(_)) {
            messages.last_mut().unwrap()["created_at"] = json!(session.message_time(index));
        }
        let parts = messages.last_mut().unwrap()["parts"]
            .as_array_mut()
            .unwrap();
        match message {
            Message::Assistant(message) => {
                parts.push(json!({"type":"text", "text":message.content}))
            }
            Message::ToolCall(call) => parts
                .push(json!({"type":"dynamic-tool", "toolCallId":call.call_id,
                "toolName":call.name, "input":call.arguments, "state":"input-available"})),
            Message::ToolResult(result) => {
                if let Some(part) = parts.iter_mut().find(|p| p["toolCallId"] == result.call_id) {
                    let child_outcome = matches!(
                        part["toolName"].as_str(),
                        Some("spawn_agent" | "send_message" | "wait_agent" | "stop_agent")
                    ) && result.content.output["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with(crate::agent::subagents::PREFIX))
                        && serde_json::from_value::<crate::agent::subagents::Status>(
                            result.content.output["status"].clone(),
                        )
                        .is_ok();
                    if result
                        .content
                        .output
                        .get("error")
                        .is_some_and(|value| !value.is_null())
                        && !child_outcome
                    {
                        part["state"] = json!("output-error");
                        part["errorText"] = json!(result.content.output.to_string());
                    } else {
                        part["state"] = json!("output-available");
                        part["output"] = result.content.output.clone();
                    }
                }
            }
            Message::User(_) => unreachable!(),
        }
    }
    messages
}

pub(crate) struct SnapshotProjection {
    run_id: String,
    revision: Option<i64>,
}
impl SnapshotProjection {
    pub(crate) fn new(run_id: String) -> Self {
        Self {
            run_id,
            revision: None,
        }
    }
    fn accept(&mut self, id: &str, revision: i64) -> bool {
        if id != self.run_id || self.revision.is_some_and(|last| revision <= last) {
            return false;
        }
        self.revision = Some(revision);
        true
    }
    pub(crate) fn snapshot(&mut self, view: &crate::agent::run_view::RunView) -> Option<Value> {
        if !self.accept(&view.run.id, view.revision) {
            return None;
        }
        let mut live = serde_json::to_value(&view.live).expect("Live view is serializable");
        if view
            .live
            .current
            .as_ref()
            .is_some_and(|p| p.status == crate::agent::run_view::PartialStatus::Committed)
        {
            live["current"] = Value::Null;
        }
        Some(
            json!({"type":"snapshot","run_id":view.run.id,"session_id":view.run.session_id,
            "revision":view.revision,"phase":view.run.phase,"cancel_requested":view.run.cancel_requested,
            "messages":history(&view.session),"plan":view.plan,"live":live,
            "queued":view.queued,"deliveries":view.deliveries,"compaction":view.compaction}),
        )
    }
}

#[cfg(test)]
mod run_view_tests {
    use super::*;
    #[test]
    fn child_outcomes_are_preserved_and_null_errors_do_not_fail_tools() {
        use crate::agent::{
            message::{ToolCall, ToolResult},
            session::SessionId,
        };
        let mut session = Session::new(SessionId::new("parent"));
        for (index, output) in [
            json!({"id":"companion:subagent:one","status":"running","result":null,"error":null}),
            json!({"id":"companion:subagent:two","status":"failed","result":null,"error":"Child request budget exhausted"}),
            json!({"error":"Unknown child agent"}),
        ].into_iter().enumerate() {
            session.push(Message::ToolCall(ToolCall {call_id:index.to_string(),name:"wait_agent".into(),arguments:json!({})}));
            session.push(Message::ToolResult(ToolResult {call_id:index.to_string(),content:output.into()}));
        }
        let parts = history(&session)[0]["parts"].as_array().unwrap().clone();
        assert_eq!(parts[0]["state"], "output-available");
        assert_eq!(parts[0]["output"]["status"], "running");
        assert_eq!(parts[1]["state"], "output-available");
        assert_eq!(parts[1]["output"]["status"], "failed");
        assert_eq!(parts[2]["state"], "output-error");
    }
    #[test]
    fn message_timestamps_survive_reload_edits_and_legacy_history() {
        use crate::agent::{AssistantMessage, UserMessage, session::SessionId};
        let mut session = Session::new(SessionId::new("timestamps"));
        session.push(Message::User(UserMessage {
            content: "question".into(),
            images: vec![],
            files: vec![],
        }));
        session.push(Message::Assistant(AssistantMessage {
            content: "answer".into(),
        }));
        let shown = history(&session);
        assert!(shown[0]["created_at"].is_string());
        assert!(shown[1]["created_at"].is_string());
        let mut saved = serde_json::to_value(&session).unwrap();
        let restored: Session = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(history(&restored), shown);
        let mut branch = restored.fork_before(SessionId::new("branch"), 0).unwrap();
        assert!(branch.message_time(0).is_none());
        branch.push(Message::User(UserMessage {
            content: "edited".into(),
            images: vec![],
            files: vec![],
        }));
        assert!(history(&branch)[0]["created_at"].is_string());
        let mut truncated = restored.clone();
        truncated.truncate_view(1).unwrap();
        assert!(truncated.message_time(1).is_none());
        assert_eq!(history(&truncated)[0], shown[0]);
        saved.as_object_mut().unwrap().remove("message_times");
        let mut legacy: Session = serde_json::from_value(saved).unwrap();
        legacy.push(Message::User(UserMessage {
            content: "new question".into(),
            images: vec![],
            files: vec![],
        }));
        let shown = history(&legacy);
        assert!(shown[0]["created_at"].is_null());
        assert!(shown[1]["created_at"].is_null());
        assert!(shown[2]["created_at"].is_string());
    }

    #[test]
    fn chat_stream_snapshot_revision_rejects_duplicates_and_old_frames() {
        let mut projection = SnapshotProjection::new("run".into());
        assert!(projection.accept("run", 5));
        assert!(!projection.accept("run", 5));
        assert!(!projection.accept("run", 4));
        assert!(!projection.accept("other", 100));
        assert!(projection.accept("run", 6));
    }
    #[tokio::test]
    async fn chat_stream_snapshot_terminal_uses_canonical_history() {
        use crate::agent::{
            run_state::{Checkpoint, RunPhase},
            run_view::PartialStatus,
            session::{Session, SessionId, SqliteSessionStore},
        };
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let mut session = Session::new(SessionId::new("cli:view"));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.model_attempts = 1;
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        store
            .save_live(&cp.record.id, 1, "partial".into(), PartialStatus::Committed)
            .await
            .unwrap();
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: "final".into(),
        }));
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        let view = store.run_view(&cp.record.id).await.unwrap();
        let mut projection = SnapshotProjection::new(cp.record.id.clone());
        let snapshot = projection.snapshot(&view).unwrap();
        assert_eq!(snapshot["messages"], json!(history(&session)));
        assert!(snapshot["live"]["current"].is_null());
    }
}
