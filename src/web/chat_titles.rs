use super::Chat;
use crate::agent::session::{SessionId, SqliteSessionStore};
use std::collections::HashMap;

pub(super) async fn init(store: &SqliteSessionStore) -> anyhow::Result<()> {
    store.init_titles().await
}

impl Chat {
    pub(super) async fn titles_revision(&self) -> anyhow::Result<i64> {
        self.sessions.with_connection(|db| {
            Ok(db.query_row("SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key='web_titles_revision'", [], |row| row.get(0))?)
        }).await
    }

    pub(super) async fn generated_title(&self, id: &str) -> anyhow::Result<Option<String>> {
        let id = self.root_id(id).await?;
        self.sessions.saved_title(&id).await
    }

    pub(super) async fn conversation_titles(&self) -> anyhow::Result<HashMap<String, String>> {
        self.sessions
            .with_connection(|db| {
                let mut statement =
                    db.prepare("SELECT conversation_id,title FROM web_chat_titles")?;
                Ok(statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?)
            })
            .await
    }

    pub(super) async fn start_title(&self, id: &SessionId) {
        match self.generated_title(id.as_str()).await {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(error) => {
                log::warn!("Conversation title read failed: {error:#}");
                return;
            }
        }
        let root = match self.root_id(id.as_str()).await {
            Ok(root) => root,
            Err(error) => {
                log::warn!("Conversation title root read failed: {error:#}");
                return;
            }
        };
        if !self.title_jobs.lock().await.insert(root.clone()) {
            return;
        }
        if let Err(error) = self.notify_title_change().await {
            self.title_jobs.lock().await.remove(&root);
            log::warn!("Conversation title notification failed: {error:#}");
            return;
        }
        let chat = self.clone();
        let id = id.clone();
        tokio::spawn(async move {
            if let Err(error) = chat.generate_title(&id).await {
                log::warn!("Conversation title failed for {}: {error:#}", id.as_str());
            }
            chat.title_jobs.lock().await.remove(&root);
            if let Err(error) = chat.notify_title_change().await {
                log::warn!("Conversation title notification failed: {error:#}");
            }
        });
    }

    async fn notify_title_change(&self) -> anyhow::Result<()> {
        self.sessions.with_connection(|db| {
            db.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')", [])?;
            Ok(())
        }).await
    }

    async fn generate_title(&self, id: &SessionId) -> anyhow::Result<()> {
        self.runner.generate_saved_title(id).await
    }
}
