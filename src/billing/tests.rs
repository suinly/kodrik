use super::*;
use serde_json::json;

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
