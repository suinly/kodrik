use super::{protocol::*, service::EnvironmentService};
use crate::{
    agent::session::{Session, SessionId, SessionStore, SqliteSessionStore},
    web::auth::{
        AuthStore, Identity,
        devices::{DeviceIdentity, PairingPoll},
    },
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

fn manifest() -> EnvironmentManifest {
    EnvironmentManifest {
        label: "Laptop".into(),
        project_root: "/project".into(),
        tools: vec![ToolDescriptor {
            id: "exec".into(),
            kind: ToolKind::Exec,
            description: "Shell".into(),
            parameters: json!({"type":"object"}),
            deferred: false,
        }],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    }
}

async fn fixture(
    owner: bool,
) -> (
    tempfile::TempDir,
    Arc<EnvironmentService>,
    SqliteSessionStore,
    AuthStore,
    DeviceIdentity,
    SessionId,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(id.clone())).await.unwrap();
    let auth = AuthStore::new(dir.path());
    let now = chrono::Utc::now().timestamp();
    let pairing = auth.start_device_pairing("Laptop", now).unwrap();
    auth.approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized { device_id, .. } = auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let service = Arc::new(EnvironmentService::new(store.clone(), auth.clone(), owner));
    (dir, service, store, auth, DeviceIdentity { device_id }, id)
}

fn invocation(key: EnvironmentKey, id: SessionId) -> Invocation {
    Invocation {
        environment: key,
        session_id: id,
        run_id: "run".into(),
        call_id: "call".into(),
        tool_id: "exec".into(),
        arguments: json!({}),
        deadline: chrono::Utc::now() + chrono::Duration::minutes(1),
    }
}

#[tokio::test(start_paused = true)]
async fn lease_expires_at_fifteen_seconds_and_heartbeat_renews_only_live_generation() {
    let (_dir, service, _store, _auth, device, id) = fixture(true).await;
    let mut revisions = service.revisions();
    let key = service
        .attach(device.clone(), id.clone(), manifest())
        .await
        .unwrap();
    assert!(revisions.has_changed().unwrap());
    revisions.borrow_and_update();
    tokio::time::advance(Duration::from_secs(14)).await;
    assert!(
        service
            .authorize(&invocation(key.clone(), id.clone()))
            .await
            .is_ok()
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        service
            .authorize(&invocation(key.clone(), id.clone()))
            .await
            .is_err()
    );
    assert!(service.heartbeat(&key).await.is_err());
    assert_eq!(service.status(&id).await.unwrap().state, "disconnected");
    assert!(revisions.has_changed().unwrap());
    let fresh = service
        .attach(device.clone(), id.clone(), manifest())
        .await
        .unwrap();
    assert_ne!(key, fresh);
    tokio::time::advance(Duration::from_secs(14)).await;
    service.heartbeat(&fresh).await.unwrap();
    tokio::time::advance(Duration::from_secs(14)).await;
    assert!(service.status(&id).await.is_some());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(service.status(&id).await.unwrap().state, "disconnected");
}

#[tokio::test]
async fn bindings_are_exact_personal_owner_sessions_and_not_recreated_on_restart() {
    let (_dir, service, store, auth, device, id) = fixture(true).await;
    let key = service
        .attach(device.clone(), id.clone(), manifest())
        .await
        .unwrap();
    assert!(
        service
            .attach(device.clone(), id.clone(), manifest())
            .await
            .is_err()
    );
    let other = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(other.clone())).await.unwrap();
    assert!(
        service
            .authorize(&invocation(key.clone(), other.clone()))
            .await
            .is_err()
    );
    assert!(service.status(&other).await.is_none());
    for denied in [
        "companion:wakeup",
        "telegram:main:-42",
        "telegram:main:42:guest",
        "missing",
    ] {
        let target = SessionId::new(denied);
        if denied != "missing" {
            store.save(&Session::new(target.clone())).await.unwrap();
        }
        assert!(
            service
                .attach(device.clone(), target, manifest())
                .await
                .is_err()
        );
    }
    let ownerless = EnvironmentService::new(store.clone(), auth.clone(), false);
    assert!(
        ownerless
            .attach(device.clone(), other.clone(), manifest())
            .await
            .is_err()
    );
    let restart = EnvironmentService::new(store, auth, true);
    assert!(restart.status(&id).await.is_none());
    let mut stale = key.clone();
    stale.generation = Uuid::new_v4();
    assert!(
        service
            .authorize(&invocation(stale, id.clone()))
            .await
            .is_err()
    );
    service.revoke(&key, "closed").await.unwrap();
    assert!(service.authorize(&invocation(key, id)).await.is_err());
}

#[tokio::test]
async fn credential_revocation_between_approval_and_dispatch_fails_closed_for_all_bindings() {
    let (_dir, service, store, auth, device, id) = fixture(true).await;
    let key = service
        .attach(device.clone(), id.clone(), manifest())
        .await
        .unwrap();
    let call = invocation(key, id.clone());
    service.authorize(&call).await.unwrap();
    let other = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(other.clone())).await.unwrap();
    service
        .attach(device.clone(), other.clone(), manifest())
        .await
        .unwrap();
    auth.revoke_device(device.device_id).unwrap();
    assert!(service.authorize(&call).await.is_err());
    assert_eq!(service.status(&other).await.unwrap().state, "disconnected");
    assert!(service.attach(device, id, manifest()).await.is_err());
}

#[tokio::test]
async fn registry_capacity_is_bounded_and_device_revoke_releases_slots() {
    let (_dir, service, store, _auth, device, id) = fixture(true).await;
    service
        .attach(device.clone(), id, manifest())
        .await
        .unwrap();
    for _ in 1..16 {
        let id = SessionId::new(Uuid::new_v4().to_string());
        store.save(&Session::new(id.clone())).await.unwrap();
        service
            .attach(device.clone(), id, manifest())
            .await
            .unwrap();
    }
    let id = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(id.clone())).await.unwrap();
    assert!(
        service
            .attach(device.clone(), id.clone(), manifest())
            .await
            .is_err()
    );
    service.revoke_device(device.device_id).await;
    service.attach(device, id, manifest()).await.unwrap();
}

#[tokio::test]
async fn owner_web_runner_shares_service_while_another_user_runner_has_no_authority() {
    use crate::agent::{Agent, AgentRunner, ContextBuilder, tools::ToolRegistry};
    struct NoModel;
    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for NoModel {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            panic!("no model calls expected")
        }
    }
    let (_dir, _service, store, auth, device, id) = fixture(true).await;
    let agent = Arc::new(Agent::new(
        Arc::new(NoModel),
        ToolRegistry::default(),
        ContextBuilder::new("test"),
    ));
    let owner = AgentRunner::new(agent.clone(), Arc::new(store.clone()));
    owner.enable_environments(auth).unwrap();
    let web = owner.for_web(Arc::new(store.clone()));
    assert!(
        owner.environments().is_some(),
        "gateway service was not initialized"
    );
    assert!(Arc::ptr_eq(
        owner.environments().unwrap(),
        web.environments().unwrap()
    ));
    let another = AgentRunner::new(agent, Arc::new(store));
    assert!(another.environments().is_none());
    let key = owner
        .environments()
        .unwrap()
        .attach(device.clone(), id.clone(), manifest())
        .await
        .unwrap();
    assert_eq!(
        web.environments()
            .unwrap()
            .status(&id)
            .await
            .unwrap()
            .environment,
        Some(key)
    );
    owner.suspend();
    tokio::time::timeout(Duration::from_secs(2), async {
        while web
            .environments()
            .unwrap()
            .status(&id)
            .await
            .is_some_and(|status| status.state == "connected")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        web.environments()
            .unwrap()
            .attach(device, id, manifest())
            .await
            .is_err(),
        "stopped gateway accepted a new environment"
    );
}

#[tokio::test]
async fn disconnected_status_preserves_origin_reason_but_never_execution_authority() {
    let (_dir, service, _store, _auth, device, id) = fixture(true).await;
    let key = service
        .attach(device.clone(), id.clone(), manifest())
        .await
        .unwrap();
    let mut revisions = service.revisions();
    revisions.borrow_and_update();
    service.heartbeat(&key).await.unwrap();
    assert!(!revisions.has_changed().unwrap());
    service.revoke(&key, "TUI closed").await.unwrap();
    let status = service.status(&id).await.unwrap();
    assert_eq!(status.state, "disconnected");
    assert_eq!(status.environment, Some(key.clone()));
    assert_eq!(status.label, "Laptop");
    assert_eq!(status.project_root, "/project");
    assert_eq!(status.reason.as_deref(), Some("TUI closed"));
    assert!(
        service
            .authorize(&invocation(key.clone(), id.clone()))
            .await
            .is_err()
    );
    assert!(service.heartbeat(&key).await.is_err());
    let fresh = service
        .attach(device, id.clone(), manifest())
        .await
        .unwrap();
    assert_ne!(key, fresh);
    assert_eq!(service.status(&id).await.unwrap().state, "connected");
    // A delayed close of an old generation cannot overwrite the fresh status.
    service.revoke(&key, "late close").await.unwrap();
    assert_eq!(service.status(&id).await.unwrap().environment, Some(fresh));
}

#[tokio::test]
async fn retired_environment_statuses_are_bounded_and_scoped_to_their_sessions() {
    let (_dir, service, store, _auth, device, first) = fixture(true).await;
    for index in 0..65 {
        let id = if index == 0 {
            first.clone()
        } else {
            SessionId::new(uuid::Uuid::new_v4().to_string())
        };
        if index > 0 {
            store.save(&Session::new(id.clone())).await.unwrap();
        }
        let key = service
            .attach(device.clone(), id, manifest())
            .await
            .unwrap();
        service.revoke(&key, "closed").await.unwrap();
    }
    assert!(
        service.status(&first).await.is_none(),
        "retired display registry grew without bound"
    );
    assert!(service.status(&SessionId::new("unrelated")).await.is_none());
}
