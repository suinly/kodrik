use crate::{bootstrap::AppContext, config::Config};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub(crate) struct UserSpaces {
    config: Config,
    root: PathBuf,
    owner_app: AppContext,
    apps: tokio::sync::Mutex<HashMap<u64, AppContext>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn discovers_only_canonical_saved_personal_users() {
        let root = tempfile::tempdir().unwrap();
        for user in [7, 8, 9] {
            crate::web::auth::AuthStore::new(root.path())
                .grant_test_user(user)
                .unwrap();
        }
        let config: Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'").unwrap();
        let owner = crate::bootstrap::build_app_at(&config, &root.path().join("owner"), false)
            .await
            .unwrap();
        let users = UserSpaces::new(&config, root.path(), owner);
        assert!(users.saved_users().await.unwrap().is_empty());
        assert!(users.is_owner(42));
        assert!(!users.is_owner(7));
        for name in ["7", "42", "0", "007", "-8", "guest-7", "invalid", "10"] {
            tokio::fs::create_dir_all(root.path().join("users").join(name))
                .await
                .unwrap();
        }
        tokio::fs::write(root.path().join("users/8"), "file")
            .await
            .unwrap();
        std::os::unix::fs::symlink(root.path().join("users/7"), root.path().join("users/9"))
            .unwrap();
        assert_eq!(users.saved_users().await.unwrap(), vec![7, 42]);
    }

    #[tokio::test]
    async fn all_user_apps_follow_owner_shutdown_including_users_opened_afterward() {
        let root = tempfile::tempdir().unwrap();
        for user in [7, 8, 9] {
            crate::web::auth::AuthStore::new(root.path())
                .grant_test_user(user)
                .unwrap();
        }
        let config: Config = toml::from_str(
            "[gateway]\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'",
        )
        .unwrap();
        let owner = crate::bootstrap::build_app_at(&config, &root.path().join("owner"), false)
            .await
            .unwrap();
        let users = UserSpaces::new(&config, root.path(), owner.clone());
        let first = users.app(7).await.unwrap();
        let second = users.app(8).await.unwrap();
        let mut first_shutdown = first.agent_runner.shutdown_receiver();
        let mut second_shutdown = second.agent_runner.shutdown_receiver();
        assert!(!*first_shutdown.borrow());
        owner.agent_runner.suspend();
        for receiver in [&mut first_shutdown, &mut second_shutdown] {
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                receiver.wait_for(|stopped| *stopped),
            )
            .await
            .expect("user app must stop with owner")
            .unwrap();
        }
        let late = users.app(9).await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            late.agent_runner
                .shutdown_receiver()
                .wait_for(|stopped| *stopped),
        )
        .await
        .expect("late user app must stop too")
        .unwrap();
    }

    #[tokio::test]
    async fn personal_apps_are_shared_per_user_and_only_owner_reuses_privileged_runner() {
        let root = tempfile::tempdir().unwrap();
        for user in [7, 8, 9] {
            crate::web::auth::AuthStore::new(root.path())
                .grant_test_user(user)
                .unwrap();
        }
        let config: Config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'").unwrap();
        let owner = crate::bootstrap::build_app_at(
            &config,
            &crate::spaces::personal(root.path(), 42),
            false,
        )
        .await
        .unwrap();
        let users = UserSpaces::new(&config, root.path(), owner.clone());
        assert_eq!(users.directory(7), root.path().join("users/7"));
        assert!(Arc::ptr_eq(
            &users.app(42).await.unwrap().agent_runner,
            &owner.agent_runner
        ));
        let (first, second) = tokio::join!(users.app(7), users.app(7));
        let first = first.unwrap();
        assert!(Arc::ptr_eq(
            &first.agent_runner,
            &second.unwrap().agent_runner
        ));
        assert!(!Arc::ptr_eq(&first.agent_runner, &owner.agent_runner));
        assert!(!Arc::ptr_eq(
            &first.agent_runner,
            &users.app(8).await.unwrap().agent_runner
        ));
        assert!(users.app(0).await.is_err());
        assert!(users.app(10).await.is_err());
        assert!(!root.path().join("users/10").exists());
        crate::web::auth::AuthStore::new(root.path())
            .revoke_user(7)
            .unwrap();
        assert!(
            users.app(7).await.is_err(),
            "cached apps must require current access"
        );
    }
}

impl UserSpaces {
    pub(crate) fn new(config: &Config, root: &Path, owner_app: AppContext) -> Self {
        Self {
            config: config.clone(),
            root: root.to_owned(),
            owner_app,
            apps: Default::default(),
        }
    }

    pub(crate) fn directory(&self, user_id: u64) -> PathBuf {
        crate::spaces::personal(&self.root, user_id)
    }

    pub(crate) fn is_owner(&self, user_id: u64) -> bool {
        self.config
            .owner
            .as_ref()
            .is_some_and(|owner| owner.user_id.get() == user_id)
    }

    pub(crate) fn allowed_user(&self, user_id: u64) -> anyhow::Result<bool> {
        Ok(self.is_owner(user_id)
            || crate::web::auth::AuthStore::new(&self.root).allowed_user(user_id)?)
    }

    pub(crate) async fn saved_users(&self) -> anyhow::Result<Vec<u64>> {
        let mut entries = match tokio::fs::read_dir(self.root.join("users")).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut users = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Ok(id) = name.parse::<u64>()
                && id > 0
                && name == id.to_string()
                && self.allowed_user(id)?
            {
                users.push(id);
            }
        }
        users.sort_unstable();
        Ok(users)
    }

    pub(crate) async fn app(&self, user_id: u64) -> anyhow::Result<AppContext> {
        anyhow::ensure!(user_id > 0, "Invalid Telegram user ID 0");
        anyhow::ensure!(self.allowed_user(user_id)?, "Access requires an invitation");
        if self.is_owner(user_id) {
            return Ok(self.owner_app.clone());
        }
        let mut apps = self.apps.lock().await;
        if let Some(app) = apps.get(&user_id) {
            return Ok(app.clone());
        }
        let root = self.root.clone();
        let config = self.config.clone();
        let directory = tokio::task::spawn_blocking(move || {
            crate::spaces::personal_data(&root, &config, user_id)
        })
        .await??;
        let app = crate::bootstrap::build_app_with_access(
            &self.config,
            &directory,
            true,
            Some((crate::web::auth::AuthStore::new(&self.root), user_id)),
        )
        .await?;
        let mut shutdown = self.owner_app.agent_runner.shutdown_receiver();
        let runner = app.agent_runner.clone();
        tokio::spawn(async move {
            if shutdown.wait_for(|stopped| *stopped).await.is_ok() {
                runner.suspend();
            }
        });
        apps.insert(user_id, app.clone());
        Ok(app)
    }
}
