use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, ensure};
use bashkit::{Bash, ExecOptions, ExecutionLimits, FsLimits, InMemoryFs};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::documents::MAX_DOCUMENT;

static WORKERS: AtomicUsize = AtomicUsize::new(0);
struct WorkerSlot;
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UiView {
    id: String,
    source: String,
    jq: String,
}

#[derive(Deserialize, Serialize)]
struct Job {
    id: String,
    jq: String,
    data: Value,
}

pub(crate) fn source_rows(data: &Value) -> anyhow::Result<Value> {
    if data.is_array() {
        return Ok(data.clone());
    }
    if let Some(records) = data["records"].as_array() {
        return records
            .iter()
            .map(|record| {
                let mut row = record["values"]
                    .as_object()
                    .context("Invalid source record")?
                    .clone();
                row.entry("id").or_insert_with(|| record["id"].clone());
                Ok(Value::Object(row))
            })
            .collect();
    }
    if let Some(groups) = data["groups"].as_array() {
        return groups
            .iter()
            .map(|group| {
                let mut row = serde_json::Map::from_iter([
                    ("group".into(), group["group"].clone()),
                    ("count".into(), group["count"].clone()),
                ]);
                for (field, metric) in group["metrics"]
                    .as_object()
                    .context("Invalid source summary")?
                {
                    row.insert(field.clone(), metric["sum"].clone());
                }
                Ok(Value::Object(row))
            })
            .collect();
    }
    anyhow::bail!("Source must contain rows, records or summary groups")
}

pub(crate) fn validate_value(value: &Value, depth: usize) -> anyhow::Result<()> {
    ensure!(depth <= 16, "View JSON exceeds nesting limit");
    match value {
        Value::String(text) => ensure!(text.len() <= 32768, "View string exceeds 32 KiB"),
        Value::Array(items) => {
            ensure!(items.len() <= 1000, "View has more than 1000 items");
            for item in items {
                validate_value(item, depth + 1)?;
            }
        }
        Value::Object(fields) => {
            ensure!(fields.len() <= 100, "View object has more than 100 fields");
            for (key, item) in fields {
                ensure!(
                    key.len() <= 128
                        && !["__proto__", "prototype", "constructor"].contains(&key.as_str()),
                    "Invalid view field"
                );
                validate_value(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn evaluate(
    views: &[UiView],
    sources: &[Value],
) -> anyhow::Result<BTreeMap<String, Value>> {
    ensure!(views.len() <= 8, "UI documents support at most eight views");
    if views.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut ids = std::collections::HashSet::new();
    let mut jobs = Vec::new();
    for view in views {
        ensure!(
            !view.id.is_empty()
                && view.id.len() <= 64
                && !["__proto__", "constructor", "prototype"].contains(&view.id.as_str())
                && view
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "Invalid view id"
        );
        ensure!(ids.insert(&view.id), "Duplicate view id {}", view.id);
        ensure!(
            !view.jq.trim().is_empty() && view.jq.len() <= 8192,
            "View {} needs a jq filter of 1–8192 bytes",
            view.id
        );
        let source = sources
            .iter()
            .find(|source| source["id"] == view.source)
            .with_context(|| format!("Unknown source for view {}", view.id))?;
        ensure!(
            source["data"]["next_cursor"].is_null(),
            "View {} needs a complete source; narrow the query or use summarize",
            view.id
        );
        jobs.push(Job {
            id: view.id.clone(),
            jq: view.jq.clone(),
            data: source_rows(&source["data"])?,
        });
    }
    let bytes = serde_json::to_vec(&jobs)?;
    ensure!(bytes.len() <= MAX_DOCUMENT, "View inputs exceed 512 KiB");
    WORKERS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            (count < 4).then_some(count + 1)
        })
        .map_err(|_| anyhow::anyhow!("UI view workers are busy; retry shortly"))?;
    let _slot = WorkerSlot;
    let mut input = tempfile::NamedTempFile::new()?;
    input.write_all(&bytes)?;
    let output = tempfile::NamedTempFile::new()?;
    let errors = tempfile::NamedTempFile::new()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errors.reopen()?);
    #[cfg(not(test))]
    command
        .arg("ui-view-worker")
        .arg(input.path())
        .arg(output.path());
    #[cfg(test)]
    command
        .args([
            "--exact",
            "web::views::tests::worker",
            "--ignored",
            "--nocapture",
        ])
        .env("KODRIK_UI_INPUT", input.path())
        .env("KODRIK_UI_OUTPUT", output.path());
    use std::os::unix::process::CommandExt;
    // The jq iterator cannot be preempted inside an async task; enforce limits in a disposable process.
    unsafe {
        command.pre_exec(|| {
            for (resource, limit) in [
                (libc::RLIMIT_CPU, 2),
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_FSIZE, MAX_DOCUMENT as u64 + 16384),
            ] {
                let limits = libc::rlimit {
                    rlim_cur: limit as libc::rlim_t,
                    rlim_max: limit as libc::rlim_t,
                };
                if libc::setrlimit(resource, &limits) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut worker = Worker(command.spawn().context("Cannot start UI view worker")?);
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = worker.0.try_wait()? {
            break status;
        }
        match resident_bytes(worker.0.id()) {
            Ok(bytes) => ensure!(bytes <= 256 * 1024 * 1024, "UI view memory limit exceeded"),
            Err(error) => {
                if let Some(status) = worker.0.try_wait()? {
                    break status;
                }
                // A process may disappear from RSS inspection before waitpid reports its exit.
                if error.raw_os_error() != Some(libc::ESRCH)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(error.into());
                }
            }
        }
        ensure!(Instant::now() < deadline, "UI view computation timed out");
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut error = String::new();
    errors.reopen()?.take(4096).read_to_string(&mut error)?;
    ensure!(
        status.success(),
        "UI view computation failed ({status}; invalid jq or resource limit): {}",
        error.trim()
    );
    ensure!(
        output.as_file().metadata()?.len() <= MAX_DOCUMENT as u64,
        "View output exceeds 512 KiB"
    );
    let result: BTreeMap<String, Value> =
        serde_json::from_reader(output.reopen()?).context("Invalid view worker result")?;
    for value in result.values() {
        validate_value(value, 0)?;
    }
    Ok(result)
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "macos")]
fn resident_bytes(pid: u32) -> std::io::Result<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if read <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    if read != size as i32 {
        return Err(std::io::Error::other(
            "Incomplete UI view memory information",
        ));
    }
    Ok(unsafe { info.assume_init() }.pti_resident_size)
}

#[cfg(target_os = "linux")]
fn resident_bytes(pid: u32) -> std::io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/statm"))?;
    let pages = stat
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| std::io::Error::other("Missing worker RSS"))?
        .parse::<u64>()
        .map_err(std::io::Error::other)?;
    Ok(pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes(_: u32) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "UI views require macOS or Linux process limits",
    ))
}

pub(crate) async fn worker(input: &Path, output: &Path) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    std::fs::File::open(input)?
        .take(MAX_DOCUMENT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_DOCUMENT, "View inputs exceed 512 KiB");
    let jobs: Vec<Job> = serde_json::from_slice(&bytes)?;
    ensure!(jobs.len() <= 8, "Too many views");
    let fs = Arc::new(InMemoryFs::with_limits(
        FsLimits::new()
            .max_total_bytes(MAX_DOCUMENT as u64)
            .max_file_size(MAX_DOCUMENT as u64)
            .max_file_count(16),
    ));
    let mut bash = Bash::builder()
        .fs(fs.clone())
        .limits(ExecutionLimits {
            timeout: Duration::from_secs(1),
            max_stdout_bytes: MAX_DOCUMENT,
            max_stderr_bytes: 4096,
            max_work_units: 1_000_000,
            ..Default::default()
        })
        .build();
    let mut result = BTreeMap::new();
    for job in jobs {
        ensure!(job.jq.len() <= 8192, "View jq filter exceeds 8 KiB");
        let out = bash
            .exec_with_options(
                "jq -c -- \"$1\"",
                ExecOptions::new()
                    .positional([job.jq])
                    .stdin(serde_json::to_string(&job.data)?),
            )
            .await?;
        ensure!(
            out.exit_code == 0 && !out.stdout_truncated,
            "View {}: {}",
            job.id,
            out.stderr
        );
        let value: Value = serde_json::from_str(&out.stdout)
            .with_context(|| format!("View {} must produce exactly one JSON value", job.id))?;
        validate_value(&value, 0)?;
        result.insert(job.id, value);
        ensure!(
            serde_json::to_vec(&result)?.len() <= MAX_DOCUMENT,
            "View output exceeds 512 KiB"
        );
    }
    std::fs::write(output, serde_json::to_vec(&result)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    #[ignore = "internal disposable view worker"]
    async fn worker() {
        let (Ok(input), Ok(output)) = (
            std::env::var("KODRIK_UI_INPUT"),
            std::env::var("KODRIK_UI_OUTPUT"),
        ) else {
            return;
        };
        if let Err(error) = super::worker(Path::new(&input), Path::new(&output)).await {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    }

    #[test]
    fn views_compute_without_shell_interpolation_and_reject_incomplete_sources() {
        let views: Vec<UiView> = serde_json::from_value(json!([
            {"id":"latest","source":"days","jq":"sort_by(.date) | last"},
            {"id":"quoted","source":"days","jq":"\"It's $(cat /host)\""}
        ]))
        .unwrap();
        let sources = vec![
            json!({"id":"days","data":{"records":[{"id":"a","values":{"date":"2026-10-20","kcal":100}},{"id":"b","values":{"date":"2026-10-21","kcal":200}}],"next_cursor":null}}),
        ];
        let result = evaluate(&views, &sources).unwrap();
        assert_eq!(result["latest"]["kcal"], 200);
        assert_eq!(result["quoted"], "It's $(cat /host)");
        let mut incomplete = sources;
        incomplete[0]["data"]["next_cursor"] = json!("more");
        assert!(
            evaluate(&views, &incomplete)
                .unwrap_err()
                .to_string()
                .contains("complete source")
        );
    }

    #[test]
    fn fast_worker_exit_does_not_fail_memory_inspection() {
        let sources = vec![json!({"id":"data","data":[]})];
        let views: Vec<UiView> =
            serde_json::from_value(json!([{"id":"result","source":"data","jq":"."}])).unwrap();
        for _ in 0..30 {
            assert_eq!(evaluate(&views, &sources).unwrap()["result"], json!([]));
        }
    }

    #[test]
    fn runaway_jq_and_invalid_output_do_not_hang_the_host() {
        let sources = vec![json!({"id":"data","data":[]})];
        for jq in [
            ".",
            "def loop: loop; loop",
            "[range(0;1000000000)]",
            "range(0;1000000000)",
            "empty",
            "{constructor:1}",
        ] {
            let views: Vec<UiView> =
                serde_json::from_value(json!([{"id":"test","source":"data","jq":jq}])).unwrap();
            let result = evaluate(&views, &sources);
            if jq == "." {
                assert!(result.is_ok(), "{result:?}");
            } else {
                assert!(result.is_err(), "{jq}");
            }
        }
    }
}
