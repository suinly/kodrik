use super::*;
use serde_json::{Value, json};

fn run(store: &DatasetStore, request: Value) -> anyhow::Result<Value> {
    store.execute(serde_json::from_value(request)?)
}

fn meals(store: &DatasetStore) {
    run(
        store,
        json!({"action":"create", "dataset":"meals", "title":"Meals",
        "fields":[{"name":"date","type":"date","required":true},
                  {"name":"kcal","type":"number","required":true},
                  {"name":"note","type":"string","required":false}],
        "date_field":"date"}),
    )
    .unwrap();
}

fn write(id: &str, date: &str, kcal: f64, revision: Option<i64>) -> Value {
    json!({"action":"upsert", "dataset":"meals", "request_id":uuid::Uuid::new_v4().to_string(),
        "records":[{"id":id,"expected_revision":revision,"values":[
            {"field":"date","value":date},{"field":"kcal","value":kcal}]}]})
}

#[test]
fn writes_survive_reopen_and_duplicate_requests_do_not_reapply() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    let first = write("breakfast", "2026-10-20", 350.0, None);
    let receipt = run(&store, first.clone()).unwrap();
    assert_eq!(receipt["records"], json!([{"id":"breakfast","revision":1}]));
    let updated = run(&store, write("breakfast", "2026-10-20", 400.0, Some(1))).unwrap();
    assert_eq!(updated["records"][0]["revision"], 2);
    assert_eq!(run(&store, first.clone()).unwrap(), receipt);
    let mut conflicting = first;
    conflicting["records"][0]["values"][1]["value"] = json!(999);
    assert!(run(&store, conflicting).is_err());
    drop(store);
    let reopened = DatasetStore::open(dir.path().to_owned()).unwrap();
    let result = run(&reopened, json!({"action":"query","dataset":"meals"})).unwrap();
    assert_eq!(
        result["records"][0]["values"],
        json!({"date":"2026-10-20","kcal":400.0})
    );
    assert_eq!(result["records"][0]["revision"], 2);
}

#[test]
fn invalid_batches_and_stale_updates_preserve_previous_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    run(&store, write("breakfast", "2026-10-20", 350.0, None)).unwrap();
    let mut batch = write("breakfast", "2026-10-20", 450.0, Some(1));
    batch["records"]
        .as_array_mut()
        .unwrap()
        .push(write("bad", "2026-02-30", 1.0, None)["records"][0].clone());
    assert!(run(&store, batch).is_err());
    for revision in [None, Some(0), Some(2)] {
        assert!(run(&store, write("breakfast", "2026-10-20", 999.0, revision)).is_err());
    }
    let records = run(&store, json!({"action":"query","dataset":"meals"})).unwrap();
    assert_eq!(records["records"].as_array().unwrap().len(), 1);
    assert_eq!(records["records"][0]["values"]["kcal"], 350.0);
}

#[test]
fn rejects_invalid_definitions_values_and_unrelated_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    for invalid in [
        json!({"action":"list","dataset":"meals"}),
        json!({"action":"list","unknown":true}),
        json!({"action":"describe","dataset":"../meals"}),
        json!({"action":"create","dataset":"meals","title":"changed","fields":[]}),
        json!({"action":"create","dataset":"other","title":"Other", "date_field":"missing",
            "fields":[{"name":"kcal","type":"number","required":true}]}),
    ] {
        assert!(run(&store, invalid.clone()).is_err(), "{invalid}");
    }
    for bad in [Value::Null, json!(true), json!("350"), json!({"nested":1})] {
        let mut request = write("bad", "2026-10-20", 350.0, None);
        request["records"][0]["values"][1]["value"] = bad;
        assert!(run(&store, request).is_err());
    }
    let mut missing = write("bad", "2026-10-20", 350.0, None);
    missing["records"][0]["values"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert!(run(&store, missing).is_err());
    let mut unknown = write("bad", "2026-10-20", 350.0, None);
    unknown["records"][0]["values"][1]["field"] = json!("unknown");
    assert!(run(&store, unknown).is_err());
    let mut duplicate = write("bad", "2026-10-20", 350.0, None);
    let record = duplicate["records"][0].clone();
    duplicate["records"].as_array_mut().unwrap().push(record);
    assert!(run(&store, duplicate).is_err());
}

#[test]
fn queries_page_consistently_and_summaries_use_matching_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    for (id, day, kcal) in [
        ("a", "2026-10-20", 350.0),
        ("b", "2026-10-20", 650.0),
        ("c", "2026-10-21", 400.0),
    ] {
        run(&store, write(id, day, kcal, None)).unwrap();
    }
    let summary = run(
        &store,
        json!({"action":"summarize","dataset":"meals","metrics":["kcal"],"group_by":"day"}),
    )
    .unwrap();
    assert_eq!(
        summary["groups"],
        json!([
            {"group":"2026-10-20","count":2,"metrics":{"kcal":{"sum":1000.0,"average":500.0,"count":2}}},
            {"group":"2026-10-21","count":1,"metrics":{"kcal":{"sum":400.0,"average":400.0,"count":1}}}
        ])
    );
    let summary = run(
        &store,
        json!({"action":"summarize","dataset":"meals","metrics":["kcal"]}),
    )
    .unwrap();
    assert_eq!(summary["groups"][0]["count"], 3);
    assert_eq!(summary["groups"][0]["metrics"]["kcal"]["sum"], 1400.0);
    let first = run(
        &store,
        json!({"action":"query","dataset":"meals","limit":1}),
    )
    .unwrap();
    assert_eq!(first["records"][0]["id"], "a");
    let next = run(
        &store,
        json!({"action":"query","dataset":"meals","cursor":first["next_cursor"],"limit":1}),
    )
    .unwrap();
    assert_eq!(next["records"][0]["id"], "b");
    let filtered = run(
        &store,
        json!({"action":"query","dataset":"meals","date_from":"2026-10-20","date_to":"2026-10-21",
        "filters":[{"field":"kcal","value":650}]}),
    )
    .unwrap();
    assert_eq!(filtered["records"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["records"][0]["id"], "b");
    assert!(run(&store, json!({"action":"query","dataset":"meals","cursor":first["next_cursor"],"date_from":"2026-10-20"})).is_err());
    run(&store, write("d", "2026-10-21", 1.0, None)).unwrap();
    assert!(
        run(
            &store,
            json!({"action":"query","dataset":"meals","cursor":first["next_cursor"]})
        )
        .is_err()
    );
}

#[test]
fn retries_are_independent_of_field_order_and_numeric_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    let mut request = write("a", "2026-10-20", 350.0, None);
    let original = run(&store, request.clone()).unwrap();
    request["records"][0]["values"][1]["value"] = json!(350);
    request["records"][0]["values"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert_eq!(run(&store, request).unwrap(), original);
}

#[test]
fn optional_numeric_fields_empty_results_and_typed_filters() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    run(
        &store,
        json!({"action":"create","dataset":"items","title":"Items","fields":[
        {"name":"n","type":"integer","required":false},
        {"name":"ok","type":"boolean","required":true}]}),
    )
    .unwrap();
    for (id, cells) in [
        (
            "a",
            json!([{"field":"n","value":10},{"field":"ok","value":true}]),
        ),
        ("b", json!([{"field":"ok","value":false}])),
    ] {
        run(&store,json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),
            "records":[{"id":id,"values":cells,"expected_revision":null}]})).unwrap();
    }
    let total = run(
        &store,
        json!({"action":"summarize","dataset":"items","metrics":["n"]}),
    )
    .unwrap();
    assert_eq!(
        total["groups"],
        json!([{"group":null,"count":2,"metrics":{"n":{"sum":10.0,"average":10.0,"count":1}}}])
    );
    let filtered = run(
        &store,
        json!({"action":"query","dataset":"items","filters":[{"field":"ok","value":false}]}),
    )
    .unwrap();
    assert_eq!(filtered["records"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["records"][0]["id"], "b");
    let empty = run(&store,json!({"action":"summarize","dataset":"items","metrics":["n"],"filters":[{"field":"n","value":123}]})).unwrap();
    assert_eq!(
        empty["groups"],
        json!([{"group":null,"count":0,"metrics":{"n":{"sum":0.0,"average":null,"count":0}}}])
    );
    for invalid in [
        json!({"action":"query","dataset":"items","date_from":"2026-01-01"}),
        json!({"action":"query","dataset":"items","filters":[{"field":"ok","value":1}]}),
        json!({"action":"query","dataset":"items","filters":[{"field":"n","value":1.5}]}),
        json!({"action":"summarize","dataset":"items","metrics":["ok"]}),
        json!({"action":"summarize","dataset":"items","metrics":["n","n"]}),
        json!({"action":"summarize","dataset":"items","group_by":"day"}),
    ] {
        assert!(run(&store, invalid.clone()).is_err(), "{invalid}");
    }
}

#[test]
fn multiyear_diary_has_bounded_pages_and_database_aggregates() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let mut batch = Vec::new();
    for day in 0..800 {
        for meal in 0..10 {
            let mut record = write(
                &format!("d{day}m{meal}"),
                &(start + chrono::Days::new(day)).to_string(),
                100.0,
                None,
            )["records"][0]
                .clone();
            record["values"]
                .as_array_mut()
                .unwrap()
                .push(json!({"field":"note","value":"я".repeat(1024)}));
            batch.push(record);
        }
        if batch.len() == 100 {
            run(&store,json!({"action":"upsert","dataset":"meals","request_id":uuid::Uuid::new_v4().to_string(),"records":batch})).unwrap();
            batch.clear();
        }
    }
    let page = run(
        &store,
        json!({"action":"query","dataset":"meals","limit":200}),
    )
    .unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= 64 * 1024);
    let len = page["records"].as_array().unwrap().len();
    assert!(len > 0 && len < 200);
    let next = run(
        &store,
        json!({"action":"query","dataset":"meals","cursor":page["next_cursor"]}),
    )
    .unwrap();
    assert_eq!(
        next["records"][0]["id"],
        format!("d{}m{}", len / 10, len % 10)
    );
    let year = run(&store,json!({"action":"summarize","dataset":"meals","date_from":"2024-01-01","date_to":"2025-01-01","metrics":["kcal"]})).unwrap();
    assert_eq!(year["groups"][0]["count"], 3660);
    assert_eq!(year["groups"][0]["metrics"]["kcal"]["sum"], 366000.0);
    let months = run(
        &store,
        json!({"action":"summarize","dataset":"meals","metrics":["kcal"],"group_by":"month"}),
    )
    .unwrap();
    assert_eq!(months["groups"][0]["group"], "2024-01");
    assert_eq!(months["groups"][0]["metrics"]["kcal"]["sum"], 31000.0);
    assert!(
        run(
            &store,
            json!({"action":"summarize","dataset":"meals","group_by":"day"})
        )
        .is_err()
    );
    let days = run(&store,json!({"action":"summarize","dataset":"meals","date_from":"2024-02-28","date_to":"2024-03-01","metrics":["kcal"],"group_by":"day"})).unwrap();
    assert_eq!(days["groups"].as_array().unwrap().len(), 2);
    assert_eq!(days["groups"][1]["group"], "2024-02-29");
    assert_eq!(days["groups"][1]["metrics"]["kcal"]["sum"], 1000.0);
}

#[test]
fn concurrent_connections_do_not_overwrite_each_others_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    run(&store, write("a", "2026-10-20", 1.0, None)).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut writers = Vec::new();
    for value in [2.0, 3.0] {
        let other = DatasetStore::open(dir.path().to_owned()).unwrap();
        let barrier = barrier.clone();
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            run(&other, write("a", "2026-10-20", value, Some(1))).is_ok()
        }));
    }
    let results: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.into_iter().filter(|success| *success).count(), 1);
    let row = run(&store, json!({"action":"query","dataset":"meals"})).unwrap();
    assert_eq!(row["records"][0]["revision"], 2);
}

#[test]
fn fails_closed_on_corruption_newer_versions_and_symlinks() {
    for newer in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        if newer {
            let db = Connection::open(&path).unwrap();
            db.pragma_update(None, "user_version", 999).unwrap();
        } else {
            fs::write(&path, b"corrupt database evidence").unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert!(DatasetStore::open(dir.path().to_owned()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, b"do not overwrite").unwrap();
    let store = dir.path().join("data");
    fs::create_dir(&store).unwrap();
    std::os::unix::fs::symlink(&outside, store.join("state.sqlite3")).unwrap();
    assert!(DatasetStore::open(store).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"do not overwrite");
}

#[test]
fn list_paging_and_spaces_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let owner = DatasetStore::open(dir.path().join("owner")).unwrap();
    let group = DatasetStore::open(dir.path().join("group")).unwrap();
    meals(&owner);
    assert!(run(&group, json!({"action":"describe","dataset":"meals"})).is_err());
    run(&owner,json!({"action":"create","dataset":"other","title":"Other","fields":[{"name":"x","type":"string","required":true}]})).unwrap();
    let first = run(&owner, json!({"action":"list","limit":1})).unwrap();
    assert_eq!(
        first["datasets"],
        json!([{"dataset":"meals","title":"Meals"}])
    );
    let next = run(
        &owner,
        json!({"action":"list","limit":1,"cursor":first["next_cursor"]}),
    )
    .unwrap();
    assert_eq!(
        next["datasets"],
        json!([{"dataset":"other","title":"Other"}])
    );
    assert_eq!(next["next_cursor"], Value::Null);
    for limit in [0, 201] {
        assert!(
            run(
                &owner,
                json!({"action":"query","dataset":"meals","limit":limit})
            )
            .is_err()
        );
    }
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(dir.path().join("owner"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.path().join("owner/state.sqlite3"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn late_revision_conflicts_roll_back_records_and_the_write_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    run(&store, write("a", "2026-10-20", 100.0, None)).unwrap();
    run(&store, write("b", "2026-10-20", 200.0, None)).unwrap();
    let mut request = write("a", "2026-10-20", 999.0, Some(1));
    request["records"]
        .as_array_mut()
        .unwrap()
        .push(write("b", "2026-10-20", 888.0, Some(2))["records"][0].clone());
    assert!(run(&store, request.clone()).is_err());
    let original = run(&store, json!({"action":"query","dataset":"meals"})).unwrap();
    assert_eq!(original["revision"], 2);
    assert_eq!(original["records"][0]["values"]["kcal"], 100.0);
    assert_eq!(original["records"][0]["revision"], 1);
    request["records"][1]["expected_revision"] = json!(1);
    // Failed batches must not reserve the request ID or commit their first row.
    let receipt = run(&store, request).unwrap();
    assert_eq!(receipt["revision"], 3);
    assert_eq!(
        receipt["records"],
        json!([{"id":"a","revision":2},{"id":"b","revision":2}])
    );
}

#[test]
fn input_limits_and_nonfinite_totals_fail_without_partial_results() {
    let dir = tempfile::tempdir().unwrap();
    let store = DatasetStore::open(dir.path().to_owned()).unwrap();
    meals(&store);
    let mut overlong = write("bad", "2026-10-20", 100.0, None);
    overlong["records"][0]["values"]
        .as_array_mut()
        .unwrap()
        .push(json!({"field":"note","value":"x".repeat(2049)}));
    assert!(run(&store, overlong).is_err());
    let mut too_many = write("bad", "2026-10-20", 100.0, None);
    too_many["records"] = json!(
        (0..101)
            .map(|i| write(&format!("r{i}"), "2026-10-20", 100.0, None)["records"][0].clone())
            .collect::<Vec<_>>()
    );
    assert!(run(&store, too_many).is_err());
    let mut oversized = write("bad", "2026-10-20", 100.0, None);
    oversized["records"][0]["values"]
        .as_array_mut()
        .unwrap()
        .push(json!({"field":"note","value":"x".repeat(256*1024)}));
    assert!(run(&store, oversized).is_err());
    assert_eq!(
        run(&store, json!({"action":"query","dataset":"meals"})).unwrap()["records"],
        json!([])
    );
    for id in ["a", "b"] {
        run(&store, write(id, "2026-10-20", 1e308, None)).unwrap();
    }
    assert!(
        run(
            &store,
            json!({"action":"summarize","dataset":"meals","metrics":["kcal"]})
        )
        .is_err()
    );
    // The individual finite measurements remain readable after aggregation fails.
    assert_eq!(
        run(&store, json!({"action":"query","dataset":"meals"})).unwrap()["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn startup_refuses_orphan_sqlite_sidecars() {
    for name in ["state.sqlite3-wal", "state.sqlite3-shm"] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(name), b"preserve recovery evidence").unwrap();
        assert!(DatasetStore::open(dir.path().to_owned()).is_err());
        assert!(!dir.path().join("state.sqlite3").exists());
        assert_eq!(
            fs::read(dir.path().join(name)).unwrap(),
            b"preserve recovery evidence"
        );
    }
}
