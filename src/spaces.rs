use anyhow::Context;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) const MIGRATION_MARKER: &str = ".personal-migration-complete";
pub(crate) const OWNER_MIGRATION_MARKER: &str = ".owner-migration-complete";
const WEB_MIGRATION_MARKER: &str = ".web-chat-migration-complete";
const OWNER_ROOT_MARKER: &str = ".owner-root-migration-complete";

pub(crate) fn telegram_interface(root: &Path, interface: &str) -> PathBuf {
    let interface: String = interface
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    root.join("spaces/telegram").join(interface)
}

pub(crate) fn telegram(root: &Path, interface: &str, chat: i64) -> PathBuf {
    if chat > 0 {
        personal(root, chat as u64)
    } else {
        telegram_interface(root, interface)
            .join("groups")
            .join(chat.to_string())
    }
}

pub(crate) fn personal(root: &Path, user_id: u64) -> PathBuf {
    root.join("users").join(user_id.to_string())
}

pub(crate) fn telegram_chats(root: &Path, interface: &str) -> anyhow::Result<Vec<i64>> {
    let mut chats = Vec::new();
    for sample in [1, -1] {
        let path = telegram(root, interface, sample);
        let legacy = telegram_interface(root, interface).join("users/1");
        let paths = if sample > 0 {
            vec![path, legacy]
        } else {
            vec![path]
        };
        for path in paths {
            let directory = path.parent().context("Missing Telegram space parent")?;
            let entries = match fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Ok(chat) = name.parse::<i64>() else {
                    continue;
                };
                if chat != 0 && chat.signum() == sample && name == chat.to_string() {
                    chats.push(chat);
                }
            }
        }
    }
    chats.sort_unstable();
    chats.dedup();
    Ok(chats)
}

pub(crate) fn owner_data(root: &Path, config: &crate::config::Config) -> anyhow::Result<PathBuf> {
    config.validate_owner()?;
    match &config.owner {
        Some(owner) => personal_data(root, config, owner.user_id.get()),
        None => migrate_personal(root, root, &[], Some(root), None),
    }
}

pub(crate) fn personal_data(
    root: &Path,
    config: &crate::config::Config,
    user_id: u64,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(user_id > 0, "Invalid Telegram user ID 0");
    let destination = personal(root, user_id);
    let owner = config
        .owner
        .as_ref()
        .filter(|owner| owner.user_id.get() == user_id);
    let marker = if owner.is_some() {
        OWNER_MIGRATION_MARKER
    } else {
        MIGRATION_MARKER
    };
    if destination.join(MIGRATION_MARKER).try_exists()? {
        anyhow::ensure!(
            fs::read_to_string(destination.join(MIGRATION_MARKER))? == "1\n",
            "Unknown personal migration marker"
        );
        // Promotion to owner must not replay already imported Telegram backups.
        return migrate_personal(root, &destination, &[], owner.map(|_| root), owner);
    }
    if destination.join(marker).try_exists()? {
        return migrate_personal(root, &destination, &[], owner.map(|_| root), owner);
    }
    let mut sources = Vec::new();
    match fs::read_dir(root.join("spaces/telegram")) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let source = entry.path().join("users").join(user_id.to_string());
                    if source.try_exists()? {
                        sources.push(source);
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sources.sort();
    migrate_personal(root, &destination, &sources, owner.map(|_| root), owner)
}

fn owner_root_claim(root: &Path) -> anyhow::Result<Option<u64>> {
    match fs::read_to_string(root.join(OWNER_ROOT_MARKER)) {
        Ok(marker) => {
            let id = marker
                .strip_prefix("1:")
                .and_then(|text| text.strip_suffix('\n'))
                .and_then(|text| text.parse::<u64>().ok())
                .filter(|id| *id > 0);
            anyhow::ensure!(
                id.is_some_and(|id| marker == format!("1:{id}\n")),
                "Unknown owner root migration marker"
            );
            return Ok(id);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut directories = vec![root.join("users")];
    match fs::read_dir(root.join("spaces/telegram")) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    directories.push(entry.path().join("users"));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut claim = None;
    for directory in directories {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(id) = name.parse::<u64>() else {
                continue;
            };
            if id == 0 || name != id.to_string() {
                continue;
            }
            match fs::read_to_string(entry.path().join(OWNER_MIGRATION_MARKER)) {
                Ok(marker) => {
                    anyhow::ensure!(marker == "1\n", "Unknown owner migration marker");
                    anyhow::ensure!(
                        claim.is_none_or(|prior| prior == id),
                        "Legacy root ownership is ambiguous; reconcile saved owner migration markers before startup"
                    );
                    claim = Some(id);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(claim)
}

fn record_owner_root_claim(root: &Path, user: u64) -> anyhow::Result<()> {
    if !root.join(OWNER_ROOT_MARKER).try_exists()? {
        fs::write(root.join(OWNER_ROOT_MARKER), format!("1:{user}\n"))?;
    }
    Ok(())
}

fn migrate_personal(
    root: &Path,
    destination: &Path,
    sources: &[PathBuf],
    web_root: Option<&Path>,
    owner: Option<&crate::config::OwnerConfig>,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("spaces.lock"))?;
    lock.lock()?;
    // Root files remain backups after their first owner; never reassign them.
    let claim = owner.map(|_| owner_root_claim(root)).transpose()?.flatten();
    if let Some(claim) = claim {
        record_owner_root_claim(root, claim)?;
    }
    let web_root = web_root.filter(|_| {
        owner.is_none_or(|owner| claim.is_none_or(|claim| claim == owner.user_id.get()))
    });
    let web = if let Some(root) = web_root {
        match fs::read_to_string(root.join(WEB_MIGRATION_MARKER)) {
            Ok(version) => {
                anyhow::ensure!(version == "1\n", "Unknown web chat migration marker");
                None
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Some(root.join("web/chat-sessions"))
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let mut sources = sources.to_vec();
    if owner.is_some() && claim.is_none() {
        sources.insert(0, root.to_owned());
    }
    let marker = if owner.is_some() {
        OWNER_MIGRATION_MARKER
    } else {
        MIGRATION_MARKER
    };
    match fs::read_to_string(destination.join(marker)) {
        Ok(version) => {
            anyhow::ensure!(version == "1\n", "Unknown personal migration marker");
            if web.is_none() {
                if let Some(owner) = owner {
                    record_owner_root_claim(root, claim.unwrap_or(owner.user_id.get()))?;
                }
                return Ok(destination.to_owned());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination.parent().context("Missing space parent")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let mut session_directories: Vec<_> = sources
        .iter()
        .map(|source| source.join("sessions"))
        .collect();
    if let Some(web) = &web {
        session_directories.push(web.clone());
    }
    session_directories.push(destination.join("sessions"));
    session_directories.sort();
    session_directories.dedup();
    // Keep exclusive ownership until installation so no live store is replaced.
    let mut locks = Vec::new();
    for directory in &session_directories {
        locks.push(crate::agent::session::ownership_lock(directory, true)?);
    }
    let mut dataset_directories: Vec<_> = sources
        .iter()
        .map(|source| source.join("datasets"))
        .collect();
    dataset_directories.push(destination.join("datasets"));
    dataset_directories.sort();
    dataset_directories.dedup();
    for directory in &dataset_directories {
        locks.push(crate::agent::session::ownership_lock(directory, true)?);
    }
    let sqlite_migration = session_directories.iter().try_fold(
        false,
        |present, directory| -> anyhow::Result<bool> {
            Ok(present || directory.join("state.sqlite3").try_exists()?)
        },
    )?;
    for source in &sources {
        let tree = tempfile::tempdir_in(parent)?;
        for name in [
            "memories",
            "skills",
            "sessions",
            "attachments",
            "SOUL.md",
            "companion.json",
            "proactive.json",
            "sandbox.json",
        ] {
            if name == "sessions" && sqlite_migration {
                continue;
            }
            if source.join(name).try_exists()? {
                copy_legacy(&source.join(name), &tree.path().join(name))?;
            }
        }
        for name in ["reminders.json", "wakeups.json"] {
            if !source.join(name).try_exists()? {
                continue;
            }
            if source == root
                && let Some(owner) = owner
            {
                let tasks: Vec<crate::agent::reminders::Reminder> =
                    serde_json::from_slice(&fs::read(source.join(name))?)?;
                let tasks: Vec<_> = tasks
                    .into_iter()
                    .filter(|task| {
                        task.recipient.interface == format!("telegram:{}", owner.interface)
                            && task.recipient.chat == owner.user_id.to_string()
                    })
                    .collect();
                fs::write(tree.path().join(name), serde_json::to_vec_pretty(&tasks)?)?;
            } else {
                copy_legacy(&source.join(name), &tree.path().join(name))?;
            }
        }
        let mut missing = Vec::new();
        plan_merge(tree.path(), staging.path(), &mut missing)?;
        for (source, target) in missing {
            fs::rename(source, target)?;
        }
        let dataset_stage = tempfile::tempdir_in(parent)?;
        crate::agent::datasets::migration::stage_owner_datasets(
            &source.join("datasets"),
            &destination.join("datasets"),
            dataset_stage.path(),
        )
        .with_context(|| {
            format!(
                "Dataset migration from {} to {}",
                source.join("datasets").display(),
                destination.join("datasets").display()
            )
        })?;
        crate::agent::datasets::migration::stage_owner_datasets(
            dataset_stage.path(),
            &staging.path().join("datasets"),
            &staging.path().join("datasets"),
        )?;
    }
    if sqlite_migration {
        let canonical = destination.join("sessions");
        let legacy_root = root.join("sessions");
        // Keep current personal selectors before older per-chat and root selectors.
        session_directories.sort_by_key(|directory| {
            if directory == &canonical {
                0
            } else if directory == &legacy_root {
                2
            } else {
                1
            }
        });
        let database_stage = tempfile::tempdir_in(parent)?;
        crate::agent::session::stage_personal_sessions(
            &session_directories,
            database_stage.path(),
        )?;
        fs::create_dir_all(staging.path().join("sessions"))?;
        fs::rename(
            database_stage.path().join("state.sqlite3"),
            staging.path().join("sessions/state.sqlite3"),
        )?;
    }
    let mut missing = Vec::new();
    plan_merge(staging.path(), destination, &mut missing)?;
    if sqlite_migration {
        crate::agent::session::checkpoint_owner_destination(&destination.join("sessions"))?;
    }
    if staging.path().join("datasets/state.sqlite3").try_exists()? {
        crate::agent::session::checkpoint_owner_destination(&destination.join("datasets"))
            .context("Unable to checkpoint owner dataset destination")?;
    }
    fs::write(staging.path().join(marker), "1\n")?;
    for (source, target) in missing {
        fs::rename(source, target)?;
    }
    if staging.path().exists() {
        fs::rename(staging.path().join(marker), destination.join(marker))?;
    }
    if owner.is_some() {
        fs::write(destination.join(MIGRATION_MARKER), "1\n")?;
    }
    if let Some(root) = web_root {
        fs::write(root.join(WEB_MIGRATION_MARKER), "1\n")?;
    }
    if let Some(owner) = owner {
        record_owner_root_claim(root, claim.unwrap_or(owner.user_id.get()))?;
    }
    Ok(destination.to_owned())
}

fn plan_merge(
    source: &Path,
    destination: &Path,
    missing: &mut Vec<(PathBuf, PathBuf)>,
) -> anyhow::Result<()> {
    let target = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            missing.push((source.to_owned(), destination.to_owned()));
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let original = fs::symlink_metadata(source)?;
    if original.is_dir() && target.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            plan_merge(&entry.path(), &destination.join(entry.file_name()), missing)?;
        }
        return Ok(());
    }
    if original.is_file() && target.is_file() && fs::read(source)? == fs::read(destination)? {
        return Ok(());
    }
    if original.is_file()
        && target.is_file()
        && source
            .file_name()
            .is_some_and(|name| name == "state.sqlite3")
        && source
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "sessions" || name == "datasets")
    {
        // This is the conflict-checked merged snapshot, installed only after the
        // complete filesystem preflight and destination checkpoint have succeeded.
        missing.push((source.to_owned(), destination.to_owned()));
        return Ok(());
    }
    if original.is_symlink()
        && target.is_symlink()
        && fs::read_link(source)? == fs::read_link(destination)?
    {
        return Ok(());
    }
    anyhow::bail!(
        "Owner migration conflict at {}. Back up and reconcile the legacy and space files, then restart; no conflicting files were overwritten",
        destination.display()
    )
}

fn copy_legacy(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_symlink() {
        // Preserve trusted owner links without traversing arbitrary trees during migration.
        let target = fs::read_link(source)?;
        std::os::unix::fs::symlink(
            source.parent().context("Missing parent")?.join(target),
            destination,
        )?;
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if source.file_name().is_some_and(|name| name == "sessions")
                && matches!(
                    entry.file_name().to_str(),
                    Some(
                        "state.sqlite3"
                            | "state.sqlite3-wal"
                            | "state.sqlite3-shm"
                            | "store.lock"
                            | "init.lock"
                    )
                )
            {
                continue;
            }
            copy_legacy(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        fs::copy(source, destination)?;
    } else {
        anyhow::bail!("Cannot migrate special file {}", source.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "Offline migration check against a private temporary data snapshot"]
    fn migration_snapshot_fixture() {
        let root =
            fs::canonicalize(std::env::var_os("KODRIK_MIGRATION_SNAPSHOT").expect("snapshot path"))
                .unwrap();
        let temporary = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            root.starts_with(temporary) || root.starts_with("/private/tmp"),
            "only temporary copies may be migrated by this fixture"
        );
        let config =
            toml::from_str(&fs::read_to_string(root.join("config.toml")).unwrap()).unwrap();
        let destination = owner_data(&root, &config).unwrap();
        println!("MIGRATION_OK={}", destination.display());
    }

    #[tokio::test]
    async fn selector_precedence_never_overwrites_conflicting_session_history() {
        use crate::agent::{
            UserMessage,
            message::Message,
            session::{Session, SessionId, SessionStore, SqliteSessionStore},
        };
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = personal(root.path(), 42);
        for (directory, text) in [
            (root.path(), "old history"),
            (destination.as_path(), "current history"),
        ] {
            let store = SqliteSessionStore::open(directory.join("sessions")).unwrap();
            let mut session = Session::new(SessionId::new("same-session"));
            session.push(Message::User(UserMessage {
                content: text.into(),
                files: vec![],
                images: vec![],
            }));
            store.save(&session).await.unwrap();
        }
        let path = destination.join("sessions/state.sqlite3");
        let before = fs::read(&path).unwrap();
        let error = owner_data(root.path(), &config).unwrap_err();
        assert!(format!("{error:#}").contains("Different records"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!destination.join(OWNER_MIGRATION_MARKER).exists());
    }

    #[tokio::test]
    async fn sqlite_migration_uses_current_personal_selectors_and_preserves_json_backups() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        for personal_done in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
            let legacy = telegram_interface(root.path(), "main").join("users/42");
            let destination = personal(root.path(), 42);
            let conversation = SessionId::new("telegram:main:42");
            let cli = SessionId::new("cli:main");
            let old = Session::new(SessionId::new("old-root-session"));
            let imported = Session::new(SessionId::new("imported-personal-session"));
            let current = Session::new(SessionId::new("current-personal-session"));
            for (directory, session, stale) in [
                (root.path().to_owned(), &old, "old-root-json"),
                (legacy.clone(), &imported, "old-personal-json"),
            ] {
                let store = SqliteSessionStore::open(directory.join("sessions")).unwrap();
                store.save(session).await.unwrap();
                store.select(&conversation, session.id()).await.unwrap();
                if directory == root.path() {
                    store.select(&cli, session.id()).await.unwrap();
                }
                drop(store);
                fs::create_dir_all(directory.join("sessions/active")).unwrap();
                fs::write(
                    directory.join("sessions/active/telegram:main:42.json"),
                    serde_json::to_vec(stale).unwrap(),
                )
                .unwrap();
                fs::write(
                    directory.join("sessions/obsolete.json"),
                    "obsolete imported JSON",
                )
                .unwrap();
            }
            fs::write(legacy.join("SOUL.md"), "legacy profile").unwrap();
            if personal_done {
                let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
                store.save(&imported).await.unwrap();
                store.save(&current).await.unwrap();
                store.select(&conversation, current.id()).await.unwrap();
                drop(store);
                fs::write(destination.join(MIGRATION_MARKER), "1\n").unwrap();
                fs::write(destination.join("SOUL.md"), "updated profile").unwrap();
            }
            owner_data(root.path(), &config).unwrap();
            let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
            assert_eq!(
                store.active_id(&conversation).await.unwrap(),
                *(if personal_done {
                    current.id()
                } else {
                    imported.id()
                })
            );
            assert_eq!(store.active_id(&cli).await.unwrap(), *old.id());
            assert!(store.load(old.id()).await.unwrap().is_some());
            assert!(store.load(imported.id()).await.unwrap().is_some());
            assert_eq!(
                fs::read_to_string(destination.join("SOUL.md")).unwrap(),
                if personal_done {
                    "updated profile"
                } else {
                    "legacy profile"
                }
            );
            assert!(!destination.join("sessions/obsolete.json").exists());
            assert_eq!(
                fs::read_to_string(legacy.join("sessions/obsolete.json")).unwrap(),
                "obsolete imported JSON"
            );
        }
    }

    #[test]
    fn ambiguous_previous_owners_require_reconciliation_instead_of_guessing() {
        let root = tempfile::tempdir().unwrap();
        for user in [7, 42] {
            let directory = root.path().join("users").join(user.to_string());
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(super::OWNER_MIGRATION_MARKER), "1\n").unwrap();
        }
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        assert!(
            super::owner_data(root.path(), &config)
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(!root.path().join(super::OWNER_ROOT_MARKER).exists());
    }

    #[tokio::test]
    async fn changing_owner_never_reassigns_the_retained_legacy_root_backup() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("memories")).unwrap();
        fs::write(
            root.path().join("memories/USER.md"),
            "owner42 private memory",
        )
        .unwrap();
        let local = SqliteSessionStore::open(root.path().join("sessions")).unwrap();
        local
            .save(&Session::new(SessionId::new("owner42-private")))
            .await
            .unwrap();
        drop(local);
        for user in [42, 7] {
            let config: crate::config::Config = toml::from_str(&format!("[owner]\ninterface='main'\nuser_id={user}\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]")).unwrap();
            let directory = super::owner_data(root.path(), &config).unwrap();
            let store = SqliteSessionStore::open(directory.join("sessions")).unwrap();
            assert_eq!(
                store
                    .load(&SessionId::new("owner42-private"))
                    .await
                    .unwrap()
                    .is_some(),
                user == 42
            );
            assert_eq!(directory.join("memories/USER.md").exists(), user == 42);
        }
        assert_eq!(
            fs::read_to_string(root.path().join("memories/USER.md")).unwrap(),
            "owner42 private memory"
        );
    }

    #[test]
    fn existing_owner_markers_in_any_previous_user_space_claim_the_root_backup() {
        for canonical in [false, true] {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("memories")).unwrap();
            fs::write(
                root.path().join("memories/USER.md"),
                "owner42 private memory",
            )
            .unwrap();
            let previous = if canonical {
                root.path().join("users/42")
            } else {
                super::telegram_interface(root.path(), "removed").join("users/42")
            };
            fs::create_dir_all(&previous).unwrap();
            fs::write(previous.join(super::OWNER_MIGRATION_MARKER), "1\n").unwrap();
            let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=7\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
            let directory = super::owner_data(root.path(), &config).unwrap();
            assert!(
                !directory.join("memories/USER.md").exists(),
                "canonical={canonical}"
            );
        }
    }

    #[tokio::test]
    async fn previous_owners_unmigrated_web_archive_never_moves_to_new_owner() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        for canonical in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let previous = if canonical {
                root.path().join("users/42")
            } else {
                super::telegram_interface(root.path(), "removed").join("users/42")
            };
            fs::create_dir_all(&previous).unwrap();
            fs::write(previous.join(super::OWNER_MIGRATION_MARKER), "1\n").unwrap();
            let web = SqliteSessionStore::open(root.path().join("web/chat-sessions")).unwrap();
            web.save(&Session::new(SessionId::new("owner42-web")))
                .await
                .unwrap();
            drop(web);
            for user in [7, 42] {
                let config: crate::config::Config = toml::from_str(&format!("[owner]\ninterface='main'\nuser_id={user}\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]")).unwrap();
                let directory = super::owner_data(root.path(), &config).unwrap();
                let store = SqliteSessionStore::open(directory.join("sessions")).unwrap();
                assert_eq!(
                    store
                        .load(&SessionId::new("owner42-web"))
                        .await
                        .unwrap()
                        .is_some(),
                    user == 42,
                    "canonical={canonical}, user={user}"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_existing_personal_user_inherits_local_history_when_promoted_to_owner() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[gateway]\n[provider]").unwrap();
        let personal = super::personal_data(root.path(), &config, 42).unwrap();
        for (directory, id) in [
            (root.path().join("sessions"), "local"),
            (personal.join("sessions"), "telegram"),
        ] {
            let store = SqliteSessionStore::open(directory).unwrap();
            store.save(&Session::new(SessionId::new(id))).await.unwrap();
        }
        let owner: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = super::owner_data(root.path(), &owner).unwrap();
        let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn local_owner_web_migration_is_not_replayed_after_configuring_telegram_owner() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[gateway]\n[provider]").unwrap();
        let web = SqliteSessionStore::open(root.path().join("web/chat-sessions")).unwrap();
        web.save(&Session::new(SessionId::new("deleted-web")))
            .await
            .unwrap();
        drop(web);
        assert_eq!(
            super::owner_data(root.path(), &config).unwrap(),
            root.path()
        );
        let db = rusqlite::Connection::open(root.path().join("sessions/state.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.execute_batch("DELETE FROM sessions").unwrap();
        drop(db);
        let owner: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = super::owner_data(root.path(), &owner).unwrap();
        let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn personal_migration_merges_old_interfaces_and_leaves_conflicts_untouched() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[gateway]\n[provider]").unwrap();
        for key in ["main", "other"] {
            let directory = super::telegram_interface(root.path(), key).join("users/7");
            let store = SqliteSessionStore::open(directory.join("sessions")).unwrap();
            store
                .save(&Session::new(SessionId::new(format!("telegram:{key}:7"))))
                .await
                .unwrap();
            fs::write(directory.join("SOUL.md"), key).unwrap();
            fs::write(directory.join("sandbox.json"), "saved virtual files").unwrap();
        }
        let destination = root.path().join("users/7");
        assert!(super::personal_data(root.path(), &config, 7).is_err());
        assert!(!destination.join("SOUL.md").exists());
        assert!(!destination.join("sessions/state.sqlite3").exists());
        assert!(!destination.join(super::MIGRATION_MARKER).exists());
        let other = super::telegram_interface(root.path(), "other").join("users/7");
        fs::write(other.join("SOUL.md"), "main").unwrap();
        super::personal_data(root.path(), &config, 7).unwrap();
        let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        assert_eq!(store.list().await.unwrap().len(), 2);
        assert_eq!(
            fs::read_to_string(destination.join("sandbox.json")).unwrap(),
            "saved virtual files"
        );
        assert!(other.join("sessions/state.sqlite3").exists());
    }

    #[tokio::test]
    async fn canonical_owner_migration_uses_current_telegram_data_and_imports_web_metadata_once() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let legacy = super::telegram_interface(root.path(), "main").join("users/42");
        for (path, id) in [
            (root.path().join("sessions"), "stale"),
            (legacy.join("sessions"), "current"),
            (root.path().join("web/chat-sessions"), "web"),
        ] {
            let store = SqliteSessionStore::open(path).unwrap();
            store.save(&Session::new(SessionId::new(id))).await.unwrap();
        }
        let attachment = crate::agent::file::FileAttachment::save(
            &root.path().join("web/chat-attachments"),
            "note.txt",
            None,
            b"saved file",
        )
        .unwrap();
        let web = SqliteSessionStore::open(root.path().join("web/chat-sessions")).unwrap();
        let mut session = Session::new(SessionId::new("web"));
        session.push(crate::agent::message::Message::User(
            crate::agent::UserMessage {
                content: "saved input".into(),
                images: vec![],
                files: vec![attachment.clone()],
            },
        ));
        session.set_compaction(1, "saved summary".into());
        web.save(&session).await.unwrap();
        web.save(&Session::new(SessionId::new("web-branch")))
            .await
            .unwrap();
        drop(web);
        fs::write(legacy.join(".owner-migration-complete"), "1\n").unwrap();
        let db = rusqlite::Connection::open(root.path().join("web/chat-sessions/state.sqlite3"))
            .unwrap();
        db.execute_batch("CREATE TABLE web_chat_titles(conversation_id TEXT PRIMARY KEY,title TEXT NOT NULL); INSERT INTO web_chat_titles VALUES('web','Saved title'); CREATE TABLE web_chat_pins(root_id TEXT PRIMARY KEY); INSERT INTO web_chat_pins VALUES('web'); CREATE TABLE web_chat_branches(session_id TEXT PRIMARY KEY,root_id TEXT NOT NULL,parent_id TEXT NOT NULL,fork_index INTEGER NOT NULL); CREATE TABLE web_chat_selection(root_id TEXT PRIMARY KEY,session_id TEXT NOT NULL); INSERT INTO web_chat_selection VALUES('web','web');").unwrap();
        db.execute_batch("INSERT INTO web_chat_branches VALUES('web-branch','web','web',0); UPDATE web_chat_selection SET session_id='web-branch'; INSERT INTO runs VALUES('saved-run','web','web',1,1,X'01',NULL); INSERT INTO deliveries VALUES('saved-delivery','saved-run','web',X'02'); INSERT INTO compaction_jobs VALUES('web','saved-compaction','saved-run','Complete',X'03');").unwrap();
        drop(db);
        let destination = super::owner_data(root.path(), &config).unwrap();
        assert_eq!(destination, root.path().join("users/42"));
        let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 3);
        let saved = store.load(&SessionId::new("web")).await.unwrap().unwrap();
        assert_eq!(saved.compaction(), Some((1, "saved summary")));
        let crate::agent::message::Message::User(user) = &saved.messages()[0] else {
            panic!("expected saved input")
        };
        assert_eq!(user.files[0].path, attachment.path);
        assert_eq!(user.files[0].bytes().await.unwrap(), b"saved file");
        assert!(
            store
                .load(&SessionId::new("stale"))
                .await
                .unwrap()
                .is_none()
        );
        drop(store);
        let db = rusqlite::Connection::open(destination.join("sessions/state.sqlite3")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT title FROM web_chat_titles WHERE conversation_id='web'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "Saved title"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM web_chat_pins", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT session_id FROM web_chat_selection WHERE root_id='web'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "web-branch"
        );
        assert_eq!(
            db.query_row(
                "SELECT parent_id FROM web_chat_branches WHERE session_id='web-branch'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "web"
        );
        for (table, payload) in [
            ("runs", vec![1]),
            ("deliveries", vec![2]),
            ("compaction_jobs", vec![3]),
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT payload FROM {table}"), [], |r| r
                    .get::<_, Vec<u8>>(0))
                    .unwrap(),
                payload
            );
        }
        db.execute_batch("DELETE FROM web_chat_pins; DELETE FROM web_chat_titles; DELETE FROM web_chat_selection; DELETE FROM web_chat_branches; DELETE FROM compaction_jobs; DELETE FROM deliveries; DELETE FROM runs; DELETE FROM sessions WHERE id='web';").unwrap();
        drop(db);
        super::owner_data(root.path(), &config).unwrap();
        let store = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        assert!(store.load(&SessionId::new("web")).await.unwrap().is_none());
        assert!(legacy.join("sessions/state.sqlite3").exists());
        assert!(root.path().join("web/chat-sessions/state.sqlite3").exists());
    }

    fn dataset_record(directory: &std::path::Path, kcal: f64) {
        use crate::agent::datasets::DatasetStore;
        use serde_json::json;
        let store = DatasetStore::open(directory.to_owned()).unwrap();
        for request in [
            json!({"action":"create","dataset":"meals","title":"Meals","fields":[{"name":"kcal","type":"number","required":true}]}),
            json!({"action":"upsert","dataset":"meals","request_id":"a9d91f12-bd17-47c2-98f7-5a8aa71b04f9",
                "records":[{"id":"breakfast","expected_revision":null,"values":[{"field":"kcal","value":kcal}]}]}),
        ] {
            store
                .execute(serde_json::from_value(request).unwrap())
                .unwrap();
        }
    }

    #[test]
    fn owner_migration_carries_dataset_wal_and_preserves_the_source() {
        use crate::agent::datasets::DatasetStore;
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        dataset_record(&root.path().join("datasets"), 350.0);
        let source =
            rusqlite::Connection::open(root.path().join("datasets/state.sqlite3")).unwrap();
        source.execute_batch("PRAGMA wal_autocheckpoint=0; UPDATE records SET values_json='{\"kcal\":450.0}',revision=2; UPDATE datasets SET revision=2;").unwrap();
        assert!(root.path().join("datasets/state.sqlite3-wal").exists());
        let destination = super::owner_data(root.path(), &config).unwrap();
        let store = DatasetStore::open(destination.join("datasets")).unwrap();
        let result = store
            .execute(serde_json::from_value(json!({"action":"query","dataset":"meals"})).unwrap())
            .unwrap();
        assert_eq!(result["records"][0]["values"]["kcal"], 450.0);
        assert_eq!(result["records"][0]["revision"], 2);
        assert!(root.path().join("datasets/state.sqlite3").exists());
        drop(store);
        fs::remove_file(destination.join(super::OWNER_MIGRATION_MARKER)).unwrap();
        // An interrupted filesystem transfer can be retried with identical data.
        super::owner_data(root.path(), &config).unwrap();
    }

    #[test]
    fn owner_migration_ignores_initialized_empty_dataset_stores() {
        use crate::agent::datasets::DatasetStore;
        use serde_json::json;
        for populated in ["root", "legacy", "destination"] {
            let root = tempfile::tempdir().unwrap();
            let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
            let destination = personal(root.path(), 42);
            let legacy = telegram_interface(root.path(), "main").join("users/42");
            for (name, directory) in [
                ("root", root.path()),
                ("legacy", legacy.as_path()),
                ("destination", destination.as_path()),
            ] {
                drop(DatasetStore::open(directory.join("datasets")).unwrap());
                if name == populated {
                    dataset_record(&directory.join("datasets"), 350.0);
                }
            }
            if populated != "legacy" {
                fs::write(destination.join(MIGRATION_MARKER), "1\n").unwrap();
            }
            fs::write(root.path().join(WEB_MIGRATION_MARKER), "1\n").unwrap();
            owner_data(root.path(), &config).unwrap();
            let store = DatasetStore::open(destination.join("datasets")).unwrap();
            let data = store
                .execute(
                    serde_json::from_value(json!({"action":"query","dataset":"meals"})).unwrap(),
                )
                .unwrap();
            assert_eq!(data["records"][0]["values"]["kcal"], 350.0, "{populated}");
            assert_eq!(data["records"][0]["revision"], 1, "{populated}");
            drop(store);
            // Completed migration can be retried without importing again.
            owner_data(root.path(), &config).unwrap();
        }
    }

    #[test]
    fn dataset_destination_with_receipts_is_not_empty() {
        use crate::agent::datasets::DatasetStore;
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        dataset_record(&root.path().join("datasets"), 350.0);
        let destination = personal(root.path(), 42);
        drop(DatasetStore::open(destination.join("datasets")).unwrap());
        let path = destination.join("datasets/state.sqlite3");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute("INSERT INTO receipts VALUES('saved-request','{}','{}')", [])
            .unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        let error = owner_data(root.path(), &config).unwrap_err();
        assert!(format!("{error:#}").contains("Owner dataset migration conflict"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!destination.join(OWNER_MIGRATION_MARKER).exists());
    }

    #[test]
    fn dataset_migration_refuses_active_writers_and_conflicting_data() {
        use crate::agent::datasets::DatasetStore;
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        dataset_record(&root.path().join("datasets"), 350.0);
        let open = DatasetStore::open(root.path().join("datasets")).unwrap();
        let destination = super::telegram(root.path(), "main", 42);
        assert!(super::owner_data(root.path(), &config).is_err());
        assert!(!destination.join(super::MIGRATION_MARKER).exists());
        drop(open);
        dataset_record(&destination.join("datasets"), 650.0);
        let before = fs::read(destination.join("datasets/state.sqlite3")).unwrap();
        assert!(super::owner_data(root.path(), &config).is_err());
        assert_eq!(
            fs::read(destination.join("datasets/state.sqlite3")).unwrap(),
            before
        );
        assert!(!destination.join(super::MIGRATION_MARKER).exists());
    }

    #[test]
    fn dataset_migration_refuses_orphan_sqlite_sidecars() {
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        for orphan_in_destination in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let destination = super::telegram(root.path(), "main", 42);
            dataset_record(&root.path().join("datasets"), 350.0);
            let directory = if orphan_in_destination {
                destination.join("datasets")
            } else {
                root.path().join("datasets")
            };
            if orphan_in_destination {
                dataset_record(&directory, 650.0);
            }
            let connection = rusqlite::Connection::open(directory.join("state.sqlite3")).unwrap();
            connection.execute_batch("PRAGMA wal_autocheckpoint=0; UPDATE records SET values_json='{\"kcal\":999.0}';").unwrap();
            let wal = directory.join("state.sqlite3-wal");
            let evidence = fs::read(&wal).unwrap();
            assert!(!evidence.is_empty());
            fs::remove_file(directory.join("state.sqlite3")).unwrap();
            assert!(
                super::owner_data(root.path(), &config).is_err(),
                "orphan destination={orphan_in_destination}"
            );
            assert_eq!(fs::read(&wal).unwrap(), evidence);
            assert!(!destination.join(super::MIGRATION_MARKER).exists());
            assert!(!directory.join("state.sqlite3").exists());
        }
    }

    #[test]
    fn owner_migration_locks_legacy_stores_before_first_sqlite_open() {
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        std::fs::write(
            root.path().join("sessions/legacy.json"),
            r#"{"id":"legacy","messages":[]}"#,
        )
        .unwrap();
        let destination = super::telegram(root.path(), "main", 42);
        // First open holds shared ownership before it creates the main database.
        let opening =
            crate::agent::session::ownership_lock(&destination.join("sessions"), false).unwrap();
        let result = super::owner_data(root.path(), &config);
        assert!(result.is_err());
        assert!(!destination.join(super::MIGRATION_MARKER).exists());
        drop(opening);
        super::owner_data(root.path(), &config).unwrap();
    }

    #[tokio::test]
    async fn owner_migration_retries_incomplete_import_and_merges_json_into_sqlite() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = super::telegram(root.path(), "main", 42);
        let target = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        target
            .save(&Session::new(SessionId::new("isolated")))
            .await
            .unwrap();
        drop(target);
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        std::fs::write(root.path().join("sessions/source.json"), "broken").unwrap();
        assert!(SqliteSessionStore::open(root.path().join("sessions")).is_err());
        assert!(super::owner_data(root.path(), &config).is_err());
        std::fs::write(
            root.path().join("sessions/source.json"),
            r#"{"id":"legacy","messages":[]}"#,
        )
        .unwrap();
        super::owner_data(root.path(), &config).unwrap();
        let merged = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        assert_eq!(merged.list().await.unwrap().len(), 2);
        assert!(
            merged
                .load(&SessionId::new("legacy"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn owner_migration_merges_sqlite_wal_history_and_keeps_sources() {
        use crate::agent::session::{Session, SessionId, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = super::telegram(root.path(), "main", 42);
        let source = SqliteSessionStore::open(root.path().join("sessions")).unwrap();
        source
            .save(&Session::new(SessionId::new("legacy")))
            .await
            .unwrap();
        let target = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        target
            .save(&Session::new(SessionId::new("isolated")))
            .await
            .unwrap();
        // An open writer must not lose writes to a renamed database.
        assert!(super::owner_data(root.path(), &config).is_err());
        drop(source);
        drop(target);
        // Keep committed source changes in WAL, as after an unclean shutdown.
        let source_db =
            rusqlite::Connection::open(root.path().join("sessions/state.sqlite3")).unwrap();
        source_db
            .execute_batch("PRAGMA wal_autocheckpoint=0")
            .unwrap();
        source_db
            .execute(
                "INSERT INTO sessions VALUES ('wal-only', ?1, 1)",
                [br#"{"id":"wal-only","messages":[]}"#.as_slice()],
            )
            .unwrap();
        super::owner_data(root.path(), &config).unwrap();
        let merged = SqliteSessionStore::open(destination.join("sessions")).unwrap();
        assert_eq!(merged.list().await.unwrap().len(), 3);
        assert!(
            merged
                .load(&SessionId::new("wal-only"))
                .await
                .unwrap()
                .is_some()
        );
        let source = SqliteSessionStore::open(root.path().join("sessions")).unwrap();
        assert_eq!(source.list().await.unwrap().len(), 2);
        super::owner_data(root.path(), &config).unwrap();
        assert_eq!(merged.list().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn owner_migration_sqlite_conflict_changes_neither_store() {
        use crate::agent::session::{Session, SessionStore, SqliteSessionStore};
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = super::telegram(root.path(), "main", 42);
        for (path, content) in [
            (root.path().to_path_buf(), "legacy"),
            (destination.clone(), "isolated"),
        ] {
            let store = SqliteSessionStore::open(path.join("sessions")).unwrap();
            let session: Session = serde_json::from_value(
                serde_json::json!({"id":"same", "messages":[{"User":{"content":content}}]}),
            )
            .unwrap();
            store.save(&session).await.unwrap();
        }
        let before = std::fs::read(destination.join("sessions/state.sqlite3")).unwrap();
        assert!(super::owner_data(root.path(), &config).is_err());
        assert_eq!(
            std::fs::read(destination.join("sessions/state.sqlite3")).unwrap(),
            before
        );
        assert!(!destination.join(super::MIGRATION_MARKER).exists());
    }

    #[test]
    fn discovers_only_canonical_chat_directories_for_this_interface() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            super::telegram_chats(root.path(), "main")
                .unwrap()
                .is_empty()
        );
        for chat in [7, -42] {
            std::fs::create_dir_all(super::telegram(root.path(), "main", chat)).unwrap();
        }
        std::fs::create_dir_all(super::telegram(root.path(), "other", 8)).unwrap();
        let path = super::telegram(root.path(), "main", 7);
        let users = path.parent().unwrap();
        for name in ["0", "-9", "007", "invalid"] {
            std::fs::create_dir_all(users.join(name)).unwrap();
        }
        std::fs::write(users.join("12"), "not a directory").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(super::telegram(root.path(), "other", 8), users.join("13"))
            .unwrap();
        assert_eq!(
            super::telegram_chats(root.path(), "main").unwrap(),
            vec![-42, 7, 8]
        );
    }
    use super::*;

    #[test]
    fn space_paths_separate_users_groups_and_interfaces() {
        let root = Path::new("/data");
        assert_ne!(telegram(root, "main", 42), telegram(root, "main", -42));
        assert_ne!(telegram(root, "main", -42), telegram(root, "other", -42));
        assert!(telegram(root, "../../outside", -42).starts_with(root.join("spaces/telegram")));
    }

    #[test]
    fn migration_into_existing_space_preserves_both_nonconflicting_trees() {
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = telegram(root.path(), "main", 42);
        fs::create_dir_all(destination.join("sessions")).unwrap();
        fs::write(
            destination.join("sessions/isolated.json"),
            "isolated history",
        )
        .unwrap();
        fs::create_dir_all(root.path().join("sessions")).unwrap();
        fs::write(root.path().join("sessions/legacy.json"), "legacy history").unwrap();
        fs::write(root.path().join("SOUL.md"), "legacy personality").unwrap();
        // A previous interrupted attempt may already have installed identical files.
        fs::write(destination.join("SOUL.md"), "legacy personality").unwrap();
        owner_data(root.path(), &config).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("sessions/legacy.json")).unwrap(),
            "legacy history"
        );
        assert_eq!(
            fs::read_to_string(destination.join("sessions/isolated.json")).unwrap(),
            "isolated history"
        );
        fs::write(destination.join("SOUL.md"), "updated personality").unwrap();
        owner_data(root.path(), &config).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("SOUL.md")).unwrap(),
            "updated personality"
        );
    }

    #[test]
    fn migration_conflict_leaves_data_intact_and_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let destination = telegram(root.path(), "main", 42);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("SOUL.md"), "isolated personality").unwrap();
        fs::write(root.path().join("SOUL.md"), "legacy personality").unwrap();
        fs::create_dir_all(root.path().join("memories")).unwrap();
        fs::write(root.path().join("memories/USER.md"), "legacy memory").unwrap();
        let error = owner_data(root.path(), &config).unwrap_err();
        assert!(error.to_string().contains("SOUL.md"));
        assert!(!destination.join("memories").exists());
        assert_eq!(
            fs::read_to_string(destination.join("SOUL.md")).unwrap(),
            "isolated personality"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("SOUL.md")).unwrap(),
            "legacy personality"
        );
        fs::rename(
            destination.join("SOUL.md"),
            destination.join("SOUL.backup.md"),
        )
        .unwrap();
        owner_data(root.path(), &config).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("memories/USER.md")).unwrap(),
            "legacy memory"
        );
    }

    #[test]
    fn legacy_state_is_copied_once_only_to_owner() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("memories")).unwrap();
        std::fs::write(root.path().join("memories/USER.md"), "PRIVATE").unwrap();
        std::fs::write(root.path().join("config.toml"), "TOKEN").unwrap();
        let config: crate::config::Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        let tasks: Vec<_> = [42, -42, 7].into_iter().map(|chat| serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(), "recipient":{"interface":"telegram:main", "chat":chat.to_string()},
            "text":"legacy private task", "due_at":"2026-10-13T10:00:00Z", "status":"pending", "retry_at":null, "last_error":null,
            "source_session":"legacy"
        })).collect();
        for name in ["reminders.json", "wakeups.json"] {
            std::fs::write(root.path().join(name), serde_json::to_vec(&tasks).unwrap()).unwrap();
        }
        let path = owner_data(root.path(), &config).unwrap();
        for name in ["reminders.json", "wakeups.json"] {
            let migrated: Vec<crate::agent::reminders::Reminder> =
                serde_json::from_slice(&std::fs::read(path.join(name)).unwrap()).unwrap();
            assert_eq!(migrated.len(), 1);
            assert_eq!(migrated[0].recipient.chat, "42");
            let original: Vec<crate::agent::reminders::Reminder> =
                serde_json::from_slice(&std::fs::read(root.path().join(name)).unwrap()).unwrap();
            assert_eq!(original.len(), 3);
        }
        assert_eq!(
            std::fs::read_to_string(path.join("memories/USER.md")).unwrap(),
            "PRIVATE"
        );
        assert!(!path.join("config.toml").exists());
        assert!(!telegram(root.path(), "main", -42).exists());
        std::fs::write(path.join("memories/USER.md"), "UPDATED").unwrap();
        owner_data(root.path(), &config).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("memories/USER.md")).unwrap(),
            "UPDATED"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("memories/USER.md")).unwrap(),
            "PRIVATE"
        );
    }
}
