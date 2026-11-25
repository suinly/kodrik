# Discover the session history

Resolve the session your human partner named using the tools and information
available in this environment. Your knowledge can suggest where to look; verify
the result against the actual history.

In Codrik, use `session_search` and bounded `context_read` on the identified session, using its generation to detect replaced history. Do not inspect hidden runtime storage. If the user supplied an authorized export, measure files before reading and follow [context-safety.md](context-safety.md). A supplied usable path does not need another search.

Confirm identity using the available session id, working directory, timestamps,
and matching conversation content. Recency alone is not confirmation. Distinguish
the requested session from its children and unrelated candidates. Ask for a
missing identifying fact when the available evidence cannot distinguish them.

For each filesystem source, obtain its full absolute path from the environment,
with home-directory shorthand and variables expanded. Use that same path in the
case record and in the discovery answer you give your human partner.

Establish the record meanings needed for the requested investigation from
observed records or documentation. Distinguish human messages from injected
messages, tool results, and a parent agent's dispatch. Match tool calls to their
results. Establish usage-counter semantics before calculating totals. Do not
infer a format from another harness or turn a missing field into a zero.

Record the exact sources, relevant field meanings, supporting record locations,
associated sessions, rejected plausible candidates, and unresolved information
in the case file. Subsequent readers use that record rather than repeating
discovery. If history is missing, inaccessible, or ambiguous, state the specific
limitation and ask for the missing path, export, or identifying detail.
