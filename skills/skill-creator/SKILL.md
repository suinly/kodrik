---
name: skill-creator
description: Create or update a Kodrik skill when the user asks to add a reusable workflow, write SKILL.md instructions, or improve an existing skill.
---

# Create a Kodrik skill

Create a focused skill for the user's requested workflow. Match the user's
language. Ask for missing information only when it changes what the skill
should do; otherwise use the request and project context.

## Location and existing files

- Put user skills in `~/.kodrik/skills/<name>/SKILL.md`, under the home directory
  of the user running Kodrik. Expand `~` to an absolute path before passing it
  to tools that do not perform shell expansion.
- Inspect an existing skill before changing it. Preserve unrelated instructions
  and resources. Do not overwrite a same-name skill without checking its purpose.
- `~/.kodrik/skills/.system/` is a generated cache for built-in skills. Never create
  or edit user skills there: updates can replace its contents. To customize a
  built-in, create a user skill with the same name in `~/.kodrik/skills/<name>/`;
  a valid user skill takes precedence.

## Required format

The file starts with YAML frontmatter between two lines containing `---`, then
Markdown instructions. Required fields:

- `name`: a string matching the containing directory; 1–64 lowercase ASCII
  letters, digits or hyphens; no leading, trailing or consecutive hyphens.
- `description`: a nonblank string of at most 1024 characters explaining what
  the skill does and when to use it. The model sees this before reading the body.

Quote YAML strings containing punctuation such as `: ` or `#`. Use a YAML block
scalar for multiline descriptions. Kodrik currently ignores other metadata;
`allowed-tools` does not change permissions, and `agents/openai.yaml` is not used.

```markdown
---
name: release-notes
description: Draft release notes from a Git diff when the user asks to summarize changes for a release.
---

Read the requested diff. Group user-visible changes into features, fixes and
breaking changes; omit empty groups. Explain migration steps only when supported
by the diff. Do not invent issue links or publish the notes unless requested.
```

## Write useful instructions

State the expected result, the inputs to inspect, the necessary steps and any
real constraints. Add only guidance that helps perform this workflow. Preserve
the user's scope and authorization; a skill does not grant extra permissions.

Prefer a single SKILL.md. Add `references/`, `scripts/` or `assets/` only when the
task needs them. Link supporting files relative to the skill directory and say
when to read or run them. Run scripts with their interpreter, for example
`python3 scripts/check.py` from the skill directory. Do not reference tools or
dependencies that have not been verified as available.

## Verify and hand off

Read the resulting files back. Check the YAML field types, name and directory
match, description limits, relative paths and absence of unfinished placeholders.
Run any added scripts with representative inputs. Check that the description
matches a realistic user request and does not attract unrelated tasks.

Report the created path and one example prompt. Explain that Kodrik discovers
skills at the first message of a new session: use `/new` after adding a skill
or changing its metadata. Existing sessions keep their saved catalog.
Distinguish file validation from a real model invocation; do not claim the
workflow was tested unless it actually ran.
