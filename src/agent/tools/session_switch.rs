use super::{ReplayPolicy, Tool, ToolOutput};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffRequest {
    /// Existing personal session in this user's store; omit when create_new is true.
    #[serde(default)]
    pub session_id: String,
    /// Create a new personal dialogue instead of selecting an existing session.
    #[serde(default)]
    pub create_new: bool,
    /// Task, decisions, progress, and already performed effects. Do not ask to replay effects.
    pub context: String,
}

pub(crate) struct SwitchSessionTool;

#[async_trait::async_trait]
impl Tool for SwitchSessionTool {
    fn name(&self) -> String {
        "switch_session".into()
    }
    fn description(&self) -> String {
        "Move this task to an existing suitable personal session, or set create_new=true and omit session_id to create a new dialogue for an unrelated topic. Its history becomes active and later messages stay there. Supply task context, progress and already performed effects; never replay them. Runs only after all calls in this batch finish. Not available to children, wakeups, isolated, group or guest runs.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        schema_for!(HandoffRequest).to_value()
    }
    fn strict_schema(&self) -> bool {
        // Session creation omits session_id; strict mode requires every property.
        false
    }
    fn replay_policy(&self) -> ReplayPolicy {
        ReplayPolicy::Safe
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        anyhow::bail!("switch_session requires a normal main-agent durable run")
    }
}

pub(crate) async fn validate(
    cp: &crate::agent::run_state::Checkpoint,
    source: &crate::agent::session::Session,
    args: serde_json::Value,
) -> anyhow::Result<(HandoffRequest, ToolOutput)> {
    use crate::agent::session::{SessionId, SessionStore, handoff::eligible};
    let mut request: HandoffRequest = serde_json::from_value(args)?;
    anyhow::ensure!(
        !request.context.trim().is_empty() && request.context.len() <= 32_768,
        "Context must contain 1–32768 bytes"
    );
    anyhow::ensure!(
        cp.record.handoff_hops < 4 && cp.record.pending_handoff.is_none(),
        "Session handoff limit reached or already pending"
    );
    anyhow::ensure!(
        cp.record.isolated == Some(false)
            && !cp.record.conversation_id.contains(":guest")
            && !cp.record.conversation_id.starts_with("companion:"),
        "Session handoff is restricted to normal personal runs"
    );
    let target = if request.create_new {
        anyhow::ensure!(
            request.session_id.is_empty(),
            "Omit session_id when creating a new dialogue"
        );
        request.session_id = uuid::Uuid::new_v4().to_string();
        crate::agent::session::Session::new(SessionId::new(&request.session_id))
    } else {
        cp.store
            .load(&SessionId::new(&request.session_id))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Target session not found in this user's store"))?
    };
    anyhow::ensure!(
        eligible(source, &target),
        "Session handoff is not allowed between these sessions"
    );
    Ok((request, serde_json::json!({"handoff":"pending", "note":"The session changes after this tool batch; do not replay completed effects."}).into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        run_state::Checkpoint,
        session::{Session, SessionId, SessionStore, SqliteSessionStore},
    };

    #[test]
    fn advertised_schema_supports_optional_handoff_fields() {
        let tool = SwitchSessionTool;
        let schema = tool.parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], serde_json::json!(["context"]));
        assert!(!tool.strict_schema());
    }

    #[tokio::test]
    async fn creation_request_is_validated_without_creating_until_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let source = Session::new(SessionId::new("source"));
        let mut cp = Checkpoint::new(&store, source.id(), &source);
        cp.record.isolated = Some(false);
        let (request, _) = validate(
            &cp,
            &source,
            serde_json::json!({"create_new":true,"context":"new topic"}),
        )
        .await
        .unwrap();
        assert!(!request.session_id.is_empty());
        assert!(
            store
                .load(&SessionId::new(request.session_id))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn new_dialogue_rejects_restricted_sources_and_conflicting_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        for id in [
            "telegram:bot:-100",
            "telegram:bot:1:guest",
            "cli:once:1",
            "companion:wakeup:1",
            "webhook:1",
        ] {
            let source = Session::new(SessionId::new(id));
            let mut cp = Checkpoint::new(&store, source.id(), &source);
            cp.record.isolated = Some(false);
            assert!(
                validate(
                    &cp,
                    &source,
                    serde_json::json!({"create_new":true,"context":"task"})
                )
                .await
                .is_err(),
                "{id}"
            );
        }
        let source = Session::new(SessionId::new("source"));
        let mut cp = Checkpoint::new(&store, source.id(), &source);
        cp.record.isolated = Some(false);
        assert!(
            validate(
                &cp,
                &source,
                serde_json::json!({"create_new":true,"session_id":"target","context":"task"})
            )
            .await
            .is_err()
        );
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn validation_restricts_scope_hops_and_malformed_context_without_effects() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let source = Session::new(SessionId::new("source"));
        let target = Session::new(SessionId::new("target"));
        store.save(&target).await.unwrap();
        let mut cp = Checkpoint::new(&store, source.id(), &source);
        let args = serde_json::json!({"session_id":"target","context":"task progress"});
        assert!(validate(&cp, &source, args.clone()).await.is_err());
        cp.record.isolated = Some(false);
        assert!(validate(&cp, &source, args.clone()).await.is_ok());
        assert_eq!(store.active_id(source.id()).await.unwrap(), *source.id());
        assert!(cp.record.pending_handoff.is_none());
        cp.record.handoff_hops = 4;
        assert!(validate(&cp, &source, args.clone()).await.is_err());
        cp.record.handoff_hops = 0;
        for conversation in ["telegram:bot:1:guest", "companion:wakeup:1"] {
            cp.record.conversation_id = conversation.into();
            assert!(validate(&cp, &source, args.clone()).await.is_err());
        }
        cp.record.conversation_id = "source".into();
        for bad in [
            serde_json::json!({"session_id":"target","context":" "}),
            serde_json::json!({"session_id":"target","context":"x".repeat(32769)}),
            serde_json::json!({"session_id":"missing","context":"task"}),
        ] {
            assert!(validate(&cp, &source, bad).await.is_err());
        }
    }
}
