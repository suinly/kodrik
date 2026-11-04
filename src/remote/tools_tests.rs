use super::{protocol::*, service::EnvironmentService};
use crate::{
    agent::{
        message::ToolCall,
        session::{Session, SessionId, SessionStore, SqliteSessionStore},
        tools::{Tool, ToolOutput, ToolRegistry},
    },
    web::auth::{
        AuthStore, Identity,
        devices::{DeviceIdentity, PairingPoll},
    },
};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

struct ServerTool;
#[async_trait::async_trait]
impl Tool for ServerTool {
    fn name(&self) -> String {
        "exec_command".into()
    }
    fn description(&self) -> String {
        "server shell".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(json!({"origin":"server"}).into())
    }
}

struct DeferredServer;
#[async_trait::async_trait]
impl Tool for DeferredServer {
    fn name(&self) -> String {
        "mcp__server__notes".into()
    }
    fn description(&self) -> String {
        "server notes search".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    fn deferred(&self) -> bool {
        true
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(json!({"server":true}).into())
    }
}

#[tokio::test]
async fn search_merges_origins_and_transcript_cannot_resurrect_revoked_tools() {
    use crate::agent::message::{Message, ToolResult};
    let (_dir, service, id, key) = fixture().await;
    assert!(
        service
            .step_scope(&SessionId::new("another-session"))
            .await
            .unwrap()
            .is_none()
    );
    let scope = service.step_scope(&id).await.unwrap().unwrap();
    let scoped = scope
        .tools(&ToolRegistry::default().register(Arc::new(DeferredServer)))
        .unwrap();
    let found = scoped
        .execute("mcp_search", json!({"query":"notes","limit":16}))
        .await
        .unwrap();
    assert_eq!(found.output["loaded_tools"].as_array().unwrap().len(), 2);
    let history = vec![
        Message::ToolCall(ToolCall {
            call_id: "search".into(),
            name: "mcp_search".into(),
            arguments: json!({"query":"notes"}),
        }),
        Message::ToolResult(ToolResult {
            call_id: "search".into(),
            content: found,
        }),
    ];
    assert_eq!(
        scoped
            .for_model(&history)
            .unwrap()
            .iter()
            .filter(|tool| tool.deferred())
            .count(),
        2
    );
    let name = scoped
        .list()
        .iter()
        .find(|tool| tool.audit_target().is_some())
        .unwrap()
        .name();
    service.revoke(&key, "test").await.unwrap();
    let revoked = scoped
        .execute(&name, json!({"cmd":"must not execute"}))
        .await
        .unwrap_err();
    assert!(
        revoked.is::<super::tools::EnvironmentUnavailable>(),
        "revoked call lost its typed pause: {revoked:#}"
    );
    let names = scoped
        .for_model(&history)
        .unwrap()
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["mcp__server__notes", "mcp_search"]);
    assert_eq!(
        scoped
            .execute("mcp_search", json!({"query":"notes","limit":16}))
            .await
            .unwrap()
            .output["total_matches"],
        1
    );
}

struct NamedTool(String);
#[async_trait::async_trait]
impl Tool for NamedTool {
    fn name(&self) -> String {
        self.0.clone()
    }
    fn description(&self) -> String {
        "fixture".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(json!({}).into())
    }
}
#[tokio::test]
async fn overlay_rejects_alias_collisions_and_overfull_model_catalog() {
    let (_dir, service, id, _key) = fixture().await;
    let scope = service.step_scope(&id).await.unwrap().unwrap();
    let name = scope
        .tools(&ToolRegistry::default())
        .unwrap()
        .list()
        .iter()
        .find(|tool| tool.audit_target().is_some())
        .unwrap()
        .name();
    assert!(
        scope
            .tools(&ToolRegistry::default().register(Arc::new(NamedTool(name))))
            .is_err()
    );
    let mut base = ToolRegistry::default();
    for i in 0..127 {
        base = base.register(Arc::new(NamedTool(format!("server_{i}"))));
    }
    assert!(scope.tools(&base).is_err());
}

#[tokio::test]
async fn deferred_catalog_requires_search_even_in_temporary_overlay() {
    let (_dir, service, id, _) = fixture().await;
    let scope = service.step_scope(&id).await.unwrap().unwrap();
    let overlay = scope.context_overlay();
    assert!(!overlay.contains("mcp__notes__search"));
    assert!(overlay.contains("mcp_search"));
}

#[tokio::test]
async fn background_session_cannot_inherit_even_an_explicit_scope() {
    let (_dir, service, id, _) = fixture().await;
    let scope = service.step_scope(&id).await.unwrap().unwrap();
    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let agent = crate::agent::Agent::new(
        Arc::new(InspectModel { contexts: send }),
        ToolRegistry::default(),
        crate::agent::ContextBuilder::new("server"),
    );
    let mut session = Session::new(SessionId::new("companion:wakeup:fixture"));
    super::tools::CURRENT_SCOPE
        .scope(
            Some(scope),
            agent.run(
                &mut session,
                crate::agent::UserMessage {
                    content: "inspect".into(),
                    images: vec![],
                    files: vec![],
                },
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert!(
        !received
            .recv()
            .await
            .unwrap()
            .1
            .iter()
            .any(|name| name.starts_with("local__"))
    );
}
async fn fixture() -> (
    tempfile::TempDir,
    Arc<EnvironmentService>,
    SessionId,
    EnvironmentKey,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let session = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(session.clone())).await.unwrap();
    let auth = AuthStore::new(dir.path());
    let now = chrono::Utc::now().timestamp();
    let pair = auth.start_device_pairing("laptop", now).unwrap();
    auth.approve_device_pairing(pair.request_id, &pair.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized { device_id, .. } = auth
        .poll_device_pairing(pair.request_id, &pair.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let service = Arc::new(EnvironmentService::new(store, auth, true));
    let key = service
        .attach(
            DeviceIdentity { device_id },
            session.clone(),
            EnvironmentManifest {
                label: "Laptop".into(),
                project_root: "/laptop/project".into(),
                tools: vec![
                    ToolDescriptor {
                        id: "exec_command".into(),
                        kind: ToolKind::Exec,
                        description: "shell".into(),
                        parameters: json!({"type":"object"}),
                        deferred: false,
                    },
                    ToolDescriptor {
                        id: "mcp__notes__search".into(),
                        kind: ToolKind::Mcp,
                        description: "laptop notes search".into(),
                        parameters: json!({"type":"object"}),
                        deferred: true,
                    },
                ],
                skills: vec![SkillDescriptor {
                    id: "review".into(),
                    name: "Review".into(),
                    description: "local review".into(),
                    path: "/laptop/skills/review/SKILL.md".into(),
                    content_hash: "a".repeat(64),
                }],
                project_instructions: Some(ProjectInstructions {
                    path: "/laptop/project/AGENTS.md".into(),
                    content: "Run local-check in this project".into(),
                    content_hash: format!(
                        "{:x}",
                        <sha2::Sha256 as sha2::Digest>::digest(b"Run local-check in this project")
                    ),
                }),
                warnings: vec![],
            },
        )
        .await
        .unwrap();
    (dir, service, session, key)
}

#[tokio::test]
async fn local_scope_preserves_server_names_and_normalizes_only_to_laptop_root() {
    let (_dir, service, session, key) = fixture().await;
    let scope = service
        .step_scope(&session)
        .await
        .unwrap()
        .expect("missing local scope");
    let base = ToolRegistry::default().register(Arc::new(ServerTool));
    let scoped = scope.tools(&base).unwrap();
    assert_eq!(base.list().len(), 1);
    let name = scoped
        .list()
        .iter()
        .find(|tool| tool.name().starts_with("local__exec__"))
        .expect("missing proxy")
        .name();
    assert!(name.len() <= 64 && name.is_ascii());
    assert_eq!(
        scoped
            .execute("exec_command", json!({}))
            .await
            .unwrap()
            .output["origin"],
        "server"
    );
    let call = ToolCall {
        call_id: "call".into(),
        name: name.clone(),
        arguments: json!({"cmd":"pwd"}),
    };
    let invocation = scope.validate_call(&call).unwrap();
    assert_eq!(invocation.environment, key);
    assert_eq!(invocation.arguments["workdir"], "/laptop/project");
    assert_eq!(invocation.tool_id, "exec_command");
    let unavailable = crate::agent::run_state::CURRENT_TOOL
        .scope(
            Some(("no-transport-run".into(), "no-transport-call".into())),
            scoped.execute(&name, json!({"cmd":"must not execute"})),
        )
        .await
        .unwrap_err();
    assert!(
        unavailable.is::<super::tools::EnvironmentUnavailable>(),
        "missing transport did not yield typed pause: {unavailable:#}"
    );
    assert!(
        scope
            .validate_call(&ToolCall {
                name: "exec_command".into(),
                ..call
            })
            .is_err()
    );
    service.revoke(&key, "test").await.unwrap();
    assert!(
        !scoped
            .for_model(&[])
            .unwrap()
            .iter()
            .any(|tool| tool.name() == name)
    );
    assert!(
        scoped
            .execute(&name, json!({"cmd":"touch must-not-run"}))
            .await
            .is_err()
    );
    assert!(service.step_scope(&session).await.unwrap().is_none());
}

#[tokio::test]
async fn temporary_overlay_and_one_search_are_scoped_to_exact_session() {
    let (_dir, service, session, key) = fixture().await;
    let scope = service
        .step_scope(&session)
        .await
        .unwrap()
        .expect("missing local scope");
    let overlay = scope.context_overlay();
    assert!(overlay.contains("/laptop/project/AGENTS.md"));
    assert!(overlay.contains("Run local-check"));
    assert!(overlay.contains("/laptop/skills/review/SKILL.md"));
    assert!(overlay.contains("local__"));
    let scoped = scope.tools(&ToolRegistry::default()).unwrap();
    assert_eq!(
        scoped
            .list()
            .iter()
            .filter(|tool| tool.name() == "mcp_search")
            .count(),
        1
    );
    let found = scoped
        .execute("mcp_search", json!({"query":"notes","limit":null}))
        .await
        .unwrap();
    assert_eq!(found.output["loaded_tools"].as_array().unwrap().len(), 1);
    assert!(
        scoped
            .for_model(&[])
            .unwrap()
            .iter()
            .all(|tool| !tool.deferred())
    );
    service.revoke(&key, "test").await.unwrap();
    assert_eq!(
        scoped
            .execute("mcp_search", json!({"query":"notes","limit":null}))
            .await
            .unwrap()
            .output["total_matches"],
        0
    );
    assert!(scope.context_overlay().is_empty());
}

struct InspectModel {
    contexts: tokio::sync::mpsc::UnboundedSender<(String, Vec<String>)>,
}
#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for InspectModel {
    async fn generate(
        &self,
        context: &crate::agent::LlmContext<'_>,
        tools: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
        self.contexts.send((
            context.instructions.to_string(),
            tools.iter().map(|tool| tool.name()).collect(),
        ))?;
        Ok((
            vec![crate::agent::message::Message::Assistant(
                crate::agent::AssistantMessage {
                    content: "done".into(),
                },
            )],
            None,
        ))
    }
}
#[tokio::test]
async fn agent_refreshes_live_overlay_without_mutating_session_snapshots() {
    let (_dir, service, id, key) = fixture().await;
    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let agent = crate::agent::Agent::new(
        Arc::new(InspectModel { contexts: send }),
        ToolRegistry::default(),
        crate::agent::ContextBuilder::new("server prompt"),
    );
    let mut session = Session::new(id.clone());
    session.initialize_skills_catalog("server Review skill".into());
    for expected in [true, false] {
        if !expected {
            service.revoke(&key, "test").await.unwrap();
        }
        super::tools::CURRENT_ENVIRONMENTS
            .scope(
                Some(service.clone()),
                agent.run(
                    &mut session,
                    crate::agent::UserMessage {
                        content: "inspect".into(),
                        images: vec![],
                        files: vec![],
                    },
                    None,
                    None,
                ),
            )
            .await
            .unwrap();
        let (context, names) = received.recv().await.unwrap();
        assert_eq!(context.contains("/laptop/project/AGENTS.md"), expected);
        assert_eq!(
            names.iter().any(|name| name.starts_with("local__")),
            expected
        );
        assert!(context.contains("server Review skill"));
        assert!(!session.system_prompt().unwrap().contains("laptop"));
        assert!(!session.skills_catalog().unwrap().contains("laptop"));
    }
    let mut other = Session::new(SessionId::new("other"));
    super::tools::CURRENT_ENVIRONMENTS
        .scope(
            Some(service),
            agent.run(
                &mut other,
                crate::agent::UserMessage {
                    content: "inspect".into(),
                    images: vec![],
                    files: vec![],
                },
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert!(
        !received
            .recv()
            .await
            .unwrap()
            .1
            .iter()
            .any(|name| name.starts_with("local__"))
    );
}

#[tokio::test]
async fn local_approval_shows_structured_target_and_binds_generation() {
    use crate::agent::{
        message::Message,
        run_state::{Checkpoint, RunPhase},
    };
    let (dir, service, id, key) = fixture().await;
    let scope = service.step_scope(&id).await.unwrap().unwrap();
    let registry = scope.tools(&ToolRegistry::default()).unwrap();
    let name = registry
        .list()
        .iter()
        .find(|tool| {
            tool.audit_target()
                .is_some_and(|target| target.tool_kind == ToolKind::Exec)
        })
        .unwrap()
        .name();
    let mut call = ToolCall {
        call_id: "approval".into(),
        name,
        arguments: json!({"cmd":"rm artifact"}),
    };
    scope.normalize(&mut call).unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let mut session = store.load(&id).await.unwrap().unwrap();
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&store, &id, &session);
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: call.call_id.clone(),
        },
        None,
    )
    .await
    .unwrap();
    store
        .record_command_audits(&cp.record, &[(call.clone(), "ask".into())])
        .await
        .unwrap();
    store
        .set_command_audit_mode(
            id.as_str(),
            crate::agent::command_audit::CommandAuditMode::Ask,
        )
        .await
        .unwrap();
    let pending = store.pending_command_approvals(id.as_str()).await.unwrap();
    let value = serde_json::to_value(&pending[0]).unwrap();
    assert_eq!(
        value["target"]["environment"]["generation"],
        key.generation.to_string()
    );
    assert_eq!(value["target"]["project_root"], "/laptop/project");
    assert!(pending[0].arguments_text().contains("/laptop/project"));
    assert!(
        pending[0]
            .arguments_text()
            .contains(&key.generation.to_string())
    );
    service.revoke(&key, "test").await.unwrap();
    assert!(
        store
            .pending_command_approvals(id.as_str())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .decide_command_approval(id.as_str(), &pending[0].id, true)
            .await
            .is_err()
    );
    let auth = AuthStore::new(dir.path());
    let device = auth
        .list_devices(&Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()[0]
        .id;
    let mut manifest: EnvironmentManifest = store
        .with_connection(move |db| {
            let bytes: Vec<u8> =
                db.query_row("SELECT manifest FROM remote_bindings LIMIT 1", [], |row| {
                    row.get(0)
                })?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .await
        .unwrap();
    manifest.skills[0].description = "Refreshed laptop skill".into();
    let fresh = service
        .attach(DeviceIdentity { device_id: device }, id.clone(), manifest)
        .await
        .unwrap();
    assert_ne!(fresh.generation, key.generation);
    let refreshed = service.step_scope(&id).await.unwrap().unwrap();
    assert!(
        refreshed
            .context_overlay()
            .contains("Refreshed laptop skill")
    );
    assert!(
        !refreshed
            .tools(&ToolRegistry::default())
            .unwrap()
            .contains(&call.name)
    );
    assert!(scope.normalize(&mut call).is_err());
    assert!(
        store
            .decide_command_approval(id.as_str(), &pending[0].id, true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn child_receives_only_explicit_parent_scope_and_loses_it_after_revoke() {
    use crate::agent::{
        Agent, ContextBuilder,
        subagents::{ParentContext, Subagents},
    };
    let (dir, service, id, key) = fixture().await;
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let manager = Subagents::new(
        Arc::new(Agent::new(
            Arc::new(InspectModel { contexts: send }),
            ToolRegistry::default(),
            ContextBuilder::new("server"),
        )),
        store.clone(),
    )
    .await
    .unwrap();
    let (_lifetime, finished) = tokio::sync::watch::channel(false);
    let parent = ParentContext {
        session: store.load(&id).await.unwrap().unwrap(),
        finished,
    };
    let scope = service.step_scope(&id).await.unwrap().unwrap();
    let child = super::tools::CURRENT_SCOPE
        .scope(Some(scope), manager.spawn(&parent, "inspect laptop".into()))
        .await
        .unwrap();
    let (context, names) = tokio::time::timeout(std::time::Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(context.contains("/laptop/project/AGENTS.md"));
    assert!(names.iter().any(|name| name.starts_with("local__")));
    manager
        .wait(&parent.session, &child.id, 2000)
        .await
        .unwrap();
    let other_session = Session::new(SessionId::new("unrelated-parent"));
    store.save(&other_session).await.unwrap();
    let other = ParentContext {
        session: other_session,
        finished: parent.finished.clone(),
    };
    let other_child = super::tools::CURRENT_SCOPE
        .scope(
            service.step_scope(&id).await.unwrap(),
            manager.spawn(&other, "inspect unrelated".into()),
        )
        .await
        .unwrap();
    let (_, names) = received.recv().await.unwrap();
    manager
        .wait(&other.session, &other_child.id, 2000)
        .await
        .unwrap();
    assert!(
        !names.iter().any(|name| name.starts_with("local__")),
        "another parent's scope leaked into child"
    );
    service.revoke(&key, "test").await.unwrap();
    manager
        .send(&parent, &child.id, "inspect again".into())
        .await
        .unwrap();
    let (_, names) = tokio::time::timeout(std::time::Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!names.iter().any(|name| name.starts_with("local__")));
    manager
        .wait(&parent.session, &child.id, 2000)
        .await
        .unwrap();
}
