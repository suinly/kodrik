use serde_json::{Value, json};
use std::{io::Write, process::Command};

#[test]
fn view_worker_cli_uses_the_requested_files_without_loading_provider_config() {
    let mut input = tempfile::NamedTempFile::new().unwrap();
    let output = tempfile::NamedTempFile::new().unwrap();
    input.write_all(&serde_json::to_vec(&json!([{"id":"latest","jq":"sort_by(.date) | last","data":[{"date":"2026-10-20","value":1},{"date":"2026-10-21","value":2}]}])).unwrap()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .arg("ui-view-worker")
        .arg(input.path())
        .arg(output.path())
        .env_clear()
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: Value = serde_json::from_reader(output.reopen().unwrap()).unwrap();
    assert_eq!(value["latest"]["value"], 2);
}
