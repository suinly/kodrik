use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct PageStore {
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Visibility {
    Private,
    Public,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub release: Uuid,
    pub visibility: Visibility,
    #[serde(default)]
    pub document: bool,
}

pub(crate) struct PageFile {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub download: bool,
}

pub(crate) struct PageSummary {
    pub slug: String,
    pub title: String,
    pub visibility: Visibility,
    pub document: bool,
}

pub(crate) fn validate_slug(slug: &str) -> anyhow::Result<()> {
    ensure!(
        (1..=64).contains(&slug.len())
            && slug
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
        "Invalid page slug"
    );
    Ok(())
}

// Resolve platform aliases such as macOS /var before checking our own tree.
pub(super) fn data_root(data: &Path) -> PathBuf {
    data.canonicalize().unwrap_or_else(|_| data.to_path_buf())
}

pub(super) fn reject_symlinks(path: &Path) -> std::io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if meta.is_symlink() => {
                return Err(std::io::Error::other("Symlink is forbidden"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn checked_dir(path: &Path) -> std::io::Result<()> {
    reject_symlinks(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}

pub(super) fn open_regular(path: &Path) -> std::io::Result<File> {
    reject_symlinks(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("Not a regular file"));
    }
    Ok(file)
}

impl PageStore {
    pub(crate) fn new(data: &Path) -> Self {
        Self {
            root: data_root(data).join("pages"),
        }
    }
    pub(crate) fn drafts(&self) -> PathBuf {
        self.root.join("drafts")
    }

    pub(crate) fn list(&self) -> anyhow::Result<Vec<PageSummary>> {
        let directory = self.root.join("manifests");
        reject_symlinks(&directory)?;
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error.into()),
        };
        let mut pages = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(slug) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if validate_slug(slug).is_err() {
                continue;
            }
            let Some(manifest) = self.manifest(slug)? else {
                continue;
            };
            let title = if manifest.document {
                let file = self
                    .read(slug, &manifest, "document.json")?
                    .context("Published document is missing")?;
                serde_json::from_slice::<super::documents::PublishedDocument>(&file.bytes)?
                    .document
                    .title
            } else {
                slug.to_owned()
            };
            pages.push(PageSummary {
                slug: slug.to_owned(),
                title,
                visibility: manifest.visibility,
                document: manifest.document,
            });
        }
        pages.sort_by_cached_key(|page| (page.title.to_lowercase(), page.slug.clone()));
        Ok(pages)
    }

    pub(crate) fn manifest(&self, slug: &str) -> anyhow::Result<Option<Manifest>> {
        validate_slug(slug)?;
        let path = self.root.join("manifests").join(format!("{slug}.json"));
        let file = match open_regular(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "Page manifest exceeds limit");
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub(crate) fn publish(
        &self,
        slug: &str,
        visibility: Option<Visibility>,
    ) -> anyhow::Result<Manifest> {
        self.publish_release(slug, visibility, None)
    }

    pub(crate) fn publish_document(
        &self,
        slug: &str,
        document: &super::documents::PublishedDocument,
        visibility: Option<Visibility>,
    ) -> anyhow::Result<Manifest> {
        let bytes = serde_json::to_vec(document)?;
        ensure!(
            bytes.len() <= 2 * super::documents::MAX_DOCUMENT,
            "Published document exceeds limit"
        );
        self.publish_release(slug, visibility, Some(bytes))
    }

    fn lock(&self, slug: &str) -> anyhow::Result<File> {
        validate_slug(slug)?;
        let locks = self.root.join("locks");
        checked_dir(&locks)?;
        let lock_path = locks.join(format!("{slug}.lock"));
        reject_symlinks(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_path)?;
        lock.lock()?;
        Ok(lock)
    }

    pub(crate) fn delete(&self, slug: &str) -> anyhow::Result<()> {
        let _lock = self.lock(slug)?;
        let path = self.root.join("manifests").join(format!("{slug}.json"));
        reject_symlinks(&path)?;
        // shortcut: release snapshots remain on disk; add garbage collection if retention needs limits.
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn publish_release(
        &self,
        slug: &str,
        visibility: Option<Visibility>,
        document: Option<Vec<u8>>,
    ) -> anyhow::Result<Manifest> {
        let _lock = self.lock(slug)?;
        let old = self.manifest(slug)?;
        let draft = self.drafts().join(slug);
        if document.is_none() {
            open_regular(&draft.join("index.html"))
                .context("Page requires a regular index.html")?;
        }
        let releases = self.root.join("releases").join(slug);
        checked_dir(&releases)?;
        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&releases)?;
        let mut budget = Budget {
            bytes: 0,
            files: 0,
            directories: 0,
        };
        if let Some(bytes) = &document {
            fs::write(staging.path().join("document.json"), bytes)?;
            File::open(staging.path().join("document.json"))?.sync_all()?;
        } else {
            copy_tree(&draft, staging.path(), 0, &mut budget)?;
        }
        let manifest = Manifest {
            release: Uuid::new_v4(),
            document: document.is_some(),
            visibility: visibility
                .or(old.map(|m| m.visibility))
                .unwrap_or(Visibility::Private),
        };
        fs::rename(staging.path(), releases.join(manifest.release.to_string()))?;
        let manifests = self.root.join("manifests");
        checked_dir(&manifests)?;
        let mut file = tempfile::NamedTempFile::new_in(&manifests)?;
        serde_json::to_writer(&mut file, &manifest)?;
        file.as_file().sync_all()?;
        file.persist(manifests.join(format!("{slug}.json")))?;
        Ok(manifest)
    }

    pub(crate) fn read(
        &self,
        slug: &str,
        manifest: &Manifest,
        relative: &str,
    ) -> anyhow::Result<Option<PageFile>> {
        validate_slug(slug)?;
        ensure!(
            !relative.is_empty()
                && !relative.contains(['\\', '\0'])
                && relative
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "Invalid page path"
        );
        let path = self
            .root
            .join("releases")
            .join(slug)
            .join(manifest.release.to_string())
            .join(relative);
        let file = match open_regular(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_BYTES, "Page file exceeds limit");
        let mime = mime_guess::from_path(relative).first_raw();
        Ok(Some(PageFile {
            bytes,
            mime: mime.unwrap_or("application/octet-stream").into(),
            download: mime.is_none(),
        }))
    }
}

struct Budget {
    bytes: u64,
    files: usize,
    directories: usize,
}

fn copy_tree(
    source: &Path,
    target: &Path,
    depth: usize,
    budget: &mut Budget,
) -> anyhow::Result<()> {
    reject_symlinks(source)?;
    ensure!(depth <= 32, "Page nesting exceeds limit");
    budget.directories += 1;
    ensure!(budget.directories <= 1000, "Too many page directories");
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&destination)?;
            copy_tree(&entry.path(), &destination, depth + 1, budget)?;
        } else {
            ensure!(kind.is_file(), "Page contains a symlink or special file");
            budget.files += 1;
            ensure!(budget.files <= 1000, "Too many page files");
            let source = open_regular(&entry.path())?;
            let mut bytes = Vec::new();
            source
                .take(MAX_BYTES - budget.bytes + 1)
                .read_to_end(&mut bytes)?;
            budget.bytes += bytes.len() as u64;
            ensure!(budget.bytes <= MAX_BYTES, "Page exceeds 32 MiB");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(destination)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
    }
    if depth == 0 {
        open_regular(&target.join("index.html"))
            .context("Published snapshot requires index.html")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_removes_publication_and_preserves_sources() {
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(dir.path());
        let draft = store.drafts().join("report");
        fs::create_dir_all(&draft).unwrap();
        fs::write(draft.join("index.html"), "report").unwrap();
        store.publish("report", Some(Visibility::Public)).unwrap();
        store.delete("report").unwrap();
        assert!(store.manifest("report").unwrap().is_none());
        assert!(store.list().unwrap().is_empty());
        assert_eq!(fs::read(draft.join("index.html")).unwrap(), b"report");
        store.delete("report").unwrap();
        assert!(store.delete("../report").is_err());
        let published = store.publish("report", None).unwrap();
        assert_eq!(published.visibility, Visibility::Private);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn deletion_rejects_manifest_and_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(dir.path());
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("report.json"), "keep").unwrap();
        let manifests = store.root.join("manifests");
        fs::create_dir_all(&manifests).unwrap();
        symlink(outside.join("report.json"), manifests.join("report.json")).unwrap();
        assert!(store.delete("report").is_err());
        fs::remove_file(manifests.join("report.json")).unwrap();
        fs::remove_dir(&manifests).unwrap();
        symlink(&outside, &manifests).unwrap();
        assert!(store.delete("report").is_err());
        assert_eq!(fs::read(outside.join("report.json")).unwrap(), b"keep");
    }

    #[test]
    fn catalog_ignores_unrelated_files_and_rejects_manifest_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let store = PageStore::new(dir.path());
        assert!(store.list().unwrap().is_empty());
        let manifests = store.root.join("manifests");
        fs::create_dir_all(&manifests).unwrap();
        for name in [".temporary", "notes.txt", "INVALID.json"] {
            fs::write(manifests.join(name), "unrelated file").unwrap();
        }
        assert!(store.list().unwrap().is_empty());
        let outside = dir.path().join("outside.json");
        fs::write(
            &outside,
            r#"{"release":"00000000-0000-0000-0000-000000000000","visibility":"public"}"#,
        )
        .unwrap();
        symlink(&outside, manifests.join("outside.json")).unwrap();
        assert!(store.list().is_err());
        fs::remove_file(manifests.join("outside.json")).unwrap();
        fs::write(manifests.join("broken.json"), "broken manifest").unwrap();
        assert!(store.list().is_err());
    }

    #[test]
    fn publish_preserves_access_and_previous_release_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let draft = dir.path().join("pages/drafts/nutrition");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "first").unwrap();
        let store = PageStore::new(dir.path());
        let first = store
            .publish("nutrition", Some(Visibility::Public))
            .unwrap();
        std::fs::write(draft.join("index.html"), "second").unwrap();
        let second = store.publish("nutrition", None).unwrap();
        assert_eq!(second.visibility, Visibility::Public);
        assert_ne!(first.release, second.release);
        assert_eq!(
            store
                .read("nutrition", &second, "index.html")
                .unwrap()
                .unwrap()
                .bytes,
            b"second"
        );
        std::fs::remove_file(draft.join("index.html")).unwrap();
        assert!(store.publish("nutrition", None).is_err());
        assert_eq!(
            store.manifest("nutrition").unwrap().unwrap().release,
            second.release
        );
        assert!(store.publish("../outside", None).is_err());
        assert!(store.read("nutrition", &second, "../index.html").is_err());
        assert_eq!(first.visibility, Visibility::Public);
    }

    #[test]
    fn publishing_rejects_symlinks_limits_and_private_by_default() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let draft = dir.path().join("pages/drafts/test");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "ok").unwrap();
        let store = PageStore::new(dir.path());
        let first = store.publish("test", None).unwrap();
        assert_eq!(first.visibility, Visibility::Private);
        symlink("index.html", draft.join("link")).unwrap();
        assert!(store.publish("test", None).is_err());
        std::fs::remove_file(draft.join("link")).unwrap();
        let file = std::fs::File::create(draft.join("large")).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        assert!(store.publish("test", None).is_err());
        std::fs::remove_file(draft.join("large")).unwrap();
        for i in 0..1000 {
            std::fs::write(draft.join(format!("file{i}")), "").unwrap();
        }
        assert!(store.publish("test", None).is_err());
        assert_eq!(
            store.manifest("test").unwrap().unwrap().release,
            first.release
        );
        for path in [
            "/etc/passwd",
            "a/../index.html",
            "a\\b",
            "a//b",
            "./index.html",
        ] {
            assert!(store.read("test", &first, path).is_err(), "{path}");
        }
    }

    #[test]
    fn copied_snapshot_requires_index_even_if_draft_changed_after_preflight() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let source_path = source.path().canonicalize().unwrap();
        let target_path = target.path().canonicalize().unwrap();
        std::fs::write(source_path.join("index.html"), "old").unwrap();
        open_regular(&source_path.join("index.html")).unwrap();
        std::fs::remove_file(source_path.join("index.html")).unwrap();
        std::fs::write(source_path.join("data.json"), "{}").unwrap();
        let mut budget = Budget {
            bytes: 0,
            files: 0,
            directories: 0,
        };
        assert!(copy_tree(&source_path, &target_path, 0, &mut budget).is_err());
    }
}
