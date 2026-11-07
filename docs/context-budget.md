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

## Deferred built-in schemas

Foreground agents now load heavy built-in schemas through the existing
`mcp_search` mechanism. Its description lists exact built-in names and purposes.
Record/page, companion, wakeup/webhook, messaging/media and encyclopedia schemas
are deferred. Shell, patch, image viewing, memory, session search, planning,
web research and child-agent controls remain directly advertised.

Deferral affects model advertisement only, not authorization, auditing or replay.
Removed tools are also removed from discovery; owner-only tools remain absent in
isolated/invited registries. Recent selections are reconstructed from paired
successful search calls/results in session history, bounded to 32 tools. A
compacted-away selection can be searched again. Remote scope discovery uses the
same catalog and still checks live lease availability.

With the same copied local profile and identical one-shot prompt, provider input
usage changed from 12,248 to 7,988 tokens (34.8% lower, neither request cached).
Tool schema bytes fell from 41,796 to 14,947 (64.2% lower); initial tool count fell
from 25 to 14. The small instruction change is included in this comparison.
A separate read-only live check successfully searched for `datasets`, loaded its
schema on the following request and listed an empty test store. Discovery adds
an extra model round trip when a deferred tool is first needed, so this does not
promise a 34.8% saving on tool-heavy turns.
