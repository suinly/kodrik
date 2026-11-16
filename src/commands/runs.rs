use crate::i18n::Language;
use crate::{agent::session::SqliteSessionStore, config::Config};
use anyhow::Context;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Store {
    Local,
    Owner,
    Web,
}
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Select the exact store; IDs never trigger a search in other spaces
    #[arg(long, value_enum)]
    pub store: Store,
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    List,
    Show { id: String },
    Retry { id: String },
    Cancel { id: String },
}

fn select_store(root: &Path, config: &Config, store: Store) -> anyhow::Result<PathBuf> {
    let language = Language::from_environment();
    match store {
        Store::Local => {
            anyhow::ensure!(
                config.owner.is_none(),
                language.pick(
                    "Owner configured; use --store owner instead of legacy local data",
                    "Настроен владелец; используйте --store owner вместо прежних локальных данных"
                )
            );
            Ok(root.join("sessions"))
        }
        Store::Web => select_store(
            root,
            config,
            if config.owner.is_some() {
                Store::Owner
            } else {
                Store::Local
            },
        ),
        Store::Owner => {
            config.validate_owner()?;
            let owner = config
                .owner
                .as_ref()
                .context(language.pick("No owner configured", "Владелец не настроен"))?;
            let data = crate::spaces::personal(root, owner.user_id.get());
            anyhow::ensure!(
                std::fs::read_to_string(data.join(crate::spaces::OWNER_MIGRATION_MARKER))
                    .ok()
                    .as_deref()
                    == Some("1\n"),
                language.pick(
                    "Owner migration is incomplete; start the application first",
                    "Миграция владельца не завершена; сначала запустите приложение"
                )
            );
            Ok(data.join("sessions"))
        }
    }
}
pub async fn run(config: &Config, args: Args) -> anyhow::Result<()> {
    let language = Language::from_environment();
    let root = crate::config::data_dir()?;
    let path = select_store(&root, config, args.store)?;
    // Administrative inspection must not create a new empty store or traverse a space symlink.
    let mut current = root.clone();
    for component in path.strip_prefix(&root)?.components() {
        current.push(component);
        anyhow::ensure!(
            std::fs::symlink_metadata(&current)?.is_dir(),
            language.pick("Invalid store directory", "Некорректный каталог хранилища")
        );
    }
    anyhow::ensure!(
        path.join("state.sqlite3").is_file(),
        language.pick(
            "Selected store has no database",
            "В выбранном хранилище нет базы данных"
        )
    );
    let store = SqliteSessionStore::open_async(path).await?;
    match args.command {
        Command::List => println!(
            "{}",
            serde_json::to_string_pretty(&store.run_diagnostics(None).await?)?
        ),
        Command::Show { id } => {
            let rows = store.run_diagnostics(Some(&id)).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(rows.first().context(language.pick(
                    "Run not found in selected store",
                    "Запуск не найден в выбранном хранилище"
                ))?)?
            );
        }
        Command::Retry { id } => {
            store.retry_parked(&id).await?;
            println!("{}", language.pick("Retry admitted; normal recovery will recheck access and replay policy.", "Повтор разрешён; при восстановлении будут заново проверены доступ и политика повторного выполнения."));
        }
        Command::Cancel { id } => {
            store.cancel_run(&id).await?;
            println!(
                "{}",
                language.pick("Cancellation saved.", "Отмена сохранена.")
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runs_selector_never_guesses_another_store() {
        let root = tempfile::tempdir().unwrap();
        let config: crate::config::Config = toml::from_str("[gateway]\n[provider]").unwrap();
        assert_eq!(
            select_store(root.path(), &config, Store::Local).unwrap(),
            root.path().join("sessions")
        );
        assert!(select_store(root.path(), &config, Store::Owner).is_err());
        assert_eq!(
            select_store(root.path(), &config, Store::Web).unwrap(),
            root.path().join("sessions")
        );
        let owner:crate::config::Config=toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='secret'\n[provider]").unwrap();
        assert!(select_store(root.path(), &owner, Store::Local).is_err());
        assert!(select_store(root.path(), &owner, Store::Owner).is_err());
        assert!(select_store(root.path(), &owner, Store::Web).is_err());
        let canonical = root.path().join("users/42");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join(".owner-migration-complete"), "1\n").unwrap();
        assert_eq!(
            select_store(root.path(), &owner, Store::Owner).unwrap(),
            canonical.join("sessions")
        );
        assert_eq!(
            select_store(root.path(), &owner, Store::Web).unwrap(),
            canonical.join("sessions")
        );
    }
}
