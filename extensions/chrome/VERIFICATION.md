# Verification — 2026-11-13

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

No deployment, gateway restart, commits, publication or Chrome profile modifications.
