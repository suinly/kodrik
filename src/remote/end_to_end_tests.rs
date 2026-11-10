//! Private loopback acceptance fixture: no real model, user data or external services.
use super::{
    client::{ChatClient, Credential, RemoteClient},
    config::RemoteConfig,
    local::LocalEnvironment,
    protocol::{ChatCommand, EnvironmentKey, ToolKind},
};
use crate::{
    agent::{
        Agent, AgentRunner, AssistantMessage, ContextBuilder, LlmContext,
        llm::LlmProvider,
        message::{Message, ToolCall},
        recovery::RecoveryTarget,
        run_state::RunPhase,
        session::{Session, SessionId, SessionStore, SqliteSessionStore},
        tools::{Tool, ToolRegistry},
    },
    web::{
        Settings,
        auth::{AuthStore, Identity, devices::PairingPoll},
        chat::Chat,
    },
};
use serde_json::json;
use std::{
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

const MEMORY: &str = "server-memory-fixture-4729";
const LOCAL: &str = "laptop-only-fixture-8316";

struct Model {
    calls: Arc<AtomicUsize>,
    root: PathBuf,
}

#[async_trait::async_trait]
impl LlmProvider for Model {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let answer = |content: &str| {
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: content.into(),
                })],
                None,
            ))
        };
        if context
            .instructions
            .starts_with("Generate a conversation title")
        {
            anyhow::ensure!(tools.is_empty(), "title generation received tools");
            return answer("Private laptop lifecycle fixture");
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(context.instructions.contains(MEMORY), "server memory lost");
        let start = context
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::User(_)))
            .unwrap();
        let Message::User(user) = &context.messages[start] else {
            unreachable!()
        };
        let turn = &context.messages[start + 1..];
        let local_tools: Vec<_> = tools
            .iter()
            .filter(|tool| tool.audit_target().is_some())
            .collect();
        if matches!(
            user.content.as_str(),
            "discuss without laptop" | "telegram isolation probe"
        ) {
            anyhow::ensure!(
                local_tools.is_empty(),
                "disconnected local tools advertised"
            );
            return answer("server conversation remains usable");
        }
        let invoke = |id: &str, arguments| {
            let kind = match id {
                "read_file" => ToolKind::ReadFile,
                "exec_command" => ToolKind::Exec,
                "mcp" => ToolKind::Mcp,
                _ => unreachable!(),
            };
            let tool = local_tools
                .iter()
                .find(|tool| {
                    tool.audit_target()
                        .is_some_and(|target| target.tool_kind == kind)
                })
                .ok_or_else(|| anyhow::anyhow!("missing local {id}"))?;
            Ok((
                vec![Message::ToolCall(ToolCall {
                    call_id: Uuid::new_v4().to_string(),
                    name: tool.name(),
                    arguments,
                })],
                None,
            ))
        };
        let results: Vec<_> = turn
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(result) => Some(&result.content.output),
                _ => None,
            })
            .collect();
        if user.content == "local MCP effect" {
            return match results.len() {
                0 => {
                    let search = tools
                        .iter()
                        .find(|tool| tool.name() == "mcp_search")
                        .ok_or_else(|| anyhow::anyhow!("local MCP discovery missing"))?;
                    Ok((
                        vec![Message::ToolCall(ToolCall {
                            call_id: Uuid::new_v4().to_string(),
                            name: search.name(),
                            arguments: json!({"query":"laptop record","limit":1}),
                        })],
                        None,
                    ))
                }
                1 => {
                    let loaded = results[0]["loaded_tools"]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("MCP discovery failed: {}", results[0]))?;
                    anyhow::ensure!(
                        loaded.len() == 1,
                        "MCP discovery did not load one local tool"
                    );
                    let mcp = local_tools
                        .iter()
                        .find(|tool| {
                            tool.audit_target()
                                .is_some_and(|target| target.tool_kind == ToolKind::Mcp)
                        })
                        .ok_or_else(|| anyhow::anyhow!("discovered MCP schema not advertised"))?;
                    anyhow::ensure!(loaded[0] == mcp.name(), "discovery loaded wrong origin");
                    invoke("mcp", json!({"memory":MEMORY,"local":LOCAL}))
                }
                2 => {
                    anyhow::ensure!(
                        results[1]["structuredContent"]["local"] == LOCAL,
                        "MCP result did not round-trip: {}",
                        results[1]
                    );
                    answer("local MCP finished")
                }
                _ => anyhow::bail!("MCP effect repeated"),
            };
        }
        if user.content == "start interruptible work" {
            if !results.is_empty() {
                return answer("interruptible work finished");
            }
            return invoke(
                "exec_command",
                json!({"cmd":"printf 'started\\n' >> ledger; echo $$ > shell.pid; sleep 30 & echo $! > child.pid; wait; printf 'escaped\\n' >> ledger", "workdir":self.root,"timeout_ms":45000}),
            );
        }
        anyhow::ensure!(
            matches!(
                user.content.as_str(),
                "effect" | "fresh explicit effect" | "telegram effect"
            ),
            "unexpected fixture input: {}",
            user.content
        );
        match results.len() {
            0 => invoke("read_file", json!({"path":self.root.join("local.txt")})),
            1 => {
                anyhow::ensure!(results[0]["text"] == LOCAL, "local read did not round-trip");
                let ledger = if user.content == "telegram effect" {
                    "telegram-ledger"
                } else {
                    "ledger"
                };
                invoke(
                    "exec_command",
                    json!({"cmd":format!("printf '%s\\n' '{MEMORY}:{LOCAL}' >> {ledger}"),"workdir":self.root,"timeout_ms":2000}),
                )
            }
            2 => {
                anyhow::ensure!(results[1]["exit_code"] == 0, "local shell failed");
                answer("local work finished")
            }
            _ => anyhow::bail!("unexpected extra local effect"),
        }
    }
}

// Use the same libtest subprocess convention as the production supervisor tests.
#[tokio::test]
async fn local_worker() {
    let Some(root) = std::env::var_os("KODRIK_E2E_LOCAL_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut config = RemoteConfig::load(&root.join("remote.toml")).unwrap();
    let (mcp_url, _mcp) = local_mcp(&root).await;
    config.mcp.insert(
        "laptop".into(),
        crate::config::McpServerConfig::Http {
            url: mcp_url,
            headers: Default::default(),
        },
    );
    let environment = LocalEnvironment::open(&config, &root).await.unwrap();
    let manifest = environment.manifest().unwrap();
    config.authorize_manifest(&manifest, || Ok(true)).unwrap();
    let mut client = RemoteClient::connect(&config).await.unwrap();
    let key = client
        .attach(
            SessionId::new(std::env::var("KODRIK_E2E_SESSION").unwrap()),
            manifest,
        )
        .await
        .unwrap();
    std::fs::write(
        root.join("attached.json"),
        serde_json::to_vec(&key).unwrap(),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let gate = root.join("stop");
    let cancellation = tokio::spawn(async move {
        loop {
            if gate.try_exists().unwrap() {
                stop.send(()).unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    client
        .serve_local_until(environment, stopped)
        .await
        .unwrap();
    cancellation.await.unwrap();
    std::fs::write(root.join("stopped"), "local cancellation completed").unwrap();
}

// The laptop process owns both HTTP MCP and its independent effect ledger.
async fn local_mcp(root: &Path) -> (String, Server) {
    use axum::{Json, http::StatusCode, response::IntoResponse, routing::post};
    use std::io::Write;
    let root = root.to_owned();
    let app = axum::Router::new().route("/mcp", post(move |Json(body): Json<serde_json::Value>| {
        let root = root.clone();
        async move {
            if body.get("id").is_none() {
                return StatusCode::ACCEPTED.into_response();
            }
            let result = match body["method"].as_str().unwrap() {
                "initialize" => json!({"protocolVersion":body["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"laptop-fixture","version":"1"}}),
                "tools/list" => json!({"tools":[{"name":"record","description":"Record a local laptop MCP effect","inputSchema":{"type":"object","properties":{"memory":{"type":"string"},"local":{"type":"string"}},"required":["memory","local"],"additionalProperties":false}}]}),
                "tools/call" => {
                    assert_eq!(body["params"]["name"], "record");
                    assert_eq!(body["params"]["arguments"], json!({"memory":MEMORY,"local":LOCAL}));
                    let local = std::fs::read_to_string(root.join("local.txt")).unwrap();
                    assert_eq!(local, LOCAL);
                    let mut ledger = std::fs::OpenOptions::new().create(true).append(true).open(root.join("mcp-ledger")).unwrap();
                    writeln!(ledger, "{MEMORY}:{local}").unwrap();
                    ledger.sync_all().unwrap();
                    json!({"content":[{"type":"text","text":local}],"structuredContent":{"local":local},"isError":false})
                }
                method => panic!("unexpected laptop MCP method {method}"),
            };
            Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result})).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    (
        url,
        Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        })),
    )
}

struct Worker(Child);
impl Worker {
    fn spawn(root: &Path, session: &SessionId) -> Self {
        let _ = std::fs::remove_file(root.join("attached.json"));
        let _ = std::fs::remove_file(root.join("stop"));
        let _ = std::fs::remove_file(root.join("stopped"));
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "remote::end_to_end_tests::local_worker",
                    "--nocapture",
                ])
                .env("KODRIK_E2E_LOCAL_ROOT", root)
                .env("KODRIK_E2E_SESSION", session.as_str())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }
    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn wait_file(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture file did not appear: {}", path.display()))
}

async fn wait_idle(store: &SqliteSessionStore, session: &SessionId) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while store.session_busy(session.as_str()).await.unwrap() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("conversation did not yield execution ownership");
}

fn ledger(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("ledger"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn running(pid: u32) -> bool {
    // An orphan zombie is not an executing descendant on hosts that reap slowly.
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&output.stdout);
    !status.trim().is_empty() && !status.trim().starts_with('Z')
}

async fn owner_send(
    http: &reqwest::Client,
    base: &reqwest::Url,
    cookie: &str,
    session: &SessionId,
    text: &str,
) {
    let response = http
        .post(base.join("api/chat/send").unwrap())
        .header("cookie", format!("kodrik_web_session={cookie}"))
        .header("origin", base.origin().ascii_serialization())
        .json(&json!({"id":session,"request_id":Uuid::new_v4(),"text":text}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "owner send {status}: {body}");
}

// Catches broken native/web routing, missing memory/context, lost disconnect fences,
// leaked shell groups, and recovery/reattach accidentally replaying local effects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_tui_revokes_but_keeps_server_alive() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .is_test(true)
        .try_init();
    let server_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(local_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(server_dir.path().join("MEMORY.md"), MEMORY).unwrap();
    std::fs::write(local_dir.path().join("local.txt"), LOCAL).unwrap();
    let store = Arc::new(SqliteSessionStore::open(server_dir.path().join("sessions")).unwrap());
    let auth = AuthStore::new(server_dir.path());
    auth.configure_owner(Some(42)).unwrap();
    let now = chrono::Utc::now().timestamp();
    let pair = auth.start_device_pairing("e2e laptop", now).unwrap();
    auth.approve_device_pairing(pair.request_id, &pair.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized { token, device_id } = auth
        .poll_device_pairing(pair.request_id, &pair.polling_secret, now)
        .unwrap()
    else {
        panic!("fixture pairing failed")
    };
    let cookie = auth.test_session(Identity::Owner, now).unwrap().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model {
                calls: calls.clone(),
                root: local_dir.path().canonicalize().unwrap(),
            }),
            ToolRegistry::default(),
            ContextBuilder::new("private e2e fixture").with_memory(server_dir.path()),
        )),
        store.clone(),
    );
    runner.enable_environments(auth).unwrap();
    let service = runner.environments().unwrap().clone();
    let chat = Chat::new(&runner, server_dir.path()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base: reqwest::Url = format!("http://{address}/fixture-prefix/").parse().unwrap();
    let settings = Settings {
        trusted_proxies: vec![],
        listen: address,
        base_url: base.clone(),
    };
    let app = crate::web::server::router_with_chat(settings, server_dir.path(), Some(chat));
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let credential_file = local_dir.path().join("device");
    Credential { device_id, token }
        .save(&credential_file)
        .unwrap();
    std::fs::write(
        local_dir.path().join("remote.toml"),
        format!(
            "server = {:?}\ncredential_file = {:?}\nlabel = 'e2e laptop'\nallow_shell = true\n",
            base.as_str(),
            credential_file.display().to_string()
        ),
    )
    .unwrap();
    let config = RemoteConfig::load(&local_dir.path().join("remote.toml")).unwrap();
    let native = ChatClient::new(&config).unwrap();
    let created = native
        .command(&SessionId::new("new"), ChatCommand::New)
        .await
        .unwrap();
    let session = SessionId::new(created["id"].as_str().unwrap());
    store
        .set_default_command_audit_mode(crate::agent::command_audit::CommandAuditMode::Trusted)
        .await
        .unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut worker = Worker::spawn(local_dir.path(), &session);
    let original: EnvironmentKey =
        serde_json::from_str(&wait_file(&local_dir.path().join("attached.json")).await).unwrap();
    let snapshot = native.snapshot(&session).await.unwrap();
    assert_eq!(snapshot.environment.unwrap().state, "connected");
    assert!(snapshot.messages.is_empty());
    assert!(store.load(&session).await.unwrap().is_none());
    assert!(store.list().await.unwrap().is_empty());
    native
        .command(
            &session,
            ChatCommand::Send {
                request_id: Uuid::new_v4(),
                text: "effect".into(),
            },
        )
        .await
        .unwrap();
    wait_idle(&store, &session).await;
    assert!(
        local_dir.path().join("ledger").exists(),
        "missing effect; durable history: {:?}",
        store.load(&session).await.unwrap().unwrap().messages()
    );
    assert_eq!(ledger(local_dir.path()), [format!("{MEMORY}:{LOCAL}")]);
    assert!(
        serde_json::to_string(&native.snapshot(&session).await.unwrap())
            .unwrap()
            .contains("local work finished")
    );

    owner_send(&http, &base, &cookie, &session, "local MCP effect").await;
    wait_idle(&store, &session).await;
    assert_eq!(
        wait_file(&local_dir.path().join("mcp-ledger")).await,
        format!("{MEMORY}:{LOCAL}\n")
    );
    assert!(
        serde_json::to_string(&native.snapshot(&session).await.unwrap())
            .unwrap()
            .contains("local MCP finished")
    );

    // Exercise real private Telegram admission and active-session resolution,
    // not a native HTTP command relabelled as a Telegram message.
    let before_telegram = calls.load(Ordering::SeqCst);
    for (chat, sender) in [(7, 7), (42, 7)] {
        assert!(
            crate::TelegramInterface::private_fixture_input(
                server_dir.path(),
                chat,
                sender,
                "telegram effect"
            )
            .await
            .is_none(),
            "nonowner/spoofed Telegram input admitted"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), before_telegram);
    assert!(!local_dir.path().join("telegram-ledger").exists());
    let owner = crate::TelegramInterface::private_fixture_input(
        server_dir.path(),
        42,
        42,
        "telegram effect",
    )
    .await
    .expect("configured private owner input was not admitted");
    assert_eq!(owner.source.interface.0, "telegram:e2e");
    assert_eq!(owner.source.conversation_id, "42");
    let conversation = owner.session_id();
    let other = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(other.clone())).await.unwrap();
    store.select(&conversation, &other).await.unwrap();
    let isolated = crate::TelegramInterface::private_fixture_input(
        server_dir.path(),
        42,
        42,
        "telegram isolation probe",
    )
    .await
    .unwrap();
    runner
        .run(isolated.session_id(), isolated, None)
        .await
        .unwrap();
    assert_eq!(store.active_id(&conversation).await.unwrap(), other);
    assert!(
        !local_dir.path().join("telegram-ledger").exists(),
        "other session used attached environment"
    );
    store.select(&conversation, &session).await.unwrap();
    runner.run(owner.session_id(), owner, None).await.unwrap();
    assert_eq!(store.active_id(&conversation).await.unwrap(), session);
    assert_eq!(
        wait_file(&local_dir.path().join("telegram-ledger")).await,
        format!("{MEMORY}:{LOCAL}\n")
    );
    assert_eq!(ledger(local_dir.path()), [format!("{MEMORY}:{LOCAL}")]);

    owner_send(&http, &base, &cookie, &session, "start interruptible work").await;
    let shell: u32 = wait_file(&local_dir.path().join("shell.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let child: u32 = wait_file(&local_dir.path().join("child.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    assert!(
        running(shell) && running(child),
        "cleanup fixture was not running"
    );
    let interrupted = store
        .unfinished_runs()
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.session_id == session.as_str())
        .expect("interruptible run missing")
        .id;
    worker.kill();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let disconnected = service
                .status(&session)
                .await
                .is_none_or(|s| s.state != "connected");
            if disconnected && !running(shell) && !running(child) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("worker death did not revoke authority and clean ordinary children");
    wait_idle(&store, &session).await;
    let (run, _) = store.load_run(&interrupted).await.unwrap().unwrap();
    assert!(
        matches!(run.phase, RunPhase::EnvironmentPaused { .. }),
        "disconnect did not pause the in-flight run: {run:?}"
    );
    assert_eq!(
        ledger(local_dir.path()),
        [format!("{MEMORY}:{LOCAL}"), "started".into()]
    );

    owner_send(&http, &base, &cookie, &session, "discuss without laptop").await;
    wait_idle(&store, &session).await;
    let snapshot = native.snapshot(&session).await.unwrap();
    assert!(
        serde_json::to_string(&snapshot)
            .unwrap()
            .contains("server conversation remains usable")
    );
    let before = calls.load(Ordering::SeqCst);
    let mut fresh = Worker::spawn(local_dir.path(), &session);
    let key: EnvironmentKey =
        serde_json::from_str(&wait_file(&local_dir.path().join("attached.json")).await).unwrap();
    assert_ne!(key.generation, original.generation);
    runner
        .recover(RecoveryTarget::Conversation(session.clone()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "reattach/recovery restarted the model"
    );
    assert_eq!(
        ledger(local_dir.path()),
        [format!("{MEMORY}:{LOCAL}"), "started".into()],
        "reattach replayed an effect"
    );
    assert_eq!(
        std::fs::read_to_string(local_dir.path().join("mcp-ledger")).unwrap(),
        format!("{MEMORY}:{LOCAL}\n"),
        "reattach replayed an MCP effect"
    );
    native
        .command(
            &session,
            ChatCommand::Send {
                request_id: Uuid::new_v4(),
                text: "fresh explicit effect".into(),
            },
        )
        .await
        .unwrap();
    wait_idle(&store, &session).await;
    assert_eq!(
        ledger(local_dir.path()),
        [
            format!("{MEMORY}:{LOCAL}"),
            "started".into(),
            format!("{MEMORY}:{LOCAL}")
        ]
    );
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(local_dir.path().join("stop"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = fresh.0.try_wait().unwrap() {
                assert!(status.success(), "fresh worker failed: {status}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fresh worker failed to shut down");
    assert_eq!(
        wait_file(&local_dir.path().join("stopped")).await,
        "local cancellation completed"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while service
            .status(&session)
            .await
            .is_some_and(|s| s.state == "connected")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("local close did not revoke server authority");
    let continuation = crate::TelegramInterface::private_fixture_input(
        server_dir.path(),
        42,
        42,
        "discuss without laptop",
    )
    .await
    .unwrap();
    runner
        .run(continuation.session_id(), continuation, None)
        .await
        .unwrap();
    assert!(
        matches!(store.load(&session).await.unwrap().unwrap().messages().last(), Some(Message::Assistant(reply)) if reply.content == "server conversation remains usable")
    );
    assert_eq!(
        std::fs::read_to_string(local_dir.path().join("telegram-ledger")).unwrap(),
        format!("{MEMORY}:{LOCAL}\n"),
        "Telegram continuation replayed local work"
    );
    owner_send(&http, &base, &cookie, &session, "discuss without laptop").await;
    wait_idle(&store, &session).await;
    let closed = store.load(&session).await.unwrap().unwrap();
    assert!(
        matches!(closed.messages().last(), Some(Message::Assistant(reply)) if reply.content == "server conversation remains usable")
    );
    assert_eq!(
        ledger(local_dir.path()).len(),
        3,
        "local close replayed shell work"
    );
    assert_eq!(
        std::fs::read_to_string(local_dir.path().join("mcp-ledger")).unwrap(),
        format!("{MEMORY}:{LOCAL}\n")
    );
    service.close().await;
}
