# Service context budget

New sessions use compact skill catalogs (one JSON object per line, paths relative
to the declared skills root). Complete descriptions remain available for skill
selection; skill bodies are still read on demand. Native and sandbox catalogs
share the renderer. Remote laptop catalogs retain their own format.

Companion prompt snapshots contain facts and active interest topics only. Research
findings, questions, explanations, IDs and source evidence remain in the journal
and are available through `companion list`. A fact exactly matching a user or
memory entry is omitted from the companion snapshot, not deleted from storage.
Semantic or conflicting facts are not automatically merged.

Snapshots remain frozen: resuming an existing session does not replace its saved
instructions, catalog or memory. Start a new session to receive the smaller
snapshots. This change does not migrate saved conversations or modify user data.

For an anonymized three-turn test session, replaying the snapshot rendering gives:

| Block | Before (characters) | After (characters) |
| --- | ---: | ---: |
| Skills catalog | 12,699 | 9,975 |
| Companion | 7,722 | 2,074 |
| All saved instruction blocks | 37,534 | 29,162 |

This is a 22.3% reduction in saved instruction characters, not a measured token
or cost reduction. Tool schemas and dynamic instructions are additional.

Enable `RUST_LOG=kodrik::agent::llm::openai=debug` to record instruction, tool-schema
and message byte counts and tool count for each request. Contents are never logged
by this diagnostic. Provider usage remains the source of actual token counts;
bytes cannot be converted reliably to tokens. Use fresh comparable sessions for
before/after provider measurements.

Further work should follow these measurements: deferred built-in tool schemas and
task-specific instruction modules need behavior checks for tool discovery, chat,
nutrition, coding, pages and GUI. They are not part of this first reduction.
