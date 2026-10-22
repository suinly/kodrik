# Durable recovery: evidence and limits

Kodrik checkpoints input, decisions, tool intents/results and terminal answers in
SQLite. A crash may repeat an unfinished read-only model request. It must not
mechanically repeat a tool with an unknown outcome unless both the saved and
current tool policies permit replay and the tool remains available. This is not
an exactly-once guarantee for external systems or for decisions a model makes
in a later turn.

For incident handling use [Operations](operations.md#recovering-after-a-crash),
[Backups](storage.md#back-up-and-restore) and [Sessions](sessions.md#recovery-and-stopping).
This page records the tests and measurement methodology for the durable rollout.

## Crash matrix

The process fixtures run against temporary stores with fake providers and a
loopback service. The service runs in the parent process, fsyncs its own event
ledger, and survives SIGKILL of the child. A file gate identifies the exact
boundary. Tests kill the worker without cleanup, reopen SQLite and check both
persisted history and the independent effect count. No real API/Telegram
credentials are used. Delivery fixtures exercise the receipt protocol with a
fake interface, not Telegram's network service.

`matrix` below means `agent::durable_fixture::durable_process_crash_matrix`.

| Failure boundary | Evidence | Required result |
| --- | --- | --- |
| Before admission commit | matrix, `admission-before` | Rollback; same request ID may be submitted |
| After admission, before acknowledgement | matrix, `admission-after` | Duplicate returns the same submission |
| Routing decision before binding commit | matrix, `routing` | Old active mapping survives; safe routing repeats and commits atomically |
| During streaming | matrix, `stream` | Saved old partial is Interrupted; replacement/history do not contain its text |
| After assistant/tool-call commit | matrix, `decision` | Saved call executes; no fresh model decision first |
| Unsafe intent before tool invocation | matrix, `unsafe` | Interrupted/unknown result; zero external effects |
| Safe intent before tool invocation | matrix, `safe`, `revoked` | Replay only while current policy is also Safe |
| Effect accepted before result commit | matrix, `effect` | Exactly one observed effect; recovery reports unknown |
| Result committed before next model | matrix, `result` | Saved result reused; no second tool invocation |
| Final committed before delivery | matrix, `final` | Saved receipt sent; no model regeneration |
| Delivery accepted before receipt acknowledgement | matrix, `delivery` | Uncertain receipt; no unconditional resend |
| Cancellation marker committed | matrix, `cancel` | Cancelled terminal state; no new model/tool |
| During retry sleep | matrix, `retry` | First recovered model request occurs no earlier than saved deadline |
| Wakeup terminal before journal Ready | `durable_process_wakeup_terminal_before_ready_reuses_saved_answer` | Same stable run ID and saved answer, no second model request |
| Compaction result ready before apply | `durable_process_ready_compaction_preserves_new_suffix` | Saved candidate reused; newly appended suffix retained |
| Repeated crash in one phase | `recovery_five_real_process_crashes_park_without_sixth_attempt` | Parked at configured limit |
| Concurrent process opens same work | matrix `decision` plus `recovery_real_process_kill_preserves_intent_result_and_excludes_second_worker` | Busy owner remains exclusive |
| Failure before commit / error after successful commit | `durable_process_unknown_commit_stops_then_rereads` | Caller stops; recovery distinguishes unknown from saved result and never repeats the unsafe effect |

Existing focused tests additionally cover withdrawn tool availability, stopped
or reset sessions, stale compaction prefixes, overflow retry persistence,
attachment/isolation checks, observer reconnect, and unchanged request-ID payloads.
See `src/agent/recovery_tests.rs`, `compact_jobs.rs`, `core.rs`, `inbox_tests.rs`,
`run_view.rs` and the web tests.

The checkpoint/admission gates are compiled only under `cfg(test)`. Their
`KODRIK_DURABLE_*` variables have no effect in a production binary. The child
fixture is selected by an exact test name; ordinary test execution makes it a
no-op. Tests clean up child processes even when a parent assertion fails.

## Storage checks

`src/agent/storage_recovery_tests.rs` exercises killed legacy import, byte-preserved
JSON, online backup of an open database, restore of that snapshot, closed-database
copy/restore, v1-to-v6 migration, unknown future schema and corrupted bytes.
The private runtime is tested within the library rather than exposing new public
APIs just for a separate integration-test crate. `tests/web_build.rs` is unchanged.

`durable_storage_readonly_and_full_leave_confirmed_checkpoint` produces actual
SQLite READONLY and FULL errors with `query_only` and a pager `max_page_count`
quota. It verifies that no in-memory continuation overwrites the last confirmed
checkpoint. These are deterministic engine faults, not a claim to have filled
the host disk or tested every filesystem's power-loss behavior.

## Reproduce the checks

From the repository root, with loopback and child-process execution permitted:

```sh
rtk npm run build --prefix web
rtk cargo test --locked
rtk cargo clippy --all-targets --locked
rtk npm test --prefix web
rtk cargo fmt --check
rtk git diff --check
```

Focused suites:

```sh
rtk cargo test --locked durable_process
rtk cargo test --locked storage_recovery_tests
rtk cargo test --locked durable_storage_readonly_and_full
rtk cargo test --locked recovery_five_real_process_crashes
```

The manual measurement is ignored during the regular suite:

```sh
rtk cargo test --locked durable_checkpoint_benchmark -- --ignored --nocapture
```

It reports end-to-end checkpoint p50/p95 (including Session cloning, serialization
and SQLite commit), standalone Session serialization p50, database/WAL sizes,
process peak RSS and time from Stop to terminal cancellation while a model waits.
The small workload has eight 512-byte messages, 100 commits. The image workload
has sixteen 512-byte messages and sixteen synthetic 256-KiB base64 image payloads,
40 commits. Image fields are opaque serialization fixtures, not a vision quality
or image-decoding benchmark. History stays fixed while run revisions advance.
RSS is the process high-water mark, including prior work; DB and peak WAL sizes
are measured before closing the store, so small databases may still reside mostly
in WAL. Cancellation is one sample per workload, not a percentile.

Measured on 2026-10-19: Apple M4 Pro, 24 GiB RAM, macOS 27.0.1 arm64,
Rust 1.95.0, **unoptimized test/debug profile**, local temporary SQLite stores.
The final measurement ran after the other test suites finished. These are local
fixture measurements, not release-build or production latency guarantees.

| Metric | Small history | Synthetic image history |
| --- | ---: | ---: |
| Session payload | 4,450 B | 4,204,226 B |
| Commit p50 | 0.289 ms | 89.988 ms |
| Commit p95 | 0.393 ms | 90.925 ms |
| Standalone serialization p50 | 0.101 ms | 87.251 ms |
| DB / peak WAL | 4,096 / 2,204,232 B | 4,325,376 / 4,391,952 B |
| Process peak RSS | 31,899,648 B | 70,139,904 B |
| Stop to terminal (one sample) | 0.772 ms | 103.536 ms |

Full-payload serialization dominates the image workload in this debug measurement.
No append-only or artifact-store migration is included. A separate architecture
decision should first measure a release build with representative histories,
then weigh incremental message storage against its migration and atomicity cost.

Verification for this stage: 456 library + 2 binary + 1 integration tests passed;
4 ignored tests include the manual benchmark, which passed separately. Browser
suite: 11 passed. Frontend build, `cargo fmt --check` and `git diff --check` passed.
Clippy completed with 7 existing warnings, no new warnings. Self-review was used;
there was no independent reviewer.

## Remaining limits

- Local advisory locks coordinate processes sharing this filesystem, not a
  distributed queue or multiple independently copied databases.
- An uncertain external effect requires checking the external system. Retrying
  a parked run does not change a tool's replay policy or undo an earlier effect.
- Unfinished model/summary requests may repeat after process loss. Missing usage
  remains unknown; there is no cumulative pricing ledger or exactly-once billing.
- SQLite stores full Session payloads, including inline images. Large histories
  incur serialization/copying cost at each checkpoint. Changing to append-only
  messages or an artifact store is a separate architecture decision, not part of
  this certification step.
- SQLite durability depends on the OS/filesystem honoring sync. SIGKILL tests do
  not simulate hardware power loss, damaged storage controllers, network-mounted
  filesystems or failures inside Telegram/OpenAI infrastructure.
- Process tests and fake-service tests supplement, rather than replace, deployment
  monitoring, private backups, and external-action reconciliation.
