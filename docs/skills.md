# Skills

Kodrik combines built-in skills with `skills/*/SKILL.md` in the current
[space directory](storage.md#chat-spaces) at the first
message of each new session. This works in CLI, TUI and Telegram. Use `/new`
after adding, removing or changing skill metadata. Existing sessions keep
their saved catalog; older sessions without one load it on their next request.

In isolated chats, skills are available at `/skills` inside Bashkit. Only that
chat's on-disk skills are imported; skills created through the virtual shell stay
in its persistent snapshot. Host skill files act as seeds and overwrite matching
virtual files when importing. Imported files deleted on the host (including
forgotten learned skills) are removed from the virtual disk on the next input;
use `/new` to refresh the saved catalog. Independently created virtual files are
preserved, even inside an imported skill directory. Snapshots made before import
tracking keep untracked files because their origin cannot be inferred safely.
Skill scripts are limited to Bashkit's supported
commands: native programs such as Python, Git and the `self-update` workflow are
unavailable. The catalog states the effective skill root, overriding legacy
`~/.kodrik/skills` paths in bundled instructions.

The `~/.kodrik/skills` examples below describe the legacy local layout; with an
owner configured, use that owner's space directory instead.

## Built-in skills

The `skills/` directory in the source repository is embedded in the binary at
build time, including nested scripts and resources. Cargo watches this directory
for changes. The binary does not need the source directory at runtime.

When loading a session's catalog, Kodrik extracts the bundle into
`~/.kodrik/skills/.system/`. It records a
fingerprint of the embedded paths and contents in
`.system/.kodrik-builtin-skills.marker`. A matching marker skips extraction.
A missing or different marker causes replacement of the entire `.system`
directory, removing outdated files. A file lock serializes installations, and
the new bundle is extracted into a temporary directory before replacing the old
one. The skills directory must be writable; installation errors fail session initialization.

Treat `.system` as a generated cache. Do not put user files there. The marker
tracks the bundled version, not edits or deletions in the extracted files. To
repair the cache, stop Kodrik processes, remove its marker and start a new session.
A symlink or regular file at `.system` is rejected rather than replaced.

A valid user skill with the same name overrides the built-in skill in the
catalog. An invalid user override is warned about and skipped, leaving the
built-in available. Updating built-ins does not overwrite user skills.

Two skills are bundled:

| Skill | Purpose |
| --- | --- |
| `skill-creator` | Create and validate user skills and their supporting files. |
| `self-update` | Fetch upstream changes, build Kodrik and handle a verified service restart. See [Updating and running Kodrik](operations.md). |

For example, ask:

> Use skill-creator to create a skill that writes release notes from a Git diff.

It explains Kodrik's SKILL.md format, user skill location, supporting files,
validation and session refresh requirements. It creates skills through the agent's
existing tools; no separate installation command is needed.

## User skills

The companion can also derive reusable procedures from verified conversations
and save them under descriptive names such as `deploy-docker-compose/SKILL.md`,
with evidence and an undo operation. The reviewer compares the existing catalog
by meaning and reads relevant skills before explicitly choosing to create,
update by lesson ID, or skip saving. Updates preserve the skill's name, path, ID
and source history; name collisions do not implicitly update an existing skill.
Owner edits and untracked skills are protected from automatic replacement.
See [Companion](companion.md) for learning limits and controls.

For example, create `~/.kodrik/skills/code-review/SKILL.md`:

```markdown
---
name: code-review
description: Review code changes for bugs, regressions and missing tests.
---
Read the diff and the affected callers. Report actionable bugs first, with file
paths and line numbers. Explain any gaps in verification. If no bugs are found,
say so explicitly.
```

Ask Kodrik to “use code-review to review these changes”, or let it select a skill
from its description. Only the name, description and absolute file path enter
the initial context. Kodrik reads the instructions with `exec_command` when
needed. It is instructed to reuse the full skill text already available in the
current context, and to read it again if that text is lost after context compaction
or the file changes. Supporting files and scripts are resolved relative to the skill's folder.
Scripts run through the existing tool with the same permissions as Kodrik;
skill metadata does not grant extra permissions.

The YAML header requires a string `name` matching the folder name (1–64 lowercase
ASCII letters, digits or hyphens; no leading, trailing or consecutive hyphens)
and a nonblank string `description` of at most 1024 characters. Quoted and
multiline YAML values are supported. Other metadata fields are ignored, including
`allowed-tools`. The header must be enclosed by lines containing `---`.

A missing `~/.kodrik/skills` directory is created automatically. With no user skills,
the built-in catalog is still available. Invalid skills are skipped with a log
warning; directory I/O errors fail session initialization. To see warnings in CLI/gateway, run
with `RUST_LOG=warn`. The TUI currently has no log display.

Installed user skills stay outside the repository in `~/.kodrik/skills/`. There is
no download installer or catalog reload within an existing session. This implements the basic directory format from the
[Agent Skills specification](https://agentskills.io/specification).

## Vision

Local image inspection and Telegram photos are documented in [Images and vision](vision.md).

See also: [Companion learning](companion.md), [Storage and backups](storage.md).
