use super::{config::RemoteConfig, local::LocalEnvironment, protocol::*};
use crate::agent::session::SessionId;
use serde_json::json;
use std::{collections::HashMap, time::Duration};
use uuid::Uuid;

fn config(dir: &std::path::Path, shell: bool) -> RemoteConfig {
    RemoteConfig {
        server: reqwest::Url::parse("https://example.invalid/kodrik/").unwrap(),
        credential_file: dir.join("device"),
        label: "Test laptop".into(),
        allow_shell: shell,
        skill_roots: vec![],
        mcp: HashMap::new(),
        optional_mcp: vec![],
    }
}
fn invocation(tool: &str, args: serde_json::Value) -> Invocation {
    Invocation {
        environment: EnvironmentKey {
            connection_id: Uuid::new_v4(),
            generation: Uuid::new_v4(),
        },
        session_id: SessionId::new(Uuid::new_v4().to_string()),
        run_id: Uuid::new_v4().to_string(),
        call_id: "call".into(),
        tool_id: tool.into(),
        arguments: args,
        deadline: chrono::Utc::now() + chrono::Duration::seconds(60),
    }
}

#[tokio::test]
async fn skills_are_read_on_laptop_and_changed_catalog_requires_explicit_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    let skill = skills.join("review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: local review\n---\nRead resource.txt",
    )
    .unwrap();
    std::fs::write(dir.path().join("resource.txt"), "laptop resource").unwrap();
    let mut cfg = config(dir.path(), true);
    cfg.skill_roots = vec![skills];
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let call = invocation("read_file", json!({"path":"resource.txt"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        env.execute(call.clone())
            .await
            .unwrap()
            .result
            .unwrap()
            .output["text"],
        "laptop resource"
    );
    std::fs::write(skill.join("SKILL.md"), "changed catalog").unwrap();
    let saved = env.execute(call.clone()).await;
    let mut changed = call;
    changed.call_id = "changed-skill".into();
    changed.arguments = json!({"path":skill.join("SKILL.md")});
    let result = env.execute(changed).await;
    env.close().await.unwrap();
    assert_eq!(
        saved.unwrap().result.unwrap().output["text"],
        "laptop resource"
    );
    assert!(
        result.is_err(),
        "changed skill was silently served under the previous manifest"
    );
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: updated local review\n---\nUpdated instructions",
    )
    .unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "New laptop instructions").unwrap();
    let refreshed = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    assert_ne!(
        env.manifest().unwrap().skills[0].content_hash,
        refreshed.manifest().unwrap().skills[0].content_hash
    );
    let fresh_call = invocation("read_file", json!({"path":"AGENTS.md"}));
    refreshed
        .attach(
            Uuid::new_v4(),
            fresh_call.environment.clone(),
            fresh_call.session_id.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        refreshed
            .execute(fresh_call)
            .await
            .unwrap()
            .result
            .unwrap()
            .output["text"],
        "New laptop instructions"
    );
    refreshed.close().await.unwrap();
}

#[tokio::test]
async fn optional_mcp_failure_is_visible_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), false);
    cfg.mcp.insert(
        "notes".into(),
        crate::config::McpServerConfig::Http {
            url: "http://127.0.0.1:1/secret-url".into(),
            headers: HashMap::from([("Authorization".into(), "secret-header".into())]),
        },
    );
    cfg.optional_mcp = vec!["notes".into()];
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let manifest = serde_json::to_string(&env.manifest().unwrap()).unwrap();
    env.close().await.unwrap();
    assert!(manifest.contains("notes"));
    assert!(manifest.contains("unavailable"));
    assert!(!manifest.contains("secret-url"));
    assert!(!manifest.contains("secret-header"));
}

#[tokio::test]
async fn local_effect_is_once_across_retry_restart_and_changed_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), true);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let call = invocation("exec_command", json!({"cmd":"echo effect >> count; pwd"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    let done = env.execute(call.clone()).await.unwrap();
    assert_eq!(done.state, OperationState::Completed);
    assert_eq!(done.result.unwrap().output["exit_code"], 0);
    env.execute(call.clone()).await.unwrap();
    let mut changed = call.clone();
    changed.arguments = json!({"cmd":"echo different >> count"});
    assert!(env.execute(changed).await.is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("count")).unwrap(),
        "effect\n"
    );
    env.acknowledge(call.clone()).await.unwrap();
    env.close().await.unwrap();
    assert!(
        env.execute(invocation("exec_command", json!({"cmd":"true"})))
            .await
            .is_err()
    );
    drop(env);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    // A different credential cannot recover the old call's authority.
    assert!(env.execute(call).await.is_err());
    env.close().await.unwrap();
}

#[tokio::test]
async fn read_and_patch_cannot_choose_a_new_root_or_follow_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input"), "local").unwrap();
    std::fs::write(outside.path().join("secret"), "private").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let env = LocalEnvironment::open(&config(dir.path(), true), dir.path())
        .await
        .unwrap();
    let mut call = invocation("read_file", json!({"path":"input"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        env.execute(call.clone())
            .await
            .unwrap()
            .result
            .unwrap()
            .output["text"],
        "local"
    );
    for path in [
        "../secret".to_owned(),
        "link/secret".into(),
        outside.path().join("secret").to_str().unwrap().into(),
    ] {
        call.call_id = Uuid::new_v4().to_string();
        call.arguments = json!({"path":path});
        let result = env.execute(call.clone()).await;
        assert!(
            result.is_err()
                || result
                    .unwrap()
                    .result
                    .unwrap()
                    .output
                    .get("error")
                    .is_some()
        );
    }
    call.call_id = Uuid::new_v4().to_string();
    call.tool_id = "apply_patch".into();
    call.arguments = json!({"patch":"*** Begin Patch\n*** Add File: owned\n+yes\n*** End Patch","workdir":outside.path()});
    assert!(env.execute(call.clone()).await.is_err());
    assert!(!outside.path().join("owned").exists());
    call.call_id = Uuid::new_v4().to_string();
    call.arguments["workdir"] = serde_json::Value::Null;
    env.execute(call).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("owned")).unwrap(),
        "yes\n"
    );
    env.close().await.unwrap();
}

#[tokio::test]
async fn shell_disabled_and_stale_generation_never_start_an_effect() {
    let dir = tempfile::tempdir().unwrap();
    let env = LocalEnvironment::open(&config(dir.path(), false), dir.path())
        .await
        .unwrap();
    assert!(
        !env.manifest()
            .unwrap()
            .tools
            .iter()
            .any(|t| t.kind == ToolKind::Exec)
    );
    let call = invocation("exec_command", json!({"cmd":"echo no > effect"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    assert!(env.execute(call.clone()).await.is_err());
    let mut stale = call;
    stale.environment.generation = Uuid::new_v4();
    assert!(env.execute(stale).await.is_err());
    assert!(!dir.path().join("effect").exists());
    env.close().await.unwrap();
}

#[tokio::test]
async fn cancel_and_close_do_not_wait_forever_for_descendant_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let env = std::sync::Arc::new(
        LocalEnvironment::open(&config(dir.path(), true), dir.path())
            .await
            .unwrap(),
    );
    let call = invocation(
        "exec_command",
        json!({"cmd":"echo ready > started; sleep 30; echo no > effect"}),
    );
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    let task = tokio::spawn({
        let env = env.clone();
        let call = call.clone();
        async move { env.execute(call).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    env.cancel(&call.run_id, &call.call_id).await.unwrap();
    let receipt = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        receipt.state,
        OperationState::Stopped | OperationState::Unknown
    ));
    if receipt.state == OperationState::Unknown {
        assert!(
            receipt.result.is_none(),
            "Unknown is evidence, not a completed tool result"
        );
    }
    tokio::time::timeout(Duration::from_secs(5), env.close())
        .await
        .unwrap()
        .unwrap();
    assert!(!dir.path().join("effect").exists());
}

#[tokio::test]
async fn aborted_execution_cannot_leave_close_waiting_on_an_active_slot() {
    let dir = tempfile::tempdir().unwrap();
    let env = std::sync::Arc::new(
        LocalEnvironment::open(&config(dir.path(), true), dir.path())
            .await
            .unwrap(),
    );
    let call = invocation(
        "exec_command",
        json!({"cmd":"echo yes > started; sleep 30"}),
    );
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    let task = tokio::spawn({
        let env = env.clone();
        async move { env.execute(call).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(5), env.close())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn receipt_quota_is_shared_across_open_environments_and_fails_before_effect() {
    let dir = tempfile::tempdir().unwrap();
    let env = LocalEnvironment::open(&config(dir.path(), true), dir.path())
        .await
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    // Reservations count without allocating a huge test file.
    db.execute(
        "INSERT INTO receipts VALUES('other','device','run','call',X'7b7d',268435456,NULL)",
        [],
    )
    .unwrap();
    let call = invocation("exec_command", json!({"cmd":"echo no > effect"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    assert!(env.execute(call).await.is_err());
    assert!(!dir.path().join("effect").exists());
    env.close().await.unwrap();
}

#[tokio::test]
async fn deleted_journal_fails_closed_before_another_effect() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), true);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let call = invocation("exec_command", json!({"cmd":"echo first >> effect"}));
    let device = Uuid::new_v4();
    env.attach(device, call.environment.clone(), call.session_id.clone())
        .await
        .unwrap();
    env.execute(call.clone()).await.unwrap();
    std::fs::remove_file(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    let mut next = call.clone();
    next.call_id = "next".into();
    assert!(env.execute(next).await.is_err());
    env.close().await.unwrap();
    drop(env);
    assert!(
        LocalEnvironment::open(&cfg, dir.path()).await.is_err(),
        "lost journal must require explicit reconciliation, not reset"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effect")).unwrap(),
        "first\n"
    );
}

#[tokio::test]
async fn changed_ack_cannot_make_unacknowledged_evidence_evictable() {
    let dir = tempfile::tempdir().unwrap();
    let env = LocalEnvironment::open(&config(dir.path(), true), dir.path())
        .await
        .unwrap();
    let call = invocation("exec_command", json!({"cmd":"true"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    env.execute(call.clone()).await.unwrap();
    let mut changed = call;
    changed.arguments = json!({"cmd":"false"});
    assert!(env.acknowledge(changed).await.is_err());
    let db = rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    let ack: Option<i64> = db
        .query_row("SELECT acknowledged FROM receipts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ack, None);
    env.close().await.unwrap();
}

#[test]
fn aborting_patch_waiter_does_not_release_an_unfinished_file_effect() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let env = std::sync::Arc::new(
            LocalEnvironment::open(&config(dir.path(), false), dir.path())
                .await
                .unwrap(),
        );
        let call = invocation(
            "apply_patch",
            json!({"patch":"*** Begin Patch\n*** Add File: effect\n+done\n*** End Patch"}),
        );
        env.attach(
            Uuid::new_v4(),
            call.environment.clone(),
            call.session_id.clone(),
        )
        .await
        .unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        ready.await.unwrap();
        let task = tokio::spawn({
            let env = env.clone();
            async move { env.execute(call).await }
        });
        let db = rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = db
                    .query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))
                    .unwrap();
                if count == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        let closing = tokio::spawn({
            let env = env.clone();
            async move { env.close().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
        blocker.await.unwrap();
        closing.await.unwrap().unwrap();
        assert!(
            dir.path().join("effect").exists(),
            "close returned before an accepted patch's blocking effect completed"
        );
        let receipt: Vec<u8> = db
            .query_row("SELECT payload FROM receipts", [], |r| r.get(0))
            .unwrap();
        let receipt: OperationReceipt = serde_json::from_slice(&receipt).unwrap();
        assert_eq!(
            receipt.state,
            OperationState::Completed,
            "detached patch lost completion evidence"
        );
    });
}

#[tokio::test]
async fn replaced_project_root_symlink_cannot_expose_outside_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "private").unwrap();
    let env = LocalEnvironment::open(&config(dir.path(), false), &project)
        .await
        .unwrap();
    std::fs::rename(&project, dir.path().join("original")).unwrap();
    std::os::unix::fs::symlink(&outside, &project).unwrap();
    let call = invocation("read_file", json!({"path":"secret"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    let receipt = env.execute(call).await.unwrap();
    assert!(
        receipt.result.unwrap().output.get("error").is_some(),
        "root symlink exposed outside content"
    );
    env.close().await.unwrap();
}

#[tokio::test]
async fn same_device_restart_returns_evidence_and_retains_unacked_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), true);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let call = invocation("exec_command", json!({"cmd":"echo once >> effect"}));
    let device = Uuid::new_v4();
    env.attach(device, call.environment.clone(), call.session_id.clone())
        .await
        .unwrap();
    env.execute(call.clone()).await.unwrap();
    let mut changed = call.clone();
    changed.arguments = json!({"cmd":"false"});
    assert!(env.acknowledge(changed).await.is_err());
    env.close().await.unwrap();
    drop(env);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    env.attach(device, call.environment.clone(), call.session_id.clone())
        .await
        .unwrap();
    assert_eq!(
        env.execute(call.clone()).await.unwrap().state,
        OperationState::Completed
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effect")).unwrap(),
        "once\n"
    );
    env.acknowledge(call).await.unwrap();
    env.close().await.unwrap();
    drop(env);
    let db = rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    db.execute("UPDATE receipts SET acknowledged=1", [])
        .unwrap();
    drop(db);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let db = rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    env.close().await.unwrap();
}

#[tokio::test]
async fn bounded_file_and_image_reads_reject_large_and_special_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::File::create(dir.path().join("large"))
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    std::fs::File::create(dir.path().join("large.png"))
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();
    let name =
        std::ffi::CString::new(dir.path().join("fifo").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: NUL-terminated path inside fixture tempdir.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let env = LocalEnvironment::open(&config(dir.path(), false), dir.path())
        .await
        .unwrap();
    let mut call = invocation("read_file", json!({"path":"large"}));
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    for (tool, path) in [
        ("read_file", "large"),
        ("view_image", "large.png"),
        ("read_file", "fifo"),
    ] {
        call.call_id = Uuid::new_v4().to_string();
        call.tool_id = tool.into();
        call.arguments = json!({"path":path});
        let receipt = tokio::time::timeout(Duration::from_secs(5), env.execute(call.clone()))
            .await
            .unwrap()
            .unwrap();
        assert!(receipt.result.unwrap().output.get("error").is_some());
    }
    let bytes = crate::agent::image::tests::sample(image::ImageFormat::Png);
    std::fs::write(dir.path().join("ok.png"), bytes).unwrap();
    call.call_id = Uuid::new_v4().to_string();
    call.tool_id = "view_image".into();
    call.arguments = json!({"path":"ok.png"});
    assert_eq!(
        env.execute(call)
            .await
            .unwrap()
            .result
            .unwrap()
            .images
            .len(),
        1
    );
    env.close().await.unwrap();
}

#[tokio::test]
async fn skill_metadata_does_not_follow_an_external_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join("secret")).unwrap();
    std::fs::write(
        outside.path().join("secret/SKILL.md"),
        "---\nname: secret\ndescription: Private outside metadata\n---\nprivate",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("skills")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret"),
        dir.path().join("skills/secret"),
    )
    .unwrap();
    let mut cfg = config(dir.path(), false);
    cfg.skill_roots = vec![dir.path().join("skills")];
    let opened = LocalEnvironment::open(&cfg, dir.path()).await;
    assert!(
        opened.is_err()
            || opened
                .as_ref()
                .unwrap()
                .manifest()
                .unwrap()
                .skills
                .is_empty(),
        "skill discovery leaked outside-root metadata"
    );
    if let Ok(env) = opened {
        env.close().await.unwrap();
    }
}

#[test]
fn skill_fifo_cannot_block_environment_startup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("skills/stalled")).unwrap();
    let path = std::ffi::CString::new(
        dir.path()
            .join("skills/stalled/SKILL.md")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    // SAFETY: fixture path is NUL-terminated and writable inside tempdir.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote::local_tests::skill_fifo_worker",
            "--nocapture",
        ])
        .env("KODRIK_SKILL_FIFO_FIXTURE", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > Duration::from_secs(3) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if status.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    assert!(
        status.is_some_and(|s| s.success()),
        "skill discovery blocked reading a special file"
    );
}

#[tokio::test]
async fn skill_fifo_worker() {
    let Some(dir) = std::env::var_os("KODRIK_SKILL_FIFO_FIXTURE") else {
        return;
    };
    let root = std::path::Path::new(&dir);
    let mut cfg = config(root, false);
    cfg.skill_roots = vec![root.join("skills")];
    if let Ok(env) = LocalEnvironment::open(&cfg, root).await {
        env.close().await.unwrap();
    }
}

#[tokio::test]
async fn repeated_cancel_does_not_exhaust_supervisor_registrations() {
    let dir = tempfile::tempdir().unwrap();
    let env = LocalEnvironment::open(&config(dir.path(), true), dir.path())
        .await
        .unwrap();
    let mut call = invocation(
        "exec_command",
        json!({"cmd":"echo ready > started; sleep 30"}),
    );
    env.attach(
        Uuid::new_v4(),
        call.environment.clone(),
        call.session_id.clone(),
    )
    .await
    .unwrap();
    for _ in 0..18 {
        call.call_id = Uuid::new_v4().to_string();
        let task = tokio::spawn({
            let env = env.clone();
            let call = call.clone();
            async move { env.execute(call).await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dir.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cleanup registrations leaked across cancellations");
        env.cancel(&call.run_id, &call.call_id).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_file(dir.path().join("started")).unwrap();
    }
    env.close().await.unwrap();
}

#[tokio::test]
async fn replaced_empty_journal_cannot_reset_deduplication() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), false);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    env.close().await.unwrap();
    drop(env);
    std::fs::remove_file(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    drop(rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap());
    assert!(
        LocalEnvironment::open(&cfg, dir.path()).await.is_err(),
        "a replacement database reset evidence identity"
    );
}

#[tokio::test]
async fn concurrent_environments_share_deduplication_and_quota() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), true);
    let first = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let second = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    let call = invocation("exec_command", json!({"cmd":"echo once >> effect"}));
    let device = Uuid::new_v4();
    for env in [&first, &second] {
        env.attach(device, call.environment.clone(), call.session_id.clone())
            .await
            .unwrap();
    }
    let (a, b) = tokio::join!(first.execute(call.clone()), second.execute(call.clone()));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effect")).unwrap(),
        "once\n"
    );
    let db = rusqlite::Connection::open(dir.path().join("receipts/receipts.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('other','device','run','call',X'7b7d',268435456,NULL)",
        [],
    )
    .unwrap();
    let mut next = call;
    next.call_id = "next".into();
    next.arguments = json!({"cmd":"echo no > forbidden"});
    assert!(second.execute(next).await.is_err());
    assert!(!dir.path().join("forbidden").exists());
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn corrupt_journal_fails_closed_without_reinitialization() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), false);
    let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
    env.close().await.unwrap();
    drop(env);
    let path = dir.path().join("receipts/receipts.sqlite3");
    std::fs::write(&path, b"corrupt evidence").unwrap();
    assert!(LocalEnvironment::open(&cfg, dir.path()).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"corrupt evidence");
}
