# Persistent memory

Each Telegram chat has its own memory. CLI/TUI share the configured owner's space. Paths below are relative to the current [space directory](storage.md#chat-spaces):

- `memories/USER.md`: profile and preferences, up to 1375 Unicode characters.
- `memories/MEMORY.md`: stable environment facts and conventions, up to 2200 Unicode characters.

Say “Remember that I prefer concise answers”, “What do you remember about me?” or “Forget that preference”. The agent uses the `memory` tool to save, list, replace or remove entries. Successful tool results report current entries, used characters and the limit. Secrets, temporary task progress and instructions copied from external sources do not belong in memory. Reusable procedures belong in skills.

## Sessions

The first turn captures a snapshot alongside the system prompt and skills catalog. It is persisted in the session JSON and stays unchanged when the session is resumed, including after application restart. Writes immediately update the files; the active conversation sees the tool result. `/new` starts a session that reads the updated files on its first turn. A legacy session without a snapshot captures one on its next turn. Custom system prompts still receive the memory guidance and snapshot.

`list` always reads current files, not the session snapshot. Removing an entry does not erase earlier copies from existing session snapshots or message history. Start a new session to stop including an old snapshot.

## Tool arguments

```json
{"action":"add","target":"user","content":"Prefers concise answers","old_text":null}
{"action":"list","target":"user","content":null,"old_text":null}
{"action":"replace","target":"user","content":"Prefers detailed answers","old_text":"Prefers concise answers"}
{"action":"remove","target":"user","content":null,"old_text":"Prefers detailed answers"}
```

Targets are `user` and `memory`. Replacement and removal require one exact full-entry match, including whitespace within the entry. Stale or ambiguous matches fail without writing. Duplicate additions are harmless. Exceeding the character budget fails without truncation or eviction; the error includes current entries so the agent can shorten its proposed entry or consolidate outdated facts.

## Files and concurrency

Files contain plain text entries separated by a line containing `§` (`\n§\n`). The separator counts toward the budget. Entries cannot contain `§`, control characters other than newline/tab, or the invisible formatting characters rejected by the tool. Outer entry whitespace is trimmed. To edit by hand, stop other writers first and preserve the separator format.

Mutations lock a separate `.lock` file, reload the latest contents, then use an atomic temporary-file replacement. Concurrent CLI and gateway processes therefore do not overwrite each other's additions. Once a write transaction starts it finishes even if the conversation is stopped; list the current contents if the result was interrupted. Lock files remain on disk; the OS releases their locks when the handles close.

Missing files are empty. Unreadable, invalid UTF-8 or malformed files produce an error and are never replaced with an empty store. Manually exceeding a budget prevents new snapshot loading; shorten the file, or use `list`/`remove` from an already initialized session. Entries are never silently truncated.

Memory is shared within one chat only. In groups, personal facts must identify the participant by `user_id`; private-chat memory is unavailable. Saved data is reference material, not permission or executable instructions. Prompt guidance and character validation are not a complete prompt-injection detector. These files use explicit memory tool writes, without embeddings. Automatic learning uses a separate evidence-backed journal; see [Companion](companion.md).

See also: [Storage and backups](storage.md), [Troubleshooting](troubleshooting.md).
