# Verification — 2026-11-13

This is a chronological verification log. Later sections supersede earlier UI
behavior and test counts; early TXT/disclosure checks describe intermediate versions.

Passed: frontend static build, Nuxt typecheck, cargo fmt --check, git diff --check,
9 extension/bridge tests, Rust `only_chat_html_allows_extension_framing` test.
Real MV3 loaded in headless Chromium: authenticated iframe, text attachment content,
fresh recapture and new-conversation bridge reconnection. Narrow (420px) UI screenshot
visually inspected. Native Chrome side-panel placement/permission dialogs not automated.

Full cargo test: 1304 passed, 11 failed, 12 ignored. Failures all in
`commands::tui::remote_tests` (unmodified TUI), terminal expectations mismatch localized
output (e.g. expected `Stopped`, received `Остановлено`):
- rejected_remote_edit_preserves_editor_and_original_draft
- remote_copy_uses_laptop_terminal_clipboard_without_server_command
- remote_local_controls_never_become_model_input
- remote_pty_loads_server_history_sends_steers_stops_and_exits_without_provider
- remote_session_picker_and_version_navigation_use_server_commands
- remote_sessions_error_is_retried_inside_the_picker
- remote_sessions_later_page_failure_keeps_rows_and_retries_same_offset
- remote_sessions_loading_remains_cancellable_and_late_results_stay_closed
- remote_sessions_paginates_near_end_and_selects_later_page
- remote_start_attaches_and_switch_transfers_binding_without_confirmation
- successful_remote_edit_selects_server_branch_and_transfers_access

Full web suite attempted twice; exceeded 200-second command timeout and was stopped.
Both attempts reported these failures (Russian UI locators/timeouts); they have not
been established as pre-existing, nor fixed as part of this change:
- pages catalog searches published titles and addresses without losing the conversation draft
- sidebar lists update automatically while preserving searches and drafts
- section rail switches context menus and main content without losing chat drafts
- composer focus recovery respects other fields, shortcuts, overlays and hidden chat
- conversation menus appear on hover or focus and pins survive reload

At the initial verification stage: no deployment, gateway restart, publication or
Chrome profile modifications. The initial extension was later committed as `cf397ba`;
subsequent sections describe the follow-up changes.

## 2026-11-13: tab-specific panels and compact design

- Frontend build, Nuxt typecheck and diff check passed.
- 11 targeted tests passed, including tab-specific action setup/cleanup and real
  Chromium send, fresh capture, new conversation, reload restoring its attachment,
  and another tab starting without that conversation.
- Native Helium checked after reloading the unpacked extension: open on GitHub,
  switch to xlm.ru (hidden), return (visible), close, switch away and return
  (still closed). Found and fixed Helium user-gesture loss by preparing tab panel
  options before clicks; action handler now opens synchronously.
- Updated chat UI inspected in 420x850 Chromium screenshot. Live server was not
  rebuilt/restarted, so its embedded chat still uses deployed frontend assets.
- Full web suite again exceeded 200 seconds. Failures observed: pages catalog
  searches published titles and addresses without losing the conversation draft;
  sidebar lists update automatically while preserving searches and drafts;
  section rail switches context menus and main content without losing chat drafts;
  composer focus recovery respects other fields, shortcuts, overlays and hidden
  chat; conversation menus appear on hover or focus and pins survive reload.
  No full-suite green claim. Full Rust suite not rerun (no Rust changes).

## Return-to-panel stale stream regression

A browser test now holds the SSE connection silent, completes the canonical
snapshot while hidden, then restores document visibility/focus. Before the fix,
the final assistant message never appeared. After the fix the chat fetches current
state and restarts its observer, aborting the previous one; no send is repeated.
Regression test, 11 extension/bridge tests, frontend build, typecheck and diff check
passed. This is a reproduced failure mode, not proof of the precise transport
failure in the user's live Helium. Fix is not deployed. An additional existing test
`long messages stay fully visible through completion, reconnect and reopening the
conversation` timed out at line 1586 waiting for a conversation-list item; its
stream/reconnect assertions before that point passed. Full suite not rerun for
this fix; prior full-suite failures remain unresolved.

## Structured browser context

Browser page snapshots now travel as `browser_context`, not uploaded TXT files.
Verified locally:
- Extension/bridge suites: 11/11 passed, including authenticated Chromium E2E, fresh context on second send, new conversation and reload restoration without `browser-tab.txt`.
- UI regression: disclosure shows literal (escaped) page title/content and truncation metadata without executing injected HTML.
- Rust focused `browser_context` tests: 5/5 passed (validation, history/idempotency, multipart with real file, queue/edit preservation, model representation and token budget).
- Nuxt build/typecheck, cargo check, cargo fmt --check, git diff --check passed.
- Narrow 420×850 screenshot inspected: compact collapsed context chip, no TXT attachment.
Full web suite again exceeded 200 seconds; at least four existing previously observed tests failed (pages catalog, sidebar refresh, section rail, composer focus). No full-suite green claim.
Not deployed to the running server; unpacked extension reload and updated server assets/binary are both required. Existing TXT attachments in old history are unchanged.

## Compact sidebar layout

- Removed per-message context disclosure; page data still travels and persists separately.
- One-row toolbar without conversation title; extension settings directly left of search opens native connection dialog.
- Context toggle above composer; explicit expansion reads current page locally and opens upward. Expansion does not submit a chat message.
- Serial extension/bridge suite: 15/15 passed, including dialog, toolbar geometry, fresh preview and context send. Independent hidden-message-history test passed. Nuxt build/typecheck and diff check passed. Screenshot inspected at 420×850.
- Parallel browser run exposed intermittent test clicks/races; serial full extension run is green. Existing full web suite limitations above remain; not redeployed.

## Pre-commit documentation refresh

README now describes the final toolbar/modal/composer layout, explicit local
preview reads, structured context privacy, manual image/PDF uploads, unsupported
automatic screenshot/PDF capture, per-tab lifetime and server/extension update steps.
Fresh checks: 15/15 serial extension/bridge tests, 5/5 Rust browser-context tests,
Nuxt typecheck, cargo fmt --check and git diff --check passed. Full suites were not
rerun for this documentation refresh; unresolved full-suite failures above remain.
