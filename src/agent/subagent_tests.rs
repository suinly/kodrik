use super::*;
use crate::agent::{
    ContextBuilder, LlmContext,
    llm::LlmProvider,
    message::{AssistantMessage, Message},
    session::SessionStore,
    tools::{Tool, ToolRegistry},
};
use tokio::sync::{Semaphore, mpsc};

struct GatedModel {
    seen: mpsc::UnboundedSender<(String, Vec<String>)>,
    gates: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl LlmProvider for GatedModel {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let users: Vec<String> = context
            .messages
            .iter()
            .filter_map(|message| {
                if let Message::User(user) = message {
                    Some(user.content.clone())
                } else {
                    None
                }
            })
            .collect();
        self.seen
            .send((context.instructions.to_string(), users.clone()))
            .unwrap();
        self.gates.acquire().await.unwrap().forget();
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: users.join(" | "),
            })],
            None,
        ))
    }
}

async fn setup() -> (
    tempfile::TempDir,
    Arc<Subagents>,
    ParentContext,
    watch::Sender<bool>,
    mpsc::UnboundedReceiver<(String, Vec<String>)>,
    Arc<Semaphore>,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let (seen, observed) = mpsc::unbounded_channel();
    let gates = Arc::new(Semaphore::new(0));
    let agent = Arc::new(
        Agent::new(
            Arc::new(GatedModel {
                seen,
                gates: gates.clone(),
            }),
            ToolRegistry::default(),
            ContextBuilder::new("worker"),
        )
        .child_agent(),
    );
    let manager = Subagents::new(agent, store).await.unwrap();
    let mut session = Session::new(SessionId::new("parent"));
    ContextBuilder::new("PARENT RULES")
        .initialize_session(&mut session)
        .unwrap();
    session.push(Message::User(crate::agent::UserMessage {
        content: "PRIVATE OLD CONVERSATION".into(),
        files: vec![],
        images: vec![],
    }));
    let (finished, receiver) = watch::channel(false);
    let parent = ParentContext {
        session,
        finished: receiver,
    };
    (dir, manager, parent, finished, observed, gates)
}

async fn observed(
    receiver: &mut mpsc::UnboundedReceiver<(String, Vec<String>)>,
) -> (String, Vec<String>) {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn children_run_in_parallel_and_keep_parent_rules_without_copying_history() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let first = manager.spawn(&parent, "first".into()).await.unwrap();
    let second = manager.spawn(&parent, "second".into()).await.unwrap();
    let a = observed(&mut seen).await;
    let b = observed(&mut seen).await;
    assert!(a.0.contains("PARENT RULES"));
    assert!(b.0.contains("PARENT RULES"));
    assert!(!a.0.contains("PRIVATE OLD CONVERSATION"));
    let mut tasks = vec![a.1, b.1];
    tasks.sort();
    assert_eq!(tasks, vec![vec!["first"], vec!["second"]]);
    gates.add_permits(2);
    for id in [&first.id, &second.id] {
        let child = manager.wait(&parent.session, id, 3000).await.unwrap();
        assert_eq!(child.status, Status::Completed);
        assert!(child.result.is_some());
    }
}

#[tokio::test]
async fn steering_is_persisted_and_completed_children_accept_followups() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager.spawn(&parent, "initial".into()).await.unwrap();
    observed(&mut seen).await;
    manager
        .send(&parent, &child.id, "correction".into())
        .await
        .unwrap();
    gates.add_permits(1);
    let (_, users) = observed(&mut seen).await;
    assert_eq!(users, ["initial", "correction"]);
    gates.add_permits(1);
    let completed = manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    assert_eq!(completed.result.as_deref(), Some("initial | correction"));
    manager
        .send(&parent, &child.id, "followup".into())
        .await
        .unwrap();
    let (_, users) = observed(&mut seen).await;
    assert_eq!(users, ["initial", "correction", "followup"]);
    gates.add_permits(1);
    assert_eq!(
        manager
            .wait(&parent.session, &child.id, 3000)
            .await
            .unwrap()
            .result
            .as_deref(),
        Some("initial | correction | followup")
    );
}

#[tokio::test]
async fn child_access_is_bound_to_the_parent_session() {
    let (_dir, manager, parent, _finished, mut seen, _gates) = setup().await;
    let child = manager.spawn(&parent, "secret".into()).await.unwrap();
    observed(&mut seen).await;
    let mut other = parent.clone();
    other.session = Session::new(SessionId::new("other"));
    assert!(manager.list(&other.session).await.unwrap().is_empty());
    assert!(
        manager
            .send(&other, &child.id, "steal".into())
            .await
            .is_err()
    );
    assert!(manager.wait(&other.session, &child.id, 1).await.is_err());
    assert!(manager.stop(&other.session, &child.id).await.is_err());
    assert_eq!(
        manager
            .stop(&parent.session, &child.id)
            .await
            .unwrap()
            .status,
        Status::Stopped
    );
}

#[tokio::test]
async fn parent_completion_and_explicit_stop_cancel_active_children() {
    let (_dir, manager, parent, finished, mut seen, _gates) = setup().await;
    let first = manager.spawn(&parent, "first".into()).await.unwrap();
    observed(&mut seen).await;
    assert_eq!(
        manager
            .wait(&parent.session, &first.id, 1)
            .await
            .unwrap()
            .status,
        Status::Running
    );
    assert_eq!(
        manager
            .stop(&parent.session, &first.id)
            .await
            .unwrap()
            .status,
        Status::Stopped
    );
    let second = manager.spawn(&parent, "second".into()).await.unwrap();
    observed(&mut seen).await;
    drop(finished);
    assert_eq!(
        manager
            .wait(&parent.session, &second.id, 3000)
            .await
            .unwrap()
            .status,
        Status::Stopped
    );
    assert!(manager.spawn(&parent, "too late".into()).await.is_err());
}

#[tokio::test]
async fn concurrency_and_input_limits_reject_work_before_execution() {
    let (_dir, manager, parent, _finished, mut seen, _gates) = setup().await;
    assert!(manager.spawn(&parent, " ".into()).await.is_err());
    assert!(manager.spawn(&parent, "x".repeat(16_385)).await.is_err());
    for task in ["one", "two", "three"] {
        manager.spawn(&parent, task.into()).await.unwrap();
        observed(&mut seen).await;
    }
    assert!(manager.spawn(&parent, "four".into()).await.is_err());
    assert_eq!(manager.list(&parent.session).await.unwrap().len(), 3);
    for child in manager.list(&parent.session).await.unwrap() {
        manager.stop(&parent.session, &child.id).await.unwrap();
    }
}

#[tokio::test]
async fn interrupted_children_are_not_replayed_and_accept_explicit_followups() {
    use crate::agent::{
        message::ToolCall,
        run_state::{Checkpoint, RunPhase, ToolExecution},
        tools::ReplayPolicy,
    };
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let id = format!("{PREFIX}interrupted");
    let mut session = parent.session.child(SessionId::new(&id));
    manager.store.save(&session).await.unwrap();
    let submission = manager.submit(&id, "original".into()).await.unwrap().id;
    session.push(Message::User(crate::agent::UserMessage {
        content: "original".into(),
        files: vec![],
        images: vec![],
    }));
    let call = ToolCall {
        call_id: "unsafe".into(),
        name: "apply_patch".into(),
        arguments: serde_json::json!({}),
    };
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&manager.store, session.id(), &session);
    cp.record.submission_ids.push(submission.clone());
    cp.record.isolated = Some(false);
    cp.record.pending_calls.push(call.clone());
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay: ReplayPolicy::Never,
        },
        Some(ToolExecution {
            call,
            replay: ReplayPolicy::Never,
            result: None,
        }),
    )
    .await
    .unwrap();
    manager
        .save(&Child {
            task: String::new(),
            id: id.clone(),
            status: Status::Running,
            result: None,
            error: None,
            parent: parent.session.id().as_str().into(),
            generation: parent.session.generation().into(),
            submission,
        })
        .await
        .unwrap();
    let restarted = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    let child = restarted.wait(&parent.session, &id, 1).await.unwrap();
    assert_eq!(child.status, Status::Interrupted);
    assert!(
        seen.try_recv().is_err(),
        "recovery must not execute the model or unsafe tool"
    );
    assert!(matches!(
        manager
            .store
            .load_run(&cp.record.id)
            .await
            .unwrap()
            .unwrap()
            .0
            .phase,
        RunPhase::Cancelled
    ));
    restarted
        .send(&parent, &id, "inspect before continuing".into())
        .await
        .unwrap();
    let (_, users) = observed(&mut seen).await;
    assert_eq!(users, ["original", "inspect before continuing"]);
    gates.add_permits(1);
    assert_eq!(
        restarted
            .wait(&parent.session, &id, 3000)
            .await
            .unwrap()
            .status,
        Status::Completed
    );
}

#[tokio::test]
async fn a_suspended_worker_preserves_history_and_does_not_block_future_turns() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager.spawn(&parent, "initial".into()).await.unwrap();
    observed(&mut seen).await;
    manager.runner.suspend();
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    let restarted = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    restarted
        .send(&parent, &child.id, "continue carefully".into())
        .await
        .unwrap();
    let (_, users) = observed(&mut seen).await;
    assert_eq!(users, ["initial", "continue carefully"]);
    gates.add_permits(1);
    assert_eq!(
        restarted
            .wait(&parent.session, &child.id, 3000)
            .await
            .unwrap()
            .status,
        Status::Completed
    );
}

#[tokio::test]
async fn completed_children_survive_reopening_but_remain_hidden_from_history_search() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager.spawn(&parent, "saved result".into()).await.unwrap();
    observed(&mut seen).await;
    gates.add_permits(1);
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    manager
        .store
        .with_connection(|db| {
            db.execute(
                "UPDATE subagent_sessions SET payload=CAST(json_remove(payload,'$.task') AS BLOB)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let restarted = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    assert_eq!(
        restarted.list(&parent.session).await.unwrap()[0]
            .result
            .as_deref(),
        Some("saved result")
    );
    assert_eq!(
        restarted.list(&parent.session).await.unwrap()[0].task,
        "saved result"
    );
    assert!(
        manager
            .store
            .search_sessions(None)
            .await
            .unwrap()
            .sessions
            .iter()
            .all(|s| s.id().as_str() != child.id)
    );
    let mut replaced = parent.clone();
    replaced.session = Session::new(SessionId::new("parent"));
    assert!(
        restarted.list(&replaced.session).await.unwrap().is_empty(),
        "a reset parent must not inherit old child access"
    );
}

#[tokio::test]
async fn terminal_outcomes_remain_visible_while_a_remote_worker_releases_ownership() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager
        .spawn(&parent, "finished task".into())
        .await
        .unwrap();
    observed(&mut seen).await;
    gates.add_permits(1);
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    let ownership = manager
        .store
        .execution_lock("subagent", &child.id)
        .await
        .unwrap();
    let observer = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    let revision = manager.store.view_revision().await.unwrap();
    let seen = observer.wait(&parent.session, &child.id, 1).await.unwrap();
    assert_eq!(seen.status, Status::Completed);
    assert_eq!(seen.result.as_deref(), Some("finished task"));
    drop(ownership);
    assert_eq!(
        manager.store.view_revision().await.unwrap(),
        revision,
        "ownership release alone does not produce another stream revision"
    );
}

#[tokio::test]
async fn replaying_a_delivered_message_keeps_the_completed_result() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager.spawn(&parent, "initial".into()).await.unwrap();
    observed(&mut seen).await;
    let identity = Some(("parent-run".into(), "send-call".into()));
    super::super::run_state::CURRENT_TOOL
        .scope(
            identity.clone(),
            manager.send(&parent, &child.id, "once".into()),
        )
        .await
        .unwrap();
    gates.add_permits(1);
    observed(&mut seen).await;
    gates.add_permits(1);
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    let replay = super::super::run_state::CURRENT_TOOL
        .scope(
            identity.clone(),
            manager.send(&parent, &child.id, "once".into()),
        )
        .await
        .unwrap();
    assert_eq!(replay.status, Status::Completed);
    assert_eq!(replay.result.as_deref(), Some("initial | once"));
    assert!(
        super::super::run_state::CURRENT_TOOL
            .scope(identity, manager.send(&parent, &child.id, "changed".into()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn another_manager_cannot_start_a_second_worker_for_an_active_child() {
    let (_dir, manager, parent, _finished, mut seen, _gates) = setup().await;
    let child = manager.spawn(&parent, "initial".into()).await.unwrap();
    observed(&mut seen).await;
    let other = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    assert_eq!(
        other
            .wait(&parent.session, &child.id, 1)
            .await
            .unwrap()
            .status,
        Status::Running
    );
    assert!(
        other
            .send(&parent, &child.id, "competing".into())
            .await
            .is_err()
    );
    assert_eq!(
        other.stop(&parent.session, &child.id).await.unwrap().status,
        Status::Stopped
    );
}

#[tokio::test]
async fn deleting_the_parent_also_deletes_finished_children() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    manager.store.save(&parent.session).await.unwrap();
    let child = manager.spawn(&parent, "private task".into()).await.unwrap();
    observed(&mut seen).await;
    gates.add_permits(1);
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    manager.store.delete(parent.session.id()).await.unwrap();
    assert!(
        manager
            .store
            .load(&SessionId::new(&child.id))
            .await
            .unwrap()
            .is_none()
    );
    assert!(manager.list(&parent.session).await.unwrap().is_empty());
}

#[tokio::test]
async fn child_metadata_survives_personal_store_migration() {
    let (dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager
        .spawn(&parent, "migrate result".into())
        .await
        .unwrap();
    observed(&mut seen).await;
    gates.add_permits(1);
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    let agent = manager.runner.agent.clone();
    drop(manager);
    let staging = tempfile::tempdir().unwrap();
    crate::agent::session::stage_personal_sessions(&[dir.path().join("sessions")], staging.path())
        .unwrap();
    let merged = SqliteSessionStore::open_async(staging.path().into())
        .await
        .unwrap();
    let reopened = Subagents::new(agent, merged).await.unwrap();
    assert_eq!(
        reopened.list(&parent.session).await.unwrap()[0]
            .result
            .as_deref(),
        Some("migrate result")
    );
}

struct ParentModel {
    child_started: Semaphore,
}

#[async_trait::async_trait]
impl LlmProvider for ParentModel {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        if context.instructions.contains("## Delegated task") {
            assert!(
                !names
                    .iter()
                    .any(|name| name == "spawn_agent" || name == "send_message")
            );
            self.child_started.add_permits(1);
            return std::future::pending().await;
        }
        assert!(names.iter().any(|name| name == "spawn_agent"));
        if context
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
        {
            self.child_started.acquire().await.unwrap().forget();
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "parent done".into(),
                })],
                None,
            ))
        } else {
            Ok((
                vec![Message::ToolCall(crate::agent::message::ToolCall {
                    call_id: "spawn".into(),
                    name: "spawn_agent".into(),
                    arguments: serde_json::json!({"task":"child work"}),
                })],
                None,
            ))
        }
    }
}

#[tokio::test]
async fn the_real_parent_loop_binds_tools_and_stops_children_on_completion() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let base = Agent::new(
        Arc::new(ParentModel {
            child_started: Semaphore::new(0),
        }),
        ToolRegistry::default(),
        ContextBuilder::new("parent rules"),
    );
    let manager = Subagents::new(Arc::new(base.child_agent()), store)
        .await
        .unwrap();
    let agent = base.with_subagents(manager.clone());
    let mut session = Session::new(SessionId::new("parent"));
    let answer = tokio::time::timeout(
        Duration::from_secs(3),
        agent.run(
            &mut session,
            crate::agent::UserMessage {
                content: "delegate".into(),
                images: vec![],
                files: vec![],
            },
            None,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer.content, "parent done");
    let (_finished, receiver) = watch::channel(false);
    let parent = ParentContext {
        session,
        finished: receiver,
    };
    let child = manager.list(&parent.session).await.unwrap().remove(0);
    assert_eq!(
        manager
            .wait(&parent.session, &child.id, 3000)
            .await
            .unwrap()
            .status,
        Status::Stopped
    );
}

struct SandboxModel {
    secret: std::path::PathBuf,
}

#[async_trait::async_trait]
impl LlmProvider for SandboxModel {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        if let Some(output) = context.messages.iter().find_map(|message| {
            if let Message::ToolResult(result) = message {
                Some(&result.content.output)
            } else {
                None
            }
        }) {
            assert_ne!(output["exit_code"], 0);
            assert!(!output.to_string().contains("HOST_SECRET"));
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "isolated".into(),
                })],
                None,
            ))
        } else {
            Ok((
                vec![Message::ToolCall(crate::agent::message::ToolCall {
                    call_id: "read-host".into(),
                    name: "exec_command".into(),
                    arguments: serde_json::json!({"cmd":format!("cat {}", self.secret.display())}),
                })],
                None,
            ))
        }
    }
}

#[tokio::test]
async fn children_inherit_the_virtual_filesystem_without_host_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, "HOST_SECRET").unwrap();
    let space = dir.path().join("isolated");
    let store = SqliteSessionStore::open_async(space.join("sessions"))
        .await
        .unwrap();
    let sandbox = Arc::new(crate::agent::sandbox::Sandbox::new(space));
    let agent = Agent::new(
        Arc::new(SandboxModel { secret }),
        ToolRegistry::default().register(sandbox.tool("exec_command")),
        ContextBuilder::new("rules"),
    )
    .with_sandbox(sandbox);
    let manager = Subagents::new(Arc::new(agent.child_agent()), store)
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("parent"));
    ContextBuilder::new("rules")
        .initialize_session(&mut session)
        .unwrap();
    let (_finished, receiver) = watch::channel(false);
    let parent = ParentContext {
        session,
        finished: receiver,
    };
    let child = manager.spawn(&parent, "read file".into()).await.unwrap();
    let result = manager
        .wait(&parent.session, &child.id, 10_000)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{result:?}");
    assert_eq!(result.result.as_deref(), Some("isolated"));
}

#[tokio::test]
async fn failed_spawn_rolls_back_child_history_and_admitted_input() {
    let (_dir, manager, parent, _finished, _seen, _gates) = setup().await;
    manager.store.with_connection(|db| {
        db.execute_batch("CREATE TEMP TRIGGER reject_child BEFORE INSERT ON subagent_sessions BEGIN SELECT RAISE(ABORT,'injected metadata failure'); END;")?;
        Ok(())
    }).await.unwrap();
    assert!(
        manager
            .spawn(&parent, "must roll back".into())
            .await
            .is_err()
    );
    let count: i64 = manager.store.with_connection(|db| {
        Ok(db.query_row("SELECT count(*) FROM submissions WHERE conversation_id LIKE 'companion:subagent:%'", [], |r|r.get(0))?)
    }).await.unwrap();
    assert_eq!(
        count, 0,
        "unacknowledged input must not become an unreachable queued child"
    );
    assert!(
        manager
            .store
            .list()
            .await
            .unwrap()
            .iter()
            .all(|s| !s.id().as_str().starts_with(PREFIX))
    );
}

#[tokio::test]
async fn failed_send_rolls_back_input_instead_of_leaving_an_unmanaged_followup() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let child = manager.spawn(&parent, "initial".into()).await.unwrap();
    observed(&mut seen).await;
    gates.add_permits(1);
    manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    manager.store.with_connection(|db| {
        db.execute_batch("CREATE TEMP TRIGGER reject_child_update BEFORE UPDATE ON subagent_sessions BEGIN SELECT RAISE(ABORT,'injected metadata failure'); END;")?;
        Ok(())
    }).await.unwrap();
    assert!(
        manager
            .send(&parent, &child.id, "must roll back".into())
            .await
            .is_err()
    );
    assert!(
        manager
            .store
            .next_submission(&SessionId::new(&child.id))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn failed_metadata_does_not_hide_an_unfinished_durable_run() {
    use crate::agent::run_state::{Checkpoint, RunPhase};
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    let id = format!("{PREFIX}uncertain");
    let mut session = parent.session.child(SessionId::new(&id));
    manager.store.save(&session).await.unwrap();
    let submission = manager.submit(&id, "original".into()).await.unwrap();
    session.push(Message::User(crate::agent::UserMessage {
        content: "original".into(),
        images: vec![],
        files: vec![],
    }));
    let mut cp = Checkpoint::new(&manager.store, session.id(), &session);
    cp.record.submission_ids.push(submission.id.clone());
    cp.record.isolated = Some(false);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    manager
        .save(&Child {
            task: String::new(),
            id: id.clone(),
            parent: parent.session.id().as_str().into(),
            generation: parent.session.generation().into(),
            submission: submission.id,
            status: Status::Failed,
            result: None,
            error: Some("commit unconfirmed".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        manager.wait(&parent.session, &id, 1).await.unwrap().status,
        Status::Interrupted
    );
    manager
        .send(&parent, &id, "inspect and continue".into())
        .await
        .unwrap();
    assert_eq!(
        observed(&mut seen).await.1,
        ["original", "inspect and continue"]
    );
    gates.add_permits(1);
    assert_eq!(
        manager
            .wait(&parent.session, &id, 3000)
            .await
            .unwrap()
            .status,
        Status::Completed
    );
}

#[tokio::test]
async fn lifecycle_ownership_protects_admitted_children_before_the_runner_starts() {
    let (_dir, manager, parent, _finished, _seen, _gates) = setup().await;
    let id = format!("{PREFIX}starting");
    manager
        .store
        .save(&parent.session.child(SessionId::new(&id)))
        .await
        .unwrap();
    let submission = manager.submit(&id, "not yet started".into()).await.unwrap();
    let ownership = manager.store.execution_lock("subagent", &id).await.unwrap();
    manager
        .save(&Child {
            task: String::new(),
            id: id.clone(),
            parent: parent.session.id().as_str().into(),
            generation: parent.session.generation().into(),
            submission: submission.id,
            status: Status::Running,
            result: None,
            error: None,
        })
        .await
        .unwrap();
    let other = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    assert_eq!(
        other.wait(&parent.session, &id, 1).await.unwrap().status,
        Status::Running
    );
    assert!(
        manager
            .store
            .next_submission(&SessionId::new(&id))
            .await
            .unwrap()
            .is_some()
    );
    drop(ownership);
    assert_eq!(
        other.wait(&parent.session, &id, 1).await.unwrap().status,
        Status::Interrupted
    );
}

#[tokio::test]
async fn concurrent_managers_cannot_exceed_the_per_parent_session_limit() {
    let (_dir, manager, parent, _finished, mut seen, gates) = setup().await;
    for index in 0..15 {
        let id = format!("{PREFIX}finished-{index}");
        manager
            .store
            .save(&parent.session.child(SessionId::new(&id)))
            .await
            .unwrap();
        manager
            .save(&Child {
                task: String::new(),
                id,
                parent: parent.session.id().as_str().into(),
                generation: parent.session.generation().into(),
                submission: String::new(),
                status: Status::Completed,
                result: Some("done".into()),
                error: None,
            })
            .await
            .unwrap();
    }
    let other = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        manager.spawn(&parent, "one slot".into()),
        other.spawn(&parent, "same slot".into())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    observed(&mut seen).await;
    gates.add_permits(1);
    let id = a.or(b).unwrap().id;
    manager.wait(&parent.session, &id, 3000).await.unwrap();
    assert_eq!(manager.list(&parent.session).await.unwrap().len(), 16);
}

type EffectGate = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

struct ReleaseEffect(EffectGate);
impl Drop for ReleaseEffect {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

struct BlockingEffect {
    started: Arc<Semaphore>,
    gate: EffectGate,
}

#[async_trait::async_trait]
impl Tool for BlockingEffect {
    fn name(&self) -> String {
        "blocking_effect".into()
    }
    fn description(&self) -> String {
        "Hold a real blocking effect".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
    ) -> anyhow::Result<crate::agent::tools::ToolOutput> {
        let (started, gate) = (self.started.clone(), self.gate.clone());
        crate::agent::run_state::spawn_owned_blocking(move || {
            started.add_permits(1);
            let mut released = gate.0.lock().unwrap();
            while !*released {
                released = gate.1.wait(released).unwrap();
            }
        })
        .await?;
        Ok(serde_json::json!({"done":true}).into())
    }
}

struct EffectModel;
#[async_trait::async_trait]
impl LlmProvider for EffectModel {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        Ok((
            vec![Message::ToolCall(crate::agent::message::ToolCall {
                call_id: "effect".into(),
                name: "blocking_effect".into(),
                arguments: serde_json::json!({}),
            })],
            None,
        ))
    }
}

#[tokio::test]
async fn cancellation_keeps_children_busy_until_retained_blocking_effects_finish() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let started = Arc::new(Semaphore::new(0));
    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = ReleaseEffect(gate.clone());
    let agent = Agent::new(
        Arc::new(EffectModel),
        ToolRegistry::default().register(Arc::new(BlockingEffect {
            started: started.clone(),
            gate,
        })),
        ContextBuilder::new("rules"),
    );
    let manager = Subagents::new(Arc::new(agent.child_agent()), store)
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("parent"));
    ContextBuilder::new("rules")
        .initialize_session(&mut session)
        .unwrap();
    manager.store.save(&session).await.unwrap();
    let (_finished, receiver) = watch::channel(false);
    let parent = ParentContext {
        session,
        finished: receiver,
    };
    let child = manager.spawn(&parent, "do work".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let task = {
        let manager = manager.clone();
        let parent = parent.clone();
        let id = child.id.clone();
        tokio::spawn(async move { manager.stop(&parent.session, &id).await })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager
            .store
            .unfinished_runs()
            .await
            .unwrap()
            .iter()
            .any(|run| run.session_id == child.id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        manager
            .wait(&parent.session, &child.id, 50)
            .await
            .unwrap()
            .status,
        Status::Running
    );
    assert!(manager.cancellation_requested(&child.id).await.unwrap());
    let observer = Subagents::new(manager.runner.agent.clone(), manager.store.clone())
        .await
        .unwrap();
    assert!(observer.cancellation_requested(&child.id).await.unwrap());
    assert!(manager.store.delete(parent.session.id()).await.is_err());
    assert!(
        manager
            .send(&parent, &child.id, "too early".into())
            .await
            .is_err()
    );
    drop(release);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .status,
        Status::Stopped
    );
    manager.store.delete(parent.session.id()).await.unwrap();
}

struct FailThenAnswer {
    seen: mpsc::UnboundedSender<Vec<String>>,
    gates: Arc<Semaphore>,
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl LlmProvider for FailThenAnswer {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let calls = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let users: Vec<_> = context
            .messages
            .iter()
            .filter_map(|m| {
                if let Message::User(u) = m {
                    Some(u.content.clone())
                } else {
                    None
                }
            })
            .collect();
        self.seen.send(users.clone()).unwrap();
        self.gates.acquire().await.unwrap().forget();
        if calls == 0 {
            anyhow::bail!("fixture generation failure");
        }
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: users.join(" | "),
            })],
            None,
        ))
    }
}

#[tokio::test]
async fn input_accepted_after_a_committed_failure_keeps_its_worker() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let (seen, mut observed) = mpsc::unbounded_channel();
    let gates = Arc::new(Semaphore::new(0));
    let agent = Agent::new(
        Arc::new(FailThenAnswer {
            seen,
            gates: gates.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        ToolRegistry::default(),
        ContextBuilder::new("rules"),
    );
    let manager = Subagents::new(Arc::new(agent.child_agent()), store)
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("parent"));
    ContextBuilder::new("rules")
        .initialize_session(&mut session)
        .unwrap();
    let (_finished, receiver) = watch::channel(false);
    let parent = ParentContext {
        session,
        finished: receiver,
    };
    let child = manager.spawn(&parent, "initial".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), observed.recv())
        .await
        .unwrap()
        .unwrap();
    let lock = manager.active.lock().await;
    let (queued, started) = oneshot::channel();
    let send = {
        let manager = manager.clone();
        let parent = parent.clone();
        let id = child.id.clone();
        tokio::spawn(async move {
            let _ = queued.send(());
            manager.send(&parent, &id, "retry".into()).await
        })
    };
    started.await.unwrap();
    gates.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager
            .store
            .unfinished_runs()
            .await
            .unwrap()
            .iter()
            .any(|run| run.session_id == child.id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(lock);
    send.await.unwrap().unwrap();
    let users = tokio::time::timeout(Duration::from_secs(3), observed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(users, ["initial", "retry"]);
    gates.add_permits(1);
    assert_eq!(
        manager
            .wait(&parent.session, &child.id, 3000)
            .await
            .unwrap()
            .result
            .as_deref(),
        Some("initial | retry")
    );
}

struct RetryingModel(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl LlmProvider for RetryingModel {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(crate::agent::llm::GenerationRetry {
            limit: 100,
            retry_after: Some(Duration::ZERO),
            error: anyhow::anyhow!("retry"),
        }
        .into())
    }
}

#[tokio::test]
async fn child_model_budget_charges_retry_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let model = Arc::new(RetryingModel(std::sync::atomic::AtomicUsize::new(0)));
    let agent = Agent::new(
        model.clone(),
        ToolRegistry::default(),
        ContextBuilder::new("rules"),
    );
    let manager = Subagents::new(Arc::new(agent.child_agent()), store)
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("parent"));
    ContextBuilder::new("rules")
        .initialize_session(&mut session)
        .unwrap();
    let (_finished, receiver) = watch::channel(false);
    let parent = ParentContext {
        session,
        finished: receiver,
    };
    let child = manager
        .spawn(&parent, "retry within budget".into())
        .await
        .unwrap();
    let outcome = manager
        .wait(&parent.session, &child.id, 3000)
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Failed, "{outcome:?}");
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 16);
}

#[tokio::test]
async fn tool_schemas_are_strict_and_cannot_override_parent_ownership() {
    let (_dir, manager, parent, _finished, _seen, _gates) = setup().await;
    let tools = crate::agent::tools::subagents::register(ToolRegistry::default(), manager);
    for tool in tools.list() {
        let schema = tool.parameters();
        assert_eq!(schema["additionalProperties"], false);
        let properties = schema["properties"].as_object().unwrap();
        let required = schema["required"]
            .as_array()
            .expect("strict tool schemas enumerate required fields");
        assert_eq!(properties.len(), required.len());
        assert!(
            properties
                .keys()
                .all(|key| required.iter().any(|field| field == key))
        );
    }
    assert!(
        tools
            .execute("list_agents", serde_json::json!({}))
            .await
            .is_err()
    );
    assert!(
        CURRENT_PARENT
            .scope(
                Some(parent.clone()),
                tools.execute("list_agents", serde_json::json!({"parent":"other"}))
            )
            .await
            .is_err()
    );
    let result = CURRENT_PARENT
        .scope(
            Some(parent),
            tools.execute("list_agents", serde_json::json!({})),
        )
        .await
        .unwrap();
    assert_eq!(result.output, serde_json::json!({"agents":[]}));
}
