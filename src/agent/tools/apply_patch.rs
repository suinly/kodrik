use crate::agent::tools::{Tool, ToolOutput};
use anyhow::{Context, bail, ensure};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::{BTreeMap, btree_map::Entry},
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

const PATCH_FORMAT_HELP: &str = "Use Codex patch format, not a unified diff. Replace ---/+++ file headers with *** Update File: path. Use @@ or @@ context without line numbers. Prefix hunk lines with a space, +, or -. Include actual lines, never ... placeholders.\nExample:\n*** Begin Patch\n*** Update File: example.txt\n@@\n-before\n+after\n*** End Patch";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyPatchArgs {
    /// Codex patch text enclosed in *** Begin Patch / *** End Patch.
    /// Use *** Add/Update/Delete File: path; do not use ---/+++ headers or numbered @@ hunks.
    pub(crate) patch: String,
    /// Allowed workspace directory. Null uses Kodrik's current directory.
    pub(crate) workdir: Option<String>,
}

#[derive(Default)]
pub struct ApplyPatchTool;

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> String {
        "apply_patch".into()
    }

    fn description(&self) -> String {
        format!(
            "Edit files inside workdir using *** Add File: path (+ lines), *** Delete File: path, or *** Update File: path (optional *** Move to: path and *** End of File). Checks the entire patch before writing. Returns changed_files with path, action, added and removed line counts. Writes are atomic per file; an I/O failure reports files already changed. Paths must stay inside workdir; file symlinks are rejected.\n{PATCH_FORMAT_HELP}"
        )
    }

    fn parameters(&self) -> serde_json::Value {
        let mut schema = schema_for!(ApplyPatchArgs);
        schema.insert("required".into(), serde_json::json!(["patch", "workdir"]));
        schema.to_value()
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let args: ApplyPatchArgs = serde_json::from_value(properties)?;
        crate::agent::run_state::spawn_owned_blocking(move || {
            let root = fs::canonicalize(args.workdir.as_deref().unwrap_or("."))
                .context("Cannot resolve workdir")?;
            ensure!(root.is_dir(), "workdir must be a directory");
            let operations = parse_patch(&args.patch)
                .map_err(|error| anyhow::anyhow!("{error:#}\n{PATCH_FORMAT_HELP}"))?;
            let changes = prepare(&root, operations)?;
            commit(&root, changes)
        })
        .await?
        .map(Into::into)
    }
}

pub(crate) enum Operation {
    Add(String, String),
    Delete(String),
    Update(String, Option<String>, Vec<Chunk>),
}

#[derive(Default)]
pub(crate) struct Chunk {
    context: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    added: usize,
    removed: usize,
    eof: bool,
}

pub(crate) fn parse_patch(patch: &str) -> anyhow::Result<Vec<Operation>> {
    let lines: Vec<_> = patch.trim().lines().collect();
    ensure!(
        lines.len() >= 2
            && lines[0].trim() == "*** Begin Patch"
            && lines[lines.len() - 1].trim() == "*** End Patch",
        "Patch must start with *** Begin Patch and end with *** End Patch"
    );
    let end = lines.len() - 1;
    let mut index = 1;
    let mut operations = Vec::new();
    while index < end {
        let header = lines[index];
        index += 1;
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while index < end && lines[index].starts_with('+') {
                content.push_str(&lines[index][1..]);
                content.push('\n');
                index += 1;
            }
            operations.push(Operation::Add(path.into(), content));
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete(path.into()));
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let move_to = lines
                .get(index)
                .and_then(|line| line.strip_prefix("*** Move to: "));
            if move_to.is_some() {
                index += 1;
            }
            let mut chunks = Vec::new();
            while index < end && !lines[index].starts_with("*** ") {
                let mut chunk = Chunk::default();
                if lines[index] == "@@" {
                    index += 1;
                } else if let Some(context) = lines[index].strip_prefix("@@ ") {
                    chunk.context = Some(context.into());
                    index += 1;
                } else {
                    ensure!(chunks.is_empty(), "Expected @@ at patch line {}", index + 1);
                }
                let start = index;
                while index < end
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    let line = lines[index];
                    if let Some(line) = line.strip_prefix('+') {
                        chunk.new.push(line.into());
                        chunk.added += 1;
                    } else if let Some(line) = line.strip_prefix('-') {
                        chunk.old.push(line.into());
                        chunk.removed += 1;
                    } else if let Some(line) = line
                        .strip_prefix(' ')
                        .or_else(|| line.is_empty().then_some(""))
                    {
                        chunk.old.push(line.into());
                        chunk.new.push(line.into());
                    } else {
                        bail!("Invalid change at patch line {}: {line}", index + 1);
                    }
                    index += 1;
                }
                ensure!(index > start, "Empty change at patch line {}", start + 1);
                if index < end && lines[index] == "*** End of File" {
                    chunk.eof = true;
                    index += 1;
                    ensure!(
                        index == end || lines[index].starts_with("*** "),
                        "Changes after *** End of File"
                    );
                }
                chunks.push(chunk);
            }
            ensure!(
                !chunks.is_empty() || move_to.is_some(),
                "Empty update for {path}"
            );
            operations.push(Operation::Update(
                path.into(),
                move_to.map(String::from),
                chunks,
            ));
        } else {
            bail!("Invalid operation at patch line {}: {header}", index);
        }
    }
    Ok(operations)
}

fn normalized(line: &str, level: u8) -> Cow<'_, str> {
    match level {
        0 => Cow::Borrowed(line),
        1 => Cow::Borrowed(line.trim_end()),
        2 => Cow::Borrowed(line.trim()),
        _ => Cow::Owned(
            line.trim()
                .chars()
                .map(|ch| match ch {
                    '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                    '\u{2018}'..='\u{201b}' => '\'',
                    '\u{201c}'..='\u{201f}' => '"',
                    '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => {
                        ' '
                    }
                    _ => ch,
                })
                .collect(),
        ),
    }
}

fn find_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    let last = lines.len().checked_sub(pattern.len())?;
    let first = if eof { last.max(start) } else { start };
    // ponytail: linear context search; index lines if large files make matching slow.
    (0..4).find_map(|level| {
        (first..=last).find(|&index| {
            lines[index..index + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(line, expected)| normalized(line, level) == normalized(expected, level))
        })
    })
}

pub(crate) fn update(content: &[u8], chunks: Vec<Chunk>) -> anyhow::Result<Vec<u8>> {
    let content = std::str::from_utf8(content).context("Cannot update a non-UTF-8 file")?;
    if chunks.is_empty() {
        return Ok(content.as_bytes().to_vec());
    }
    let lines: Vec<_> = content.lines().map(String::from).collect();
    let mut cursor = 0;
    let mut output = Vec::new();
    for mut chunk in chunks {
        let mut start = cursor;
        if let Some(context) = &chunk.context {
            start = find_sequence(&lines, std::slice::from_ref(context), cursor, false)
                .with_context(|| format!("Cannot find context: {context}"))?
                + 1;
        }
        let index = if chunk.old.is_empty() {
            lines.len()
        } else {
            let found = find_sequence(&lines, &chunk.old, start, chunk.eof);
            if found.is_none() && chunk.old.last().is_some_and(String::is_empty) {
                chunk.old.pop();
                if chunk.new.last().is_some_and(String::is_empty) {
                    chunk.new.pop();
                }
                find_sequence(&lines, &chunk.old, start, chunk.eof)
            } else {
                found
            }
            .with_context(|| format!("Cannot find expected lines:\n{}", chunk.old.join("\n")))?
        };
        ensure!(index >= cursor, "Overlapping or out-of-order changes");
        output.extend_from_slice(&lines[cursor..index]);
        output.extend(chunk.new);
        cursor = index + chunk.old.len();
    }
    output.extend_from_slice(&lines[cursor..]);
    let mut content = output.join("\n");
    if !output.is_empty() {
        content.push('\n');
    }
    Ok(content.into_bytes())
}

fn resolve_path(root: &Path, path: &str) -> anyhow::Result<PathBuf> {
    ensure!(!path.is_empty(), "Empty file path");
    let path = Path::new(path);
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "Parent traversal is not allowed: {}",
        path.display()
    );
    let candidate = root.join(path);
    let mut ancestor = candidate.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                ensure!(
                    ancestor != candidate || !metadata.file_type().is_symlink(),
                    "File symlinks are not allowed: {}",
                    candidate.display()
                );
                break;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(ancestor.file_name().context("Invalid file path")?);
                ancestor = ancestor.parent().context("Invalid file path")?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Cannot resolve {}", candidate.display()));
            }
        }
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    ensure!(
        resolved.starts_with(root),
        "Path is outside workdir: {}",
        candidate.display()
    );
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    ensure!(
        resolved != root,
        "A patch path must name a file, not workdir"
    );
    Ok(resolved)
}

struct Change {
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    permissions: Option<fs::Permissions>,
    added: usize,
    removed: usize,
}

fn read_file(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
    };
    ensure!(
        file.metadata()?.is_file(),
        "Not a regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    Ok(Some(bytes))
}

fn load(changes: &mut BTreeMap<PathBuf, Change>, path: PathBuf) -> anyhow::Result<&mut Change> {
    Ok(match changes.entry(path) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let before = read_file(entry.key())?;
            let permissions = if before.is_some() {
                Some(fs::metadata(entry.key())?.permissions())
            } else {
                None
            };
            entry.insert(Change {
                after: before.clone(),
                before,
                permissions,
                added: 0,
                removed: 0,
            })
        }
    })
}

fn prepare(root: &Path, operations: Vec<Operation>) -> anyhow::Result<BTreeMap<PathBuf, Change>> {
    let mut changes = BTreeMap::new();
    for operation in operations {
        match operation {
            Operation::Add(path, content) => {
                let change = load(&mut changes, resolve_path(root, &path)?)?;
                change.after = Some(content.into_bytes());
                change.added = line_count(change.after.as_deref());
                change.removed = line_count(change.before.as_deref());
            }
            Operation::Delete(path) => {
                let change = load(&mut changes, resolve_path(root, &path)?)?;
                ensure!(change.after.is_some(), "Cannot delete missing file: {path}");
                change.after = None;
                change.added = 0;
                change.removed = line_count(change.before.as_deref());
            }
            Operation::Update(path, move_to, chunks) => {
                let added: usize = chunks.iter().map(|chunk| chunk.added).sum();
                let removed: usize = chunks.iter().map(|chunk| chunk.removed).sum();
                let source = resolve_path(root, &path)?;
                let change = load(&mut changes, source.clone())?;
                let content = change
                    .after
                    .as_deref()
                    .with_context(|| format!("Cannot update missing file: {path}"))?;
                let new =
                    update(content, chunks).with_context(|| format!("Cannot update {path}"))?;
                let permissions = change.permissions.clone();
                let destination = match move_to {
                    Some(path) => resolve_path(root, &path)?,
                    None => source.clone(),
                };
                let moved = destination != source;
                if moved {
                    let original = load(&mut changes, source)?;
                    original.after = None;
                    original.added = 0;
                    original.removed = line_count(original.before.as_deref());
                }
                let target = load(&mut changes, destination)?;
                target.after = Some(new);
                if target.before.is_none() || moved {
                    target.added = line_count(target.after.as_deref());
                    target.removed = line_count(target.before.as_deref());
                } else {
                    target.added += added;
                    target.removed += removed;
                }
                if target.permissions.is_none() {
                    target.permissions = permissions;
                }
            }
        }
    }
    for path in changes.keys() {
        for ancestor in path.ancestors().skip(1) {
            ensure!(
                changes
                    .get(ancestor)
                    .is_none_or(|change| change.after.is_none()),
                "File is used as a parent directory: {}",
                ancestor.display()
            );
        }
    }
    Ok(changes)
}

fn commit(root: &Path, changes: BTreeMap<PathBuf, Change>) -> anyhow::Result<serde_json::Value> {
    for (path, change) in &changes {
        ensure!(
            resolve_path(root, &path.to_string_lossy())? == *path,
            "Path changed during preparation: {}",
            path.display()
        );
        ensure!(
            read_file(path)? == change.before,
            "File changed during preparation: {}",
            path.display()
        );
    }
    let mut applied = Vec::new();
    // A failed destination write must leave the source of a move intact.
    let (writes, deletes): (Vec<_>, Vec<_>) = changes
        .into_iter()
        .partition(|(_, change)| change.after.is_some());
    for (path, change) in writes.into_iter().chain(deletes) {
        if change.before == change.after {
            continue;
        }
        let result = (|| -> anyhow::Result<()> {
            ensure!(
                resolve_path(root, &path.to_string_lossy())? == path,
                "Path changed before write"
            );
            ensure!(
                read_file(&path)? == change.before,
                "File changed before write"
            );
            if let Some(content) = &change.after {
                let parent = path.parent().context("File has no parent directory")?;
                fs::create_dir_all(parent)?;
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(content)?;
                if let Some(permissions) = change.permissions {
                    file.as_file().set_permissions(permissions)?;
                }
                file.persist(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
            Ok(())
        })();
        result.with_context(|| {
            format!(
                "Cannot apply change to {}; already changed files: {}",
                path.display(),
                serde_json::Value::Array(applied.clone())
            )
        })?;
        let action = if change.after.is_none() {
            "deleted"
        } else if change.before.is_none() {
            "added"
        } else {
            "updated"
        };
        applied.push(serde_json::json!({"path": path.strip_prefix(root)?.to_string_lossy(), "action": action, "added": change.added, "removed": change.removed}));
    }
    Ok(serde_json::json!({"changed_files": applied}))
}

fn line_count(content: Option<&[u8]>) -> usize {
    content.map_or(0, |content| {
        content.iter().filter(|&&byte| byte == b'\n').count()
            + usize::from(!content.is_empty() && !content.ends_with(b"\n"))
    })
}

#[cfg(test)]
mod tests {
    use crate::agent::tools::{ApplyPatchTool, Tool};
    use serde_json::{Value, json};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    async fn run(root: &Path, patch: &str) -> anyhow::Result<Value> {
        ApplyPatchTool
            .execute(json!({"patch": patch, "workdir": root}))
            .await
            .map(|result| result.output)
    }

    #[tokio::test]
    async fn counts_reordered_lines_as_patch_changes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file"), "first\nsecond\n").unwrap();
        let output = run(
            directory.path(),
            "*** Begin Patch\n*** Update File: file\n@@\n-first\n second\n+first\n*** End Patch",
        )
        .await
        .unwrap();
        assert_eq!(output["changed_files"][0]["added"], 1);
        assert_eq!(output["changed_files"][0]["removed"], 1);
    }

    #[tokio::test]
    async fn applies_add_update_move_and_delete() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("old.rs"),
            "fn first() {\n    old();\n}\nfn second() {\n    old();\n}\n",
        )
        .unwrap();
        fs::set_permissions(root.join("old.rs"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("remove.txt"), "remove me").unwrap();
        let output = run(
            root,
            concat!(
                "*** Begin Patch\n",
                "*** Add File: nested/new.txt\n+hello\n+world\n",
                "*** Update File: old.rs\n*** Move to: nested/new.rs\n",
                "@@ fn second() {\n-    old();\n+    new();\n",
                "@@\n }\n+// done\n*** End of File\n",
                "*** Delete File: remove.txt\n*** End Patch\n",
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("nested/new.txt")).unwrap(),
            "hello\nworld\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("nested/new.rs")).unwrap(),
            "fn first() {\n    old();\n}\nfn second() {\n    new();\n}\n// done\n"
        );
        assert_eq!(
            fs::metadata(root.join("nested/new.rs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(!root.join("old.rs").exists());
        assert!(!root.join("remove.txt").exists());
        assert_eq!(
            output["changed_files"],
            json!([
                {"path": "nested/new.rs", "action": "added", "added": 7, "removed": 0},
                {"path": "nested/new.txt", "action": "added", "added": 2, "removed": 0},
                {"path": "old.rs", "action": "deleted", "added": 0, "removed": 6},
                {"path": "remove.txt", "action": "deleted", "added": 0, "removed": 1},
            ])
        );
    }

    #[tokio::test]
    async fn matches_context_and_handles_file_endings() {
        let directory = tempfile::tempdir().unwrap();
        for (before, diff, after) in [
            (
                "header\nvalue   \ntail\n",
                "@@\n-value\n+changed\n",
                "header\nchanged\ntail\n",
            ),
            (
                "“hello”—world\n",
                "@@\n-\"hello\"-world\n+plain\n",
                "plain\n",
            ),
            (
                "same\nsame\n",
                "@@\n-same\n+last\n*** End of File\n",
                "same\nlast\n",
            ),
            ("a\nb", "@@\n-a\n+A\n@@\n-b\n+B\n", "A\nB\n"),
            ("", "@@\n+added\n", "added\n"),
            ("one\n\n", "@@\n-one\n+two\n", "two\n\n"),
            ("a\r\nb\r\n", "@@\n-a\n+A\n", "A\nb\n"),
        ] {
            fs::write(directory.path().join("file"), before).unwrap();
            let patch = format!("*** Begin Patch\n*** Update File: file\n{diff}*** End Patch\n");
            run(directory.path(), &patch).await.unwrap();
            assert_eq!(
                fs::read_to_string(directory.path().join("file")).unwrap(),
                after
            );
        }
    }

    #[tokio::test]
    async fn validates_entire_patch_before_changing_files() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file"), "original\n").unwrap();
        for invalid in [
            "*** Unexpected: file\n*** End Patch",
            "*** Update File: file\n@@\n-missing\n+changed\n*** End Patch",
            "*** Update File: file\n@@\n-original\n-missing\n+changed\n*** End Patch",
            "*** Update File: file\n*** End Patch",
            "*** Delete File: missing\n*** End Patch",
            "*** Update File: file\n@@\n-original\n+changed\n",
        ] {
            let patch = format!("*** Begin Patch\n*** Add File: created\n+new\n{invalid}");
            assert!(run(directory.path(), &patch).await.is_err());
            assert!(!directory.path().join("created").exists());
            assert_eq!(
                fs::read_to_string(directory.path().join("file")).unwrap(),
                "original\n"
            );
        }
    }

    #[tokio::test]
    async fn unified_diff_error_provides_a_working_codex_example() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("Cargo.toml"), "version = \"0.1.0\"\n").unwrap();
        fs::write(directory.path().join("example.txt"), "before\n").unwrap();
        let error = run(
            directory.path(),
            concat!(
                "*** Begin Patch\n--- a/Cargo.toml\n+++ b/Cargo.toml\n",
                "@@ -1 +1 @@\n-version = \"0.1.0\"\n+version = \"0.1.1\"\n*** End Patch",
            ),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            fs::read_to_string(directory.path().join("Cargo.toml")).unwrap(),
            "version = \"0.1.0\"\n"
        );
        let (_, example) = error
            .split_once("Example:\n")
            .expect("error should explain the supported patch format");
        run(directory.path(), example).await.unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("example.txt")).unwrap(),
            "after\n"
        );
    }

    #[tokio::test]
    async fn rejects_paths_and_symlinks_outside_workdir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, "untouched\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("file-link")).unwrap();
        std::os::unix::fs::symlink(directory.path(), root.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(directory.path().join("missing"), root.join("dangling"))
            .unwrap();
        for path in [
            "../outside".to_string(),
            outside.to_string_lossy().into_owned(),
            "file-link".to_string(),
            "dir-link/outside".to_string(),
            "dir-link/new".to_string(),
            "dangling".to_string(),
        ] {
            let patch = format!(
                "*** Begin Patch\n*** Add File: created\n+safe\n*** Add File: {path}\n+bad\n*** End Patch"
            );
            assert!(run(&root, &patch).await.is_err(), "accepted {path}");
            assert!(!root.join("created").exists());
            assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched\n");
        }
        assert!(!directory.path().join("new").exists());
    }

    #[tokio::test]
    async fn stages_repeated_operations_and_accepts_absolute_paths_inside_workdir() {
        let directory = tempfile::tempdir().unwrap();
        let absolute = directory.path().join("file");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+first\n*** Update File: file\n@@\n-first\n+second\n*** Add File: empty\n*** End Patch",
            absolute.display(),
        );
        run(directory.path(), &patch).await.unwrap();
        assert_eq!(fs::read_to_string(&absolute).unwrap(), "second\n");
        assert_eq!(fs::read(directory.path().join("empty")).unwrap(), b"");
    }

    #[tokio::test]
    async fn failed_move_keeps_source_file() {
        // SAFETY: geteuid has no arguments; root bypasses directory permissions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a"), "keep me\n").unwrap();
        fs::create_dir(root.join("z")).unwrap();
        fs::set_permissions(root.join("z"), fs::Permissions::from_mode(0o555)).unwrap();
        let result = run(
            root,
            "*** Begin Patch\n*** Update File: a\n*** Move to: z/new\n*** End Patch",
        )
        .await;
        fs::set_permissions(root.join("z"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "keep me\n");
        assert!(!root.join("z/new").exists());
    }

    #[tokio::test]
    async fn rejects_fifo_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is NUL-terminated and remains alive throughout the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run(
                directory.path(),
                "*** Begin Patch\n*** Update File: fifo\n@@\n-missing\n+new\n*** End Patch",
            ),
        )
        .await;
        if result.is_err() {
            // Unblock a regressed reader so runtime shutdown does not hang the test.
            // SAFETY: name is valid; close is used only on a successfully opened fd.
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        assert!(
            result
                .expect("FIFO must be rejected without blocking")
                .is_err()
        );
    }
}
