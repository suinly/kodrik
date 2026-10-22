use crate::agent::session::{Session, SessionId};

#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    /// Custom session-only stores do not provide durable execution checkpoints.
    fn runtime_store(&self) -> Option<&super::SqliteSessionStore> {
        None
    }

    async fn active_id(&self, conversation: &SessionId) -> anyhow::Result<SessionId>;
    async fn new_session(&self, conversation: &SessionId) -> anyhow::Result<Session>;
    async fn list(&self) -> anyhow::Result<Vec<Session>>;
    async fn select(&self, conversation: &SessionId, id: &SessionId) -> anyhow::Result<Session>;
    async fn load(&self, id: &SessionId) -> anyhow::Result<Option<Session>>;
    async fn save(&self, session: &Session) -> anyhow::Result<()>;
    async fn delete(&self, id: &SessionId) -> anyhow::Result<()>;
}
