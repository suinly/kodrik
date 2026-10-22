use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, ensure};
use bashkit::{Bash, ExecutionLimits, FileSystem, FsLimits, InMemoryFs, VfsSnapshot};
use serde_json::json;

use super::{
    Image,
    tools::{
        ApplyPatchTool, ExecCommandTool, Tool, ToolOutput, ViewImageTool,
        apply_patch::{self, ApplyPatchArgs, Operation},
        exec_command::ExecCommandArgs,
        view_image::ViewImageArgs,
    },
};

const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) struct Sandbox {
    directory: PathBuf,
}

struct Transaction {
    fs: Arc<InMemoryFs>,
    imported_skills: BTreeSet<PathBuf>,
    directory: PathBuf,
    // Cross-process lock survives detached atomic saves after cancellation.
    _lock: std::fs::File,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    #[serde(flatten)]
    fs: VfsSnapshot,
    // shortcut: old snapshots retain unknown-origin files; clean those explicitly if upgrading one.
    #[serde(default)]
    imported_skills: BTreeSet<PathBuf>,
}

impl Sandbox {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) async fn read_file(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let transaction = self.open().await?;
        transaction
            .fs
            .read_file(&virtual_path(path)?)
            .await
            .map_err(Into::into)
    }

    pub(crate) fn tool(self: &Arc<Self>, name: &'static str) -> Arc<dyn Tool> {
        Arc::new(SandboxTool {
            sandbox: self.clone(),
            name,
        })
    }

    pub(crate) async fn prepare_input(
        &self,
        input: &mut super::UserMessage,
    ) -> anyhow::Result<String> {
        let mut transaction = self.open().await?;
        // Only this space's skill files are imported; VFS-created skills stay in its snapshot.
        let skills = self.directory.join("skills");
        let files =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(PathBuf, Vec<u8>)>> {
                super::skills::load_catalog(&skills)?;
                let mut files = Vec::new();
                collect_skills(&skills, &skills, &mut files, &mut 0)?;
                Ok(files)
            })
            .await??;
        let imported_skills: BTreeSet<_> = files
            .iter()
            .map(|(relative, _)| Path::new("/skills").join(relative))
            .collect();
        let obsolete: BTreeSet<_> = transaction
            .imported_skills
            .difference(&imported_skills)
            .collect();
        // Remove only tracked files, never entire directories containing virtual-only work.
        for entry in transaction.fs.snapshot().entries() {
            if obsolete.contains(&entry.path)
                && !matches!(entry.kind, bashkit::VfsEntryKind::Directory)
            {
                transaction.fs.remove(&entry.path, false).await?;
            }
        }
        let directories: BTreeSet<_> = transaction
            .fs
            .snapshot()
            .entries()
            .iter()
            .filter(|entry| {
                matches!(entry.kind, bashkit::VfsEntryKind::Directory)
                    && entry.path.starts_with("/skills")
                    && entry.path != Path::new("/skills")
                    && obsolete.iter().any(|path| path.starts_with(&entry.path))
            })
            .map(|entry| entry.path.clone())
            .collect();
        // Descendants sort after their ancestors; prune only empty directories, deepest first.
        for directory in directories.iter().rev() {
            if transaction.fs.read_dir(directory).await?.is_empty() {
                transaction.fs.remove(directory, false).await?;
            }
        }
        for (relative, bytes) in files {
            let path = Path::new("/skills").join(relative);
            transaction
                .fs
                .mkdir(path.parent().context("Missing skill parent")?, true)
                .await?;
            transaction.fs.write_file(&path, &bytes).await?;
        }
        transaction.imported_skills = imported_skills;
        for file in &input.files {
            let attachment_root = self.directory.join("attachments").canonicalize()?;
            let path = file.path.canonicalize()?;
            ensure!(
                path.starts_with(&attachment_root),
                "Attachment belongs to another space"
            );
            let bytes = file.bytes().await?;
            let id = file.path.file_name().context("Missing attachment ID")?;
            let virtual_path = Path::new("/attachments").join(id).join(&file.filename);
            transaction
                .fs
                .mkdir(
                    virtual_path.parent().context("Missing attachment parent")?,
                    true,
                )
                .await?;
            transaction.fs.write_file(&virtual_path, &bytes).await?;
            input.content.push_str(&format!(
                "\n[Attachment in virtual filesystem: {}]",
                virtual_path.display()
            ));
        }
        let catalog = super::skills::virtual_catalog(&transaction.fs.snapshot())?;
        transaction.save().await?;
        Ok(catalog)
    }

    async fn open(&self) -> anyhow::Result<Transaction> {
        let directory = self.directory.clone();
        let ownership = super::run_state::execution_ownership();
        let transaction = tokio::task::spawn_blocking(move || -> anyhow::Result<Transaction> {
            let _ownership = ownership;
            std::fs::create_dir_all(&directory)?;
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(directory.join("sandbox.lock"))?;
            lock.lock()?;
            let fs = Arc::new(InMemoryFs::with_limits(
                FsLimits::new()
                    .max_total_bytes(50_000_000)
                    .max_file_size(20_000_000)
                    .max_file_count(10_000)
                    .max_dir_count(2_000)
                    .max_path_depth(32),
            ));
            let mut imported_skills = BTreeSet::new();
            match std::fs::File::open(directory.join("sandbox.json")) {
                Ok(file) => {
                    ensure!(
                        file.metadata()?.len() <= MAX_SNAPSHOT_BYTES,
                        "Sandbox snapshot exceeds size limit"
                    );
                    let mut bytes = Vec::new();
                    file.take(MAX_SNAPSHOT_BYTES + 1).read_to_end(&mut bytes)?;
                    ensure!(
                        bytes.len() as u64 <= MAX_SNAPSHOT_BYTES,
                        "Sandbox snapshot exceeds size limit"
                    );
                    let snapshot: Snapshot =
                        serde_json::from_slice(&bytes).context("Invalid sandbox snapshot")?;
                    fs.restore(&snapshot.fs)?;
                    imported_skills = snapshot.imported_skills;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(Transaction {
                fs,
                imported_skills,
                directory,
                _lock: lock,
            })
        })
        .await??;
        transaction.fs.mkdir(Path::new("/workspace"), true).await?;
        Ok(transaction)
    }

    async fn execute(&self, name: &str, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let transaction = self.open().await?;
        let fs = transaction.fs.clone();
        let output = match name {
            "exec_command" => {
                let args: ExecCommandArgs = serde_json::from_value(args)?;
                ensure!(!args.cmd.trim().is_empty(), "cmd must not be empty");
                let timeout_ms = args.timeout_ms.unwrap_or(30_000);
                ensure!(
                    timeout_ms > 0 && timeout_ms <= 300_000,
                    "Sandbox timeout must be 1–300000 ms"
                );
                let duration = Duration::from_millis(timeout_ms);
                let cwd = virtual_path(args.workdir.as_deref().unwrap_or("/workspace"))?;
                ensure!(
                    fs.stat(&cwd).await?.file_type.is_dir(),
                    "workdir must be a virtual directory"
                );
                let limits = ExecutionLimits {
                    timeout: duration,
                    parser_timeout: duration.min(Duration::from_secs(2)),
                    max_input_bytes: 1_000_000,
                    ..Default::default()
                };
                let mut bash = Bash::builder()
                    .fs(fs.clone())
                    .cwd(cwd)
                    .env("HOME", "/workspace")
                    .limits(limits)
                    .build();
                let result = tokio::time::timeout(duration, bash.exec(&args.cmd)).await;
                match result {
                    Ok(Ok(out)) => json!({"stdout":out.stdout.to_string(), "stderr":out.stderr.to_string(),
                        "exit_code":out.exit_code,"timed_out":false,
                        "stdout_truncated":out.stdout_truncated,"stderr_truncated":out.stderr_truncated,
                        "stdout_path":null,"stderr_path":null}).into(),
                    Err(_) | Ok(Err(bashkit::Error::ResourceLimit(bashkit::LimitExceeded::Timeout(_)))) |
                    Ok(Err(bashkit::Error::ResourceLimit(bashkit::LimitExceeded::ParserTimeout(_)))) => {
                        json!({"stdout":"","stderr":"Sandbox execution timed out; earlier virtual file changes may persist",
                            "exit_code":null,"timed_out":true,"stdout_truncated":false,"stderr_truncated":false,
                            "stdout_path":null,"stderr_path":null}).into()
                    }
                    Ok(Err(error)) => return Err(error.into()),
                }
            }
            "view_image" => {
                let args: ViewImageArgs = serde_json::from_value(args)?;
                let path = virtual_path(&args.path)?;
                let bytes = fs.read_file(&path).await?;
                let image = Image::from_bytes(&bytes)?;
                return Ok(ToolOutput {
                    output: json!({"path":path,"width":image.width,"height":image.height}),
                    images: vec![image],
                });
            }
            "apply_patch" => {
                let args: ApplyPatchArgs = serde_json::from_value(args)?;
                apply_virtual_patch(fs.as_ref(), args).await?.into()
            }
            _ => anyhow::bail!("Unknown sandbox tool"),
        };
        transaction.save().await?;
        Ok(output)
    }
}

fn collect_skills(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
    total: &mut u64,
) -> anyhow::Result<()> {
    ensure!(
        directory.strip_prefix(root)?.components().count() <= 16,
        "Skill directory nesting exceeds limit"
    );
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "Symlinks are not allowed in sandbox skills"
        );
        if kind.is_dir() {
            collect_skills(root, &entry.path(), files, total)?;
        } else if kind.is_file() {
            let file = std::fs::File::open(entry.path())?;
            let mut bytes = Vec::new();
            file.take(5_000_001).read_to_end(&mut bytes)?;
            *total += bytes.len() as u64;
            ensure!(
                *total <= 10_000_000 && bytes.len() <= 5_000_000 && files.len() < 1_000,
                "Sandbox skills exceed import limit"
            );
            files.push((entry.path().strip_prefix(root)?.to_owned(), bytes));
        }
    }
    Ok(())
}

impl Transaction {
    async fn save(self) -> anyhow::Result<()> {
        let ownership = super::run_state::execution_ownership();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _ownership = ownership;
            let transaction = self;
            let snapshot = Snapshot {
                fs: transaction.fs.snapshot(),
                imported_skills: transaction.imported_skills.clone(),
            };
            let mut file = tempfile::NamedTempFile::new_in(&transaction.directory)?;
            serde_json::to_writer(file.as_file_mut(), &snapshot)?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(transaction.directory.join("sandbox.json"))?;
            Ok(())
        })
        .await?
    }
}

fn virtual_path(path: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !path.trim().is_empty() && !path.contains('\0'),
        "Invalid virtual path"
    );
    let path = Path::new(path);
    ensure!(
        !path
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "Parent traversal is not allowed"
    );
    Ok(Path::new("/workspace").join(path))
}

async fn apply_virtual_patch(
    fs: &InMemoryFs,
    args: ApplyPatchArgs,
) -> anyhow::Result<serde_json::Value> {
    let root = virtual_path(args.workdir.as_deref().unwrap_or("/workspace"))?;
    ensure!(
        fs.stat(&root).await?.file_type.is_dir(),
        "workdir must be a virtual directory"
    );
    let path = |value: &str| -> anyhow::Result<PathBuf> {
        let candidate = root.join(value);
        let candidate = virtual_path(candidate.to_str().context("Invalid path")?)?;
        ensure!(
            candidate.starts_with(&root) && candidate != root,
            "Path is outside workdir"
        );
        ensure!(
            !fs.snapshot().entries().iter().any(|entry| {
                matches!(entry.kind, bashkit::VfsEntryKind::Symlink { .. })
                    && candidate.starts_with(&entry.path)
            }),
            "Patch paths must not traverse symbolic links"
        );
        Ok(candidate)
    };
    // Work on a disposable VFS: any preparation/write error leaves the saved disk unchanged.
    let mut changed = Vec::new();
    for operation in apply_patch::parse_patch(&args.patch)? {
        match operation {
            Operation::Add(name, text) => {
                let target = path(&name)?;
                fs.mkdir(target.parent().context("Missing parent")?, true)
                    .await?;
                fs.write_file(&target, text.as_bytes()).await?;
                changed.push(json!({"path":target,"action":"add"}));
            }
            Operation::Delete(name) => {
                let target = path(&name)?;
                fs.read_file(&target).await?;
                fs.remove(&target, false).await?;
                changed.push(json!({"path":target,"action":"delete"}));
            }
            Operation::Update(name, destination, chunks) => {
                let source = path(&name)?;
                let content = fs.read_file(&source).await?;
                let updated = apply_patch::update(&content, chunks)?;
                let target = destination
                    .map(|name| path(&name))
                    .transpose()?
                    .unwrap_or_else(|| source.clone());
                fs.mkdir(target.parent().context("Missing parent")?, true)
                    .await?;
                fs.write_file(&target, &updated).await?;
                if target != source {
                    fs.remove(&source, false).await?;
                }
                changed.push(json!({"path":target,"action":"update"}));
            }
        }
    }
    Ok(json!({"changed_files":changed}))
}

struct SandboxTool {
    sandbox: Arc<Sandbox>,
    name: &'static str,
}

#[async_trait::async_trait]
impl Tool for SandboxTool {
    fn name(&self) -> String {
        self.name.into()
    }
    fn description(&self) -> String {
        match self.name {
            "exec_command" => "Run Bashkit built-in shell commands in this chat's persistent virtual filesystem. Default workdir /workspace; skills in /skills, imported files in /attachments. No host programs, host files, host environment, or network. Commands do not fall back to a real shell. Files persist; shell variables and cwd reset each call. Timeout 1–300000 ms (default 30000). Cancellation before saving rolls back this call's file changes; an already started atomic save completes. Output limited to 1 MiB per stream; truncated output is not retained elsewhere.".into(),
            "apply_patch" => format!("{} Paths are inside this chat's virtual disk; default workdir /workspace. An error rolls back the whole patch.", ApplyPatchTool.description().replace("path, action, added and removed line counts", "path and action").replace("Writes are atomic per file; an I/O failure reports files already changed.", "The complete patch is committed atomically.")),
            "view_image" => "View a PNG, JPEG or WebP from this chat's virtual disk. Relative paths start at /workspace. No host file access; maximum 10 MiB and 32 million pixels.".into(),
            _ => unreachable!(),
        }
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = match self.name {
            "exec_command" => ExecCommandTool.parameters(),
            "apply_patch" => ApplyPatchTool.parameters(),
            "view_image" => ViewImageTool.parameters(),
            _ => unreachable!(),
        };
        if self.name == "view_image" {
            schema["properties"]["path"]["description"] = json!(
                "Image path in this chat's virtual filesystem; relative paths start at /workspace."
            );
        } else {
            schema["properties"]["workdir"]["description"] = json!(
                "Virtual working directory; null uses /workspace. Host paths are unavailable."
            );
        }
        schema
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.sandbox.execute(self.name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn sandbox_import_refresh_can_replace_directory_with_file() {
        let root = tempfile::tempdir().unwrap();
        let assets = root.path().join("skills/example/assets");
        std::fs::create_dir_all(assets.join("nested")).unwrap();
        std::fs::write(assets.join("nested/helper.sh"), "echo old").unwrap();
        let sandbox = Sandbox::new(root.path().to_owned());
        let mut input = super::super::UserMessage {
            content: "hello".into(),
            files: vec![],
            images: vec![],
        };
        sandbox.prepare_input(&mut input).await.unwrap();
        std::fs::remove_dir_all(&assets).unwrap();
        std::fs::write(&assets, "new asset").unwrap();
        let reopened = Sandbox::new(root.path().to_owned());
        reopened.prepare_input(&mut input).await.unwrap();
        let output = reopened
            .execute("exec_command", json!({"cmd":"cat /skills/example/assets"}))
            .await
            .unwrap();
        assert_eq!(output.output["stdout"], "new asset");
    }

    #[tokio::test]
    async fn sandbox_preserves_legacy_snapshot_without_import_metadata() {
        let root = tempfile::tempdir().unwrap();
        let fs = InMemoryFs::new();
        fs.mkdir(Path::new("/workspace"), true).await.unwrap();
        fs.write_file(Path::new("/workspace/note"), b"keep legacy work")
            .await
            .unwrap();
        std::fs::write(
            root.path().join("sandbox.json"),
            serde_json::to_vec(&fs.snapshot()).unwrap(),
        )
        .unwrap();
        for _ in 0..2 {
            let output = Sandbox::new(root.path().to_owned())
                .execute("exec_command", json!({"cmd":"cat note"}))
                .await
                .unwrap();
            assert_eq!(output.output["stdout"], "keep legacy work");
        }
    }

    #[tokio::test]
    async fn sandbox_removes_deleted_imports_and_preserves_virtual_skills_after_reopen() {
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("skills/learned-example");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: learned-example\ndescription: FORGOTTEN_PROCEDURE\n---\nDo work.",
        )
        .unwrap();
        std::fs::write(skill.join("helper.sh"), "echo obsolete").unwrap();
        let sandbox = Sandbox::new(root.path().to_owned());
        let mut input = super::super::UserMessage {
            content: "hello".into(),
            files: vec![],
            images: vec![],
        };
        assert!(
            sandbox
                .prepare_input(&mut input)
                .await
                .unwrap()
                .contains("FORGOTTEN_PROCEDURE")
        );
        sandbox.execute("apply_patch", json!({"workdir":"/", "patch":"*** Begin Patch\n*** Add File: skills/local/SKILL.md\n+---\n+name: local\n+description: VIRTUAL_PROCEDURE\n+---\n+Local work.\n*** Add File: skills/learned-example/local.txt\n+keep me\n*** End Patch"})).await.unwrap();
        std::fs::remove_dir_all(&skill).unwrap();
        let reopened = Sandbox::new(root.path().to_owned());
        let catalog = reopened.prepare_input(&mut input).await.unwrap();
        assert!(!catalog.contains("FORGOTTEN_PROCEDURE"));
        assert!(catalog.contains("VIRTUAL_PROCEDURE"));
        let output = reopened.execute("exec_command", json!({"cmd":"test ! -e /skills/learned-example/SKILL.md && test ! -e /skills/learned-example/helper.sh && cat /skills/learned-example/local.txt"})).await.unwrap();
        assert_eq!(output.output["exit_code"], 0);
        assert_eq!(output.output["stdout"], "keep me\n");
        assert!(
            !Sandbox::new(root.path().to_owned())
                .prepare_input(&mut input)
                .await
                .unwrap()
                .contains("FORGOTTEN_PROCEDURE")
        );
    }

    #[tokio::test]
    async fn sandbox_imports_only_own_attachments_and_discovers_persisted_skills() {
        use super::super::{FileAttachment, UserMessage};
        let root = tempfile::tempdir().unwrap();
        let group = Sandbox::new(root.path().join("group"));
        let own = FileAttachment::save(
            &root.path().join("group/attachments"),
            "notes.txt",
            None,
            b"GROUP_NOTES",
        )
        .unwrap();
        let other = FileAttachment::save(
            &root.path().join("private/attachments"),
            "secret.txt",
            None,
            b"PRIVATE_NOTES",
        )
        .unwrap();
        let mut input = UserMessage {
            content: "read attachment".into(),
            files: vec![own.clone()],
            images: vec![],
        };
        group.prepare_input(&mut input).await.unwrap();
        let path = format!(
            "/attachments/{}/notes.txt",
            own.path.file_name().unwrap().to_str().unwrap()
        );
        assert!(input.content.contains(&path));
        assert_eq!(
            group
                .execute("exec_command", json!({"cmd":format!("cat {path}")}))
                .await
                .unwrap()
                .output["stdout"],
            "GROUP_NOTES"
        );
        input.files = vec![other];
        assert!(group.prepare_input(&mut input).await.is_err());
        input.files.clear();
        group.execute("apply_patch", json!({"workdir":"/", "patch":"*** Begin Patch\n*** Add File: skills/group-only/SKILL.md\n+---\n+name: group-only\n+description: GROUP_PROCEDURE\n+---\n+Do group work.\n*** End Patch"})).await.unwrap();
        let reopened = Sandbox::new(root.path().join("group"));
        let catalog = reopened.prepare_input(&mut input).await.unwrap();
        assert!(catalog.contains("GROUP_PROCEDURE"));
        assert!(catalog.contains("/skills/group-only/SKILL.md"));
        let other = Sandbox::new(root.path().join("other"));
        assert!(
            !other
                .prepare_input(&mut input)
                .await
                .unwrap()
                .contains("GROUP_PROCEDURE")
        );
    }

    #[tokio::test]
    async fn sandbox_images_and_symlinks_stay_virtual() {
        use base64::Engine;
        let root = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(root.path().to_owned());
        let bytes = super::super::image::tests::sample(image::ImageFormat::Png);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        sandbox
            .execute(
                "exec_command",
                json!({"cmd":format!("printf '%s' '{encoded}' | base64 -d > image.png")}),
            )
            .await
            .unwrap();
        let image = sandbox
            .execute("view_image", json!({"path":"image.png"}))
            .await
            .unwrap();
        assert_eq!(image.images.len(), 1);
        assert_eq!(image.output["width"], 2);
        sandbox
            .execute("exec_command", json!({"cmd":"ln -s image.png linked.png"}))
            .await
            .unwrap();
        assert!(sandbox.execute("apply_patch", json!({"patch":"*** Begin Patch\n*** Add File: linked.png\n+overwrite\n*** End Patch"})).await.is_err());
        assert!(
            sandbox
                .execute("view_image", json!({"path":"image.png"}))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn sandbox_concurrent_calls_do_not_lose_writes_and_cancel_rolls_back() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let root = tempfile::tempdir().unwrap();
            let first = Sandbox::new(root.path().to_owned());
            let second = Sandbox::new(root.path().to_owned());
            let (a, b) = tokio::join!(
                first.execute("exec_command", json!({"cmd":"printf a >> log"})),
                second.execute("exec_command", json!({"cmd":"printf b >> log"})),
            );
            a.unwrap();
            b.unwrap();
            let out = first
                .execute("exec_command", json!({"cmd":"cat log"}))
                .await
                .unwrap();
            assert!(matches!(out.output["stdout"].as_str(), Some("ab" | "ba")));
            let running = Sandbox::new(root.path().to_owned());
            let task = tokio::spawn(async move {
                running
                    .execute(
                        "exec_command",
                        json!({"cmd":"echo partial > cancelled; sleep 60"}),
                    )
                    .await
            });
            let lock = std::fs::File::open(root.path().join("sandbox.lock")).unwrap();
            loop {
                match lock.try_lock() {
                    Ok(()) => {
                        lock.unlock().unwrap();
                        tokio::task::yield_now().await;
                    }
                    Err(std::fs::TryLockError::WouldBlock) => break,
                    Err(error) => panic!("{error}"),
                }
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let out = first
                .execute("exec_command", json!({"cmd":"cat cancelled"}))
                .await
                .unwrap();
            assert_ne!(out.output["exit_code"], 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sandbox_persists_only_its_own_virtual_files() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("owner-secret");
        std::fs::write(&secret, "PRIVATE_SENTINEL").unwrap();
        let group = Sandbox::new(root.path().join("group"));
        let out = group
            .execute(
                "exec_command",
                json!({"cmd":"echo public > note; cat note"}),
            )
            .await
            .unwrap();
        assert_eq!(out.output["stdout"], "public\n");
        let reopened = Sandbox::new(root.path().join("group"));
        assert_eq!(
            reopened
                .execute("exec_command", json!({"cmd":"cat note"}))
                .await
                .unwrap()
                .output["stdout"],
            "public\n"
        );
        let other = Sandbox::new(root.path().join("other"));
        assert_ne!(
            other
                .execute("exec_command", json!({"cmd":"cat note"}))
                .await
                .unwrap()
                .output["exit_code"],
            0
        );
        for cmd in [
            format!("cat {}", secret.display()),
            format!("/bin/sh -c 'cat {}'", secret.display()),
            "printenv".into(),
        ] {
            let out = group
                .execute("exec_command", json!({"cmd":cmd}))
                .await
                .unwrap();
            assert!(!out.output.to_string().contains("PRIVATE_SENTINEL"));
        }
        group
            .execute(
                "exec_command",
                json!({"cmd":format!("echo stolen > {}", secret.display())}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(secret).unwrap(), "PRIVATE_SENTINEL");
        assert!(!root.path().join("group/note").exists());
    }

    #[tokio::test]
    async fn sandbox_patch_and_images_share_the_shell_disk_and_fail_atomically() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(root.path().to_owned());
        sandbox
            .execute(
                "apply_patch",
                json!({"patch":"*** Begin Patch\n*** Add File: note\n+first\n*** End Patch"}),
            )
            .await
            .unwrap();
        assert_eq!(
            sandbox
                .execute("exec_command", json!({"cmd":"cat note"}))
                .await
                .unwrap()
                .output["stdout"],
            "first\n"
        );
        assert!(sandbox.execute("apply_patch", json!({"patch":"*** Begin Patch\n*** Add File: lost\n+not committed\n*** Update File: missing\n@@\n-no\n+yes\n*** End Patch"})).await.is_err());
        assert_ne!(
            sandbox
                .execute("exec_command", json!({"cmd":"cat lost"}))
                .await
                .unwrap()
                .output["exit_code"],
            0
        );
        assert!(
            sandbox
                .execute("view_image", json!({"path":"/etc/passwd"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sandbox_timeout_and_bad_snapshot_never_fall_back_to_host() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(root.path().to_owned());
        let result = sandbox
            .execute("exec_command", json!({"cmd":"sleep 5", "timeout_ms":10}))
            .await
            .unwrap();
        assert_eq!(result.output["timed_out"], true);
        std::fs::write(root.path().join("sandbox.json"), "broken").unwrap();
        assert!(
            sandbox
                .execute("exec_command", json!({"cmd":"echo hello"}))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("sandbox.json")).unwrap(),
            "broken"
        );
    }
}
