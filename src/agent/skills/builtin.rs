use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::ErrorKind,
    path::Path,
};

use anyhow::Context;
use include_dir::{Dir, DirEntry};

pub(super) const BUNDLED: Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/skills");
const MARKER: &str = ".kodrik-builtin-skills.marker";

/// Retain the returned lock until the caller finishes reading the catalog.
pub(super) fn install(root: &Path) -> anyhow::Result<fs::File> {
    fs::create_dir_all(root)
        .with_context(|| format!("Cannot create skills directory {}", root.display()))?;
    // Keep the lock outside .system, which is replaced during updates.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".system.lock"))
        .context("Cannot open built-in skills lock")?;
    lock.lock().context("Cannot lock built-in skills")?;

    let destination = root.join(".system");
    let exists = match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.is_symlink(),
                "Built-in skills destination must be a directory, not a file or symlink: {}",
                destination.display()
            );
            true
        }
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("Cannot inspect built-in skills destination"),
    };
    let expected = fingerprint(&BUNDLED);
    // ponytail: the marker checks bundle version, not cache integrity;
    // remove the marker to repair the cache, add file checks if tampering matters.
    if exists
        && fs::read_to_string(destination.join(MARKER)).is_ok_and(|value| value.trim() == expected)
    {
        return Ok(lock);
    }

    let staging = tempfile::Builder::new()
        .prefix(".system-")
        .tempdir_in(root)
        .context("Cannot create built-in skills staging directory")?;
    BUNDLED
        .extract(staging.path())
        .context("Cannot extract built-in skills")?;
    fs::write(staging.path().join(MARKER), format!("{expected}\n"))
        .context("Cannot write built-in skills marker")?;
    if exists {
        fs::remove_dir_all(&destination).context("Cannot replace previous built-in skills")?;
    }
    fs::rename(staging.path(), &destination).context("Cannot install built-in skills")?;
    Ok(lock)
}

fn fingerprint(directory: &Dir<'_>) -> String {
    fn hash_directory(directory: &Dir<'_>, hasher: &mut DefaultHasher) {
        let mut entries: Vec<_> = directory.entries().iter().collect();
        entries.sort_unstable_by_key(|entry| entry.path());
        for entry in entries {
            entry.path().hash(hasher);
            match entry {
                DirEntry::Dir(directory) => hash_directory(directory, hasher),
                DirEntry::File(file) => file.contents().hash(hasher),
            }
        }
    }
    let mut hasher = DefaultHasher::new();
    "v1".hash(&mut hasher);
    hash_directory(directory, &mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_keeps_lock_until_catalog_reader_releases_it() {
        let root = tempfile::tempdir().unwrap();
        let lock = install(root.path()).unwrap();
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.path().join(".system.lock"))
            .unwrap();
        assert!(contender.try_lock().is_err());
        drop(lock);
        assert!(contender.try_lock().is_ok());
    }

    #[test]
    fn fingerprint_tracks_nested_paths_and_contents_independent_of_entry_order() {
        use include_dir::File;

        let files = [
            DirEntry::File(File::new("skill/SKILL.md", b"instructions")),
            DirEntry::File(File::new("skill/script.py", b"print(1)")),
        ];
        let nested = [DirEntry::Dir(Dir::new("skill", &files))];
        let expected = fingerprint(&Dir::new("", &nested));
        let reversed = [files[1].clone(), files[0].clone()];
        let nested_reversed = [DirEntry::Dir(Dir::new("skill", &reversed))];
        assert_eq!(expected, fingerprint(&Dir::new("", &nested_reversed)));

        for changed in [
            DirEntry::File(File::new("skill/script.py", b"print(2)")),
            DirEntry::File(File::new("skill/renamed.py", b"print(1)")),
        ] {
            let changed = [files[0].clone(), changed];
            let nested = [DirEntry::Dir(Dir::new("skill", &changed))];
            assert_ne!(expected, fingerprint(&Dir::new("", &nested)));
        }
    }

    #[test]
    fn installs_bundle_skips_matching_marker_and_refreshes_only_system_files() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("skills");
        install(&root).unwrap();
        let system = root.join(".system");
        let creator = system.join("skill-creator/SKILL.md");
        let original = fs::read(&creator).unwrap();
        assert_eq!(
            original,
            BUNDLED
                .get_file("skill-creator/SKILL.md")
                .unwrap()
                .contents()
        );
        let marker = fs::read_to_string(system.join(MARKER)).unwrap();
        assert!(!marker.trim().is_empty());

        let stale = system.join("old-skill");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("SKILL.md"), "stale").unwrap();
        install(&root).unwrap();
        assert!(stale.exists(), "matching fingerprint must skip extraction");

        let user = root.join("skill-creator");
        fs::create_dir(&user).unwrap();
        fs::write(user.join("SKILL.md"), "user-owned").unwrap();
        fs::write(&creator, "old built-in").unwrap();
        fs::write(system.join(MARKER), "outdated fingerprint").unwrap();
        install(&root).unwrap();
        assert_eq!(fs::read(&creator).unwrap(), original);
        assert!(!stale.exists());
        assert_eq!(
            fs::read_to_string(user.join("SKILL.md")).unwrap(),
            "user-owned"
        );
        assert_eq!(fs::read_to_string(system.join(MARKER)).unwrap(), marker);

        fs::remove_file(system.join(MARKER)).unwrap();
        fs::remove_file(&creator).unwrap();
        install(&root).unwrap();
        assert_eq!(fs::read(&creator).unwrap(), original);
    }

    #[test]
    fn concurrent_installations_leave_a_complete_bundle() {
        let root = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let _lock = install(root.path()).unwrap();
                    })
                })
                .collect();
            for job in jobs {
                job.join().unwrap();
            }
        });
        assert_eq!(
            fs::read(root.path().join(".system/skill-creator/SKILL.md")).unwrap(),
            BUNDLED
                .get_file("skill-creator/SKILL.md")
                .unwrap()
                .contents()
        );
        assert_eq!(
            fs::read_to_string(root.path().join(".system").join(MARKER))
                .unwrap()
                .trim(),
            fingerprint(&BUNDLED)
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_destination_without_changing_its_target() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let keep = outside.path().join("keep");
        fs::write(&keep, "user data").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".system")).unwrap();
        assert!(install(root.path()).is_err());
        assert_eq!(fs::read_to_string(keep).unwrap(), "user data");
        assert!(!outside.path().join("skill-creator").exists());
    }
}
