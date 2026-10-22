use super::SqliteSessionStore;
use rusqlite::OptionalExtension;

impl SqliteSessionStore {
    pub(crate) async fn init_titles(&self) -> anyhow::Result<()> {
        self.with_connection(|db| {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS web_chat_titles (
                    conversation_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                    title TEXT NOT NULL
                ); INSERT OR IGNORE INTO schema_meta VALUES('web_titles_revision','0');",
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn title_root(&self, id: &str) -> anyhow::Result<String> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            let branches: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='web_chat_branches')", [], |row| row.get(0))?;
            if !branches { return Ok(id); }
            Ok(db.query_row("SELECT root_id FROM web_chat_branches WHERE session_id=?1", [&id], |row| row.get(0)).optional()?.unwrap_or(id))
        }).await
    }

    pub(crate) async fn saved_title(&self, root: &str) -> anyhow::Result<Option<String>> {
        let root = root.to_owned();
        self.with_connection(move |db| {
            Ok(db
                .query_row(
                    "SELECT title FROM web_chat_titles WHERE conversation_id=?1",
                    [root],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
    }

    pub(crate) async fn save_generated_title(
        &self,
        root: String,
        title: String,
    ) -> anyhow::Result<()> {
        // Metadata must never overwrite a newer agent checkpoint or a manual title.
        self.with_connection(move |db| {
            let tx = db.transaction()?;
            if tx.execute("INSERT OR IGNORE INTO web_chat_titles SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM sessions WHERE id=?1)", rusqlite::params![root,title])? != 0 {
                tx.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')", [])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }
}
