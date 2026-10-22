use std::{
    fs,
    io::{ErrorKind, Write},
    path::PathBuf,
};

use tempfile::NamedTempFile;

use crate::agent::session::{Session, SessionId, SessionStore};

pub struct JsonSessionStore {
    dir: PathBuf,
}

impl JsonSessionStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            dir: directory.into(),
        }
    }

    fn path_for(&self, id: &SessionId) -> PathBuf {
        self.dir.join(format!("{}.json", id.as_str()))
    }

    fn active_path(&self, conversation: &SessionId) -> PathBuf {
        self.dir
            .join("active")
            .join(format!("{}.json", conversation.as_str()))
    }
}

#[async_trait::async_trait]
impl SessionStore for JsonSessionStore {
    async fn active_id(&self, conversation: &SessionId) -> anyhow::Result<SessionId> {
        match tokio::fs::read(self.active_path(conversation)).await {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            // Existing conversations keep using their original session until /new.
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(conversation.clone()),
            Err(error) => Err(error.into()),
        }
    }

    async fn new_session(&self, conversation: &SessionId) -> anyhow::Result<Session> {
        let session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        self.save(&session).await?;
        write_atomic(
            self.active_path(conversation),
            serde_json::to_vec(session.id())?,
        )
        .await?;
        Ok(session)
    }

    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut sessions = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file()
                || entry.path().extension().is_none_or(|ext| ext != "json")
            {
                continue;
            }
            let data = tokio::fs::read(entry.path()).await?;
            let session: Session = serde_json::from_slice(&data)?;
            sessions.push((entry.metadata().await?.modified()?, session));
        }
        sessions.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.id().as_str().cmp(b.1.id().as_str()))
        });
        Ok(sessions.into_iter().map(|(_, session)| session).collect())
    }

    async fn select(&self, conversation: &SessionId, id: &SessionId) -> anyhow::Result<Session> {
        anyhow::ensure!(
            !id.as_str().is_empty() && !id.as_str().contains(['/', '\\']),
            "Invalid session ID"
        );
        let session = self
            .load(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        anyhow::ensure!(session.id() == id, "Session ID mismatch");
        write_atomic(self.active_path(conversation), serde_json::to_vec(id)?).await?;
        Ok(session)
    }

    async fn load(&self, id: &SessionId) -> anyhow::Result<Option<Session>> {
        let path = self.path_for(id);

        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }

        let data = tokio::fs::read(&path).await?;

        let session = serde_json::from_slice(&data)?;

        Ok(Some(session))
    }

    async fn save(&self, session: &Session) -> anyhow::Result<()> {
        write_atomic(
            self.path_for(session.id()),
            serde_json::to_vec_pretty(session)?,
        )
        .await
    }

    async fn delete(&self, id: &SessionId) -> anyhow::Result<()> {
        let path = self.path_for(id);

        if tokio::fs::try_exists(&path).await? {
            tokio::fs::remove_file(path).await?;
        }

        Ok(())
    }
}

async fn write_atomic(path: PathBuf, data: Vec<u8>) -> anyhow::Result<()> {
    // Once started, the write and atomic replacement finish even if the caller is cancelled.
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Session path has no parent"))?;
        fs::create_dir_all(dir)?;
        let mut file = NamedTempFile::new_in(dir)?;
        file.write_all(&data)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{UserMessage, message::Message};

    #[tokio::test]
    async fn lists_and_selects_saved_sessions_without_losing_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::new(dir.path().join("sessions"));
        assert!(store.list().await.unwrap().is_empty());
        let conversation = SessionId::new("cli:main");
        let mut original = Session::new(conversation.clone());
        original.push(Message::User(UserMessage {
            files: Vec::new(),
            content: "original".into(),
            images: vec![],
        }));
        store.save(&original).await.unwrap();
        let fresh = store.new_session(&conversation).await.unwrap();
        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| session.id() == original.id()));
        assert!(sessions.iter().any(|session| session.id() == fresh.id()));
        assert_eq!(store.active_id(&conversation).await.unwrap(), *fresh.id());
        let selected = store.select(&conversation, original.id()).await.unwrap();
        assert!(
            matches!(&selected.messages()[0], Message::User(message) if message.content == "original")
        );
        let reopened = JsonSessionStore::new(dir.path().join("sessions"));
        assert_eq!(
            reopened.active_id(&conversation).await.unwrap(),
            *original.id()
        );
        assert!(
            reopened
                .select(&conversation, &SessionId::new("missing"))
                .await
                .is_err()
        );
        assert!(
            reopened
                .select(&conversation, &SessionId::new("../outside"))
                .await
                .is_err()
        );
        assert_eq!(
            reopened.active_id(&conversation).await.unwrap(),
            *original.id()
        );
        assert!(reopened.load(fresh.id()).await.unwrap().is_some());
    }
}
