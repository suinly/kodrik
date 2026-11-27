use super::*;
use serde_json::json;

#[test]
fn account_usage_does_not_report_zero_cost_for_pre_ledger_history() -> anyhow::Result<()> {
    let db = Connection::open_in_memory()?;
    db.execute_batch(
        "CREATE TABLE sessions(id TEXT PRIMARY KEY, payload BLOB NOT NULL);
         INSERT INTO sessions VALUES('old', CAST('{\"messages\":[{}],\"token_usage\":{\"requests\":2}}' AS BLOB));",
    )?;
    for version in [17, 18, 19] {
        if version == 18 {
            migrate(&db)?;
        } else if version == 19 {
            migrate_child_origin(&db)?;
        }
        let usage = read_account_usage(&db)?;
        assert_eq!((usage.calls, usage.cost_usd), (0, None));
        assert!(usage.partial && usage.tokens_partial);
    }
    Ok(())
}

#[test]
fn saved_account_usage_marks_pending_legacy_import_without_importing() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let directory = root.path().join("sessions");
    let store = SqliteSessionStore::open(directory.clone())?;
    let db_path = directory.join(crate::agent::session::DATABASE);
    let db = Connection::open(&db_path)?;
    db.execute("DELETE FROM schema_meta WHERE key='legacy_imported'", [])?;
    std::fs::write(directory.join("legacy.json"), "not imported by reporting")?;
    let pending = saved_account_usage(root.path())?;
    assert!(pending.partial && pending.tokens_partial && pending.cost_usd.is_none());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    drop((db, store));
    std::fs::remove_file(&db_path)?;
    let json_only = saved_account_usage(root.path())?;
    assert!(json_only.partial && json_only.tokens_partial && json_only.cost_usd.is_none());
    assert!(!db_path.exists());
    Ok(())
}

#[test]
fn account_usage_keeps_known_cost_even_when_transcript_cannot_be_checked() -> anyhow::Result<()> {
    let db = Connection::open_in_memory()?;
    db.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY, payload BLOB NOT NULL);")?;
    migrate(&db)?;
    db.execute_batch("INSERT INTO sessions VALUES('bad',X'00');
        INSERT INTO model_calls (id,session_id,generation,model,kind,input_tokens,cached_tokens,output_tokens,cost_usd,partial)
        VALUES('priced','bad','generation','model','chat',11,2,5,0.01,0);")?;
    let usage = read_account_usage(&db)?;
    assert_eq!(
        (usage.calls, usage.input_tokens, usage.cost_usd),
        (1, 11, Some(0.01))
    );
    assert!(usage.partial && usage.tokens_partial);
    Ok(())
}

#[test]
fn child_origin_migration_preserves_v18_charges_when_replayed() -> anyhow::Result<()> {
    let db = Connection::open_in_memory()?;
    db.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY);")?;
    migrate(&db)?;
    db.execute_batch(
        "INSERT INTO model_calls (id,session_id,generation,model,kind,cost_usd,partial)
        VALUES ('legacy','chat','generation','model','subagent',1.25,0);",
    )?;
    migrate_child_origin(&db)?;
    db.execute_batch("PRAGMA user_version=18;")?;
    migrate_child_origin(&db)?;
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))?,
        19
    );
    let (cost, child): (f64, Option<String>) = db.query_row(
        "SELECT cost_usd,child_id FROM model_calls WHERE id='legacy'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!((cost, child), (1.25, None));
    Ok(())
}

fn priced_call(id: &str) -> Call<'_> {
    Call {
        id,
        model: "provider/model",
        kind: "primary",
        run_id: Some("run"),
        origin: None,
        usage: GenerationUsage {
            input_tokens: Some(1_000_000),
            cached_tokens: Some(500_000),
            output_tokens: Some(500_000),
            total_tokens: Some(1_500_000),
        },
        rates: Some(Rates {
            input_usd_per_million: Some(4.0),
            cached_input_usd_per_million: Some(1.0),
            output_usd_per_million: Some(8.0),
        }),
    }
}

fn insert_report_call(db: &Connection, id: &str) -> anyhow::Result<()> {
    db.execute(
        "INSERT INTO model_calls (id,session_id,generation,model,kind,input_tokens,cached_tokens,output_tokens,cost_usd,partial)
         VALUES (?1,'chat','gen','provider/model','chat',11,2,5,0.25,0)",
        [id],
    )?;
    Ok(())
}

#[tokio::test]
async fn recording_sets_date_and_usage_updates_keep_the_original_date() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().into())?;
    let now = || std::time::UNIX_EPOCH.elapsed().unwrap().as_secs() as i64;
    let started = now();
    let mut pending = priced_call("new");
    pending.usage = GenerationUsage::default();
    record_call(&store, "chat", "gen", pending).await?;
    let db = Connection::open(directory.path().join(crate::agent::session::DATABASE))?;
    let date: i64 = db.query_row(
        "SELECT created_at FROM model_calls WHERE id='new'",
        [],
        |r| r.get(0),
    )?;
    assert!((started..=now()).contains(&date));
    db.execute_batch(
        "UPDATE model_calls SET created_at=123 WHERE id='new';
      INSERT INTO model_calls (id,session_id,generation,model,kind,partial)
      VALUES ('historic','chat','gen','provider/model','primary',1);",
    )?;
    for id in ["new", "historic"] {
        record_call(&store, "chat", "gen", priced_call(id)).await?;
    }
    let rows = db
        .prepare("SELECT id, created_at, cost_usd FROM model_calls ORDER BY id")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(
        rows,
        [
            ("historic".into(), None, 6.5),
            ("new".into(), Some(123), 6.5)
        ]
    );
    Ok(())
}

#[test]
fn account_report_reads_old_schema_without_migrating_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(crate::agent::session::DATABASE);
    let db = Connection::open(&path)?;
    db.execute_batch(
        "CREATE TABLE sessions(id TEXT PRIMARY KEY, payload BLOB);
        CREATE TABLE schema_meta(key TEXT PRIMARY KEY, value TEXT);
        INSERT INTO schema_meta VALUES('legacy_imported','1');",
    )?;
    migrate(&db)?;
    insert_report_call(&db, "old")?;
    assert_eq!(
        serde_json::to_value(account_report(directory.path(), None, None, None)?)?,
        json!({
            "usage":{"calls":1,"input_tokens":11,"cached_tokens":2,"output_tokens":5,"cost_usd":0.25,"partial":false,"tokens_partial":false},
            "calls":[{"id":"old","model":"provider/model","created_at":null,"cost_usd":0.25,"partial":false}],
            "next":null,"undated_calls":1
        })
    );
    let period = account_report(directory.path(), Some(0), None, None)?;
    assert_eq!(
        (period.usage.calls, period.calls.len(), period.undated_calls),
        (0, 0, 1)
    );
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?,
        18
    );
    Ok(())
}

#[test]
fn account_report_handles_missing_storage_and_rejects_malformed_cursor() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let directory = root.path().join("sessions");
    let value = serde_json::to_value(account_report(&directory, None, None, None)?)?;
    assert_eq!(value["usage"]["calls"], 0);
    assert_eq!(value["calls"], json!([]));
    assert_eq!(value["undated_calls"], 0);
    assert!(!directory.exists());
    assert!(
        matches!(account_report(&directory, None, None, Some("malformed!")),
        Err(error) if error.is::<InvalidCursor>())
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn account_report_rejects_symlinked_account_directory() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let alias = root.path().join("account");
    std::os::unix::fs::symlink(root.path(), &alias)?;
    assert!(account_report(&alias.join("sessions"), None, None, None).is_err());
    Ok(())
}

#[test]
fn account_report_bounds_apply_to_totals_and_details() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let _store = SqliteSessionStore::open(directory.path().into())?;
    let db = Connection::open(directory.path().join(crate::agent::session::DATABASE))?;
    for (id, date) in [
        ("prior", Some(-1)),
        ("first", Some(0)),
        ("last", Some(9)),
        ("after", Some(10)),
        ("undated", None),
    ] {
        insert_report_call(&db, id)?;
        db.execute(
            "UPDATE model_calls SET created_at=?1 WHERE id=?2",
            params![date, id],
        )?;
    }
    for (from, to, expected) in [
        (Some(0), Some(10), vec!["last", "first"]),
        (Some(0), None, vec!["after", "last", "first"]),
        (None, Some(0), vec!["prior"]),
        (
            None,
            None,
            vec!["after", "last", "first", "prior", "undated"],
        ),
    ] {
        let report = account_report(directory.path(), from, to, None)?;
        assert_eq!(
            report
                .calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(report.usage.calls, expected.len() as i64);
        assert_eq!(report.usage.cost_usd, Some(expected.len() as f64 * 0.25));
        assert_eq!(report.undated_calls, 1);
    }
    Ok(())
}

#[test]
fn account_report_pages_across_equal_timestamps_and_undated_rows() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let _store = SqliteSessionStore::open(directory.path().into())?;
    let db = Connection::open(directory.path().join(crate::agent::session::DATABASE))?;
    for n in 0..241 {
        let id = format!("call-{n:03}");
        insert_report_call(&db, &id)?;
        let date = (n < 120).then_some(-1);
        db.execute(
            "UPDATE model_calls SET created_at=?1 WHERE id=?2",
            params![date, id],
        )?;
    }
    let mut ids = Vec::new();
    let mut before = None;
    for _ in 0..10 {
        let page = account_report(directory.path(), None, None, before.as_deref())?;
        assert_eq!((page.usage.calls, page.undated_calls), (241, 121));
        assert!(page.calls.len() <= 100);
        ids.extend(page.calls.into_iter().map(|call| call.id));
        before = page.next;
        if before.is_none() {
            break;
        }
    }
    assert!(before.is_none());
    assert_eq!(
        ids,
        (0..120)
            .rev()
            .chain((120..241).rev())
            .map(|n| format!("call-{n:03}"))
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
async fn child_calls_are_counted_once_and_model_history_is_complete() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().into())?;
    record_call(&store, "chat", "generation", priced_call("parent")).await?;
    for index in 0..101 {
        let id = index.to_string();
        let mut call = priced_call(&id);
        call.model = "provider/child-model";
        call.kind = "subagent";
        call.origin = Some(("child", "child-generation", Some("child-run")));
        record_call(&store, "chat", "generation", call).await?;
    }
    assert_eq!(
        child_cost(&store, "chat", "generation", "child")
            .await?
            .unwrap()
            .total_usd,
        656.5
    );
    assert!(
        child_cost(&store, "chat", "generation", "other")
            .await?
            .is_none()
    );
    assert!(
        child_cost(&store, "chat", "old-generation", "child")
            .await?
            .is_none()
    );
    let cost = session_cost(&store, "chat", "generation").await?;
    assert_eq!(cost.total_usd, 663.0);
    assert_eq!(
        session_cost(&store, "child", "child-generation")
            .await?
            .total_usd,
        656.5
    );
    assert_eq!(
        run_cost(&store, "child", "child-generation", "child-run")
            .await?
            .unwrap()
            .total_usd,
        656.5
    );
    assert_eq!(
        cost.models
            .iter()
            .find(|model| model.model == "provider/model")
            .unwrap()
            .count,
        1
    );
    let page = model_history(&store, "chat", "generation", "provider/child-model", None).await?;
    assert_eq!(page.calls.len(), 100);
    assert_eq!(page.calls[0].id, "100");
    let tail = model_history(
        &store,
        "chat",
        "generation",
        "provider/child-model",
        page.next,
    )
    .await?;
    assert_eq!(tail.calls[0].id, "0");
    assert!(tail.next.is_none());
    Ok(())
}

#[tokio::test]
async fn price_snapshot_survives_reopen_and_duplicate_write() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().into())?;
    let before = store.view_revision().await?;
    record_call(&store, "chat", "generation", priced_call("first")).await?;
    let after = store.view_revision().await?;
    assert!(
        after > before,
        "a new charge must invalidate live web snapshots"
    );
    let mut duplicate = priced_call("first");
    duplicate.rates = None;
    record_call(&store, "chat", "generation", duplicate).await?;
    assert_eq!(store.view_revision().await?, after);
    drop(store);

    let store = SqliteSessionStore::open(directory.path().into())?;
    let cost = session_cost(&store, "chat", "generation").await?;
    assert_eq!(
        serde_json::to_value(cost)?,
        json!({"total_usd":6.5,"partial":false,"models":[{"model":"provider/model","total_usd":6.5,"partial":false,"count":1}],"calls":[{
            "id":"first","model":"provider/model","kind":"primary","cost_usd":6.5,"run_id":"run"
        }]})
    );
    assert!(
        session_cost(&store, "chat", "new-generation")
            .await?
            .calls
            .is_empty()
    );
    assert!(
        session_cost(&store, "other-chat", "generation")
            .await?
            .calls
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn totals_include_every_call_but_details_are_bounded_and_newest_first() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().into())?;
    for index in 0..101 {
        record_call(
            &store,
            "chat",
            "generation",
            priced_call(&index.to_string()),
        )
        .await?;
    }
    let cost = session_cost(&store, "chat", "generation").await?;
    assert_eq!(cost.total_usd, 656.5);
    assert!(!cost.partial);
    assert_eq!(cost.calls.len(), 20);
    assert_eq!(cost.calls[0].id, "100");
    assert_eq!(cost.calls[19].id, "81");
    Ok(())
}

#[tokio::test]
async fn incomplete_calls_include_only_known_cost_components() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = SqliteSessionStore::open(directory.path().into())?;
    let mut missing_rate = priced_call("missing-rate");
    missing_rate
        .rates
        .as_mut()
        .unwrap()
        .cached_input_usd_per_million = None;
    record_call(&store, "chat", "generation", missing_rate).await?;
    let mut missing_usage = priced_call("missing-usage");
    missing_usage.usage.cached_tokens = None;
    record_call(&store, "chat", "generation", missing_usage).await?;
    let mut unknown = priced_call("unknown");
    unknown.usage = GenerationUsage::default();
    unknown.run_id = None;
    record_call(&store, "chat", "generation", unknown).await?;

    let cost = session_cost(&store, "chat", "generation").await?;
    assert_eq!(cost.total_usd, 10.0);
    assert!(cost.partial);
    assert_eq!(cost.calls[0].cost_usd, None);
    assert_eq!(cost.calls[0].run_id, None);
    assert_eq!(cost.calls[1].cost_usd, Some(4.0));
    assert_eq!(cost.calls[2].cost_usd, Some(6.0));
    Ok(())
}
