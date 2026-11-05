use crate::agent::{
    message::Message,
    run_state::{RunPhase, RunRecord},
    session::Session,
};
use rusqlite::{Connection, params};

pub(crate) fn migrate(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE web_push_subscriptions(
            endpoint TEXT PRIMARY KEY, session_hash TEXT NOT NULL,
            subscription BLOB NOT NULL, locale TEXT NOT NULL);
         CREATE TABLE web_push_presence(
            endpoint TEXT NOT NULL REFERENCES web_push_subscriptions(endpoint) ON DELETE CASCADE,
            tab_id TEXT NOT NULL, session_id TEXT NOT NULL, updated_at INTEGER NOT NULL,
            PRIMARY KEY(endpoint,tab_id));
         CREATE TABLE web_push_queue(
            event_id TEXT NOT NULL,
            endpoint TEXT NOT NULL REFERENCES web_push_subscriptions(endpoint) ON DELETE CASCADE,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            attempts INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL,
            created_at INTEGER NOT NULL, PRIMARY KEY(event_id,endpoint));
         CREATE INDEX web_push_due ON web_push_queue(next_attempt);
         CREATE TABLE web_push_bindings(endpoint TEXT PRIMARY KEY,session_hash TEXT NOT NULL);
         PRAGMA user_version=7;",
    )?;
    Ok(())
}

pub(crate) fn enqueue(db: &Connection, id: &str, session: &Session) -> anyhow::Result<()> {
    if !super::chat::personal_session(session)
        || session.webhook_instruction.is_some() && !id.starts_with("approval:")
    {
        return Ok(());
    }
    db.execute(
        "INSERT OR IGNORE INTO web_push_queue(event_id,endpoint,session_id,next_attempt,created_at)
         SELECT ?1,endpoint,?2,?3,?3 FROM web_push_subscriptions",
        params![id, session.id().as_str(), chrono::Utc::now().timestamp()],
    )?;
    Ok(())
}

pub(crate) fn migrate_presence(db: &Connection) -> anyhow::Result<()> {
    // Presence expires in 45 seconds; durable subscriptions and queued jobs stay intact.
    db.execute_batch(
        "DROP TABLE web_push_presence;
         CREATE TABLE web_push_presence(
            session_hash TEXT NOT NULL, tab_id TEXT NOT NULL,
            session_id TEXT NOT NULL, updated_at INTEGER NOT NULL,
            PRIMARY KEY(session_hash,tab_id));
         PRAGMA user_version=8;",
    )?;
    Ok(())
}

pub(crate) fn checkpoint(
    db: &Connection,
    run: &RunRecord,
    session: &Session,
) -> anyhow::Result<()> {
    if matches!(run.phase, RunPhase::Completed)
        && matches!(session.messages().last(), Some(Message::Assistant(_)))
        && session
            .message_time(session.messages().len() - 1)
            .is_some_and(|time| time >= run.started_at)
    {
        enqueue(db, &run.id, session)?;
    }
    Ok(())
}
