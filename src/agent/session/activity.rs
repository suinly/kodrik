use super::{Session, SqliteSessionStore};
use crate::agent::message::Message;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

// Collect active IDs once, not a history scan for every listed conversation.
const BUSY_SQL: &str = "SELECT EXISTS(
    SELECT 1 FROM web_chat_activity WHERE listed=1 AND session_id IN (
        SELECT value FROM json_each(?1)
        UNION ALL SELECT session_id FROM runs WHERE terminal=0
        UNION ALL SELECT coalesce(s.session_id,active.session_id,s.conversation_id)
            FROM submissions s LEFT JOIN active_sessions active ON active.conversation_id=s.conversation_id
            WHERE s.status='Queued'
            AND NOT EXISTS(SELECT 1 FROM inbox_controls c WHERE c.conversation_id=s.conversation_id)
        UNION ALL SELECT r.session_id FROM deliveries d JOIN runs r ON r.id=d.run_id
            WHERE json_extract(d.payload,'$.status') IN ('Pending','Sending')
    ))";

// Only committed assistant text changes the read token; user messages and live deltas do not.
pub(crate) fn read_revision(session: &Session) -> String {
    let messages: Vec<_> = session
        .messages()
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            Message::Assistant(message) if !message.content.is_empty() => {
                Some((index, &message.content))
            }
            _ => None,
        })
        .collect();
    if messages.is_empty() {
        return String::new();
    }
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(session.id(), session.generation(), messages))
                .expect("Assistant text is serializable")
        )
    )
}

pub(super) fn capture(db: &Connection, session: &Session) -> anyhow::Result<()> {
    db.execute(
        "INSERT INTO web_chat_activity(session_id,revision,listed) VALUES(?1,?2,?3)
        ON CONFLICT(session_id) DO UPDATE SET revision=excluded.revision,listed=excluded.listed",
        params![
            session.id().as_str(),
            read_revision(session),
            crate::web::chat::listed_session(session)
        ],
    )?;
    Ok(())
}

pub(super) fn migrate(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS web_chat_activity(
        session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
        revision TEXT NOT NULL, read_revision TEXT NOT NULL DEFAULT '', listed INTEGER NOT NULL);
        PRAGMA user_version=14;",
    )?;
    let sessions = db
        .prepare("SELECT s.payload FROM sessions s LEFT JOIN web_chat_activity a ON a.session_id=s.id WHERE a.session_id IS NULL")?
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for payload in sessions {
        let session: Session = serde_json::from_slice(&payload)?;
        capture(db, &session)?;
        // Baseline only newly imported history; existing unread markers must survive retries.
        db.execute(
            "UPDATE web_chat_activity SET read_revision=revision WHERE session_id=?1",
            [session.id().as_str()],
        )?;
    }
    Ok(())
}

impl SqliteSessionStore {
    pub(crate) async fn mark_chat_read(&self, id: &str, revision: &str) -> anyhow::Result<bool> {
        let (id, revision) = (id.to_owned(), revision.to_owned());
        self.with_connection(move |db| {
            // A stale browser must not acknowledge an answer that arrived after its snapshot.
            Ok(db.execute(
                "UPDATE web_chat_activity SET read_revision=?2
                WHERE session_id=?1 AND revision=?2 AND listed=1",
                params![id, revision],
            )? != 0)
        })
        .await
    }

    pub(crate) async fn chat_activity(&self, running: Vec<String>) -> anyhow::Result<(bool, bool)> {
        self.with_connection(move |db| {
            let busy = db.query_row(BUSY_SQL, [serde_json::to_string(&running)?], |row| {
                row.get(0)
            })?;
            let unread = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM web_chat_activity
                WHERE listed=1 AND session_id NOT GLOB 'companion:subagent:*'
                AND revision!='' AND revision!=read_revision)",
                [],
                |row| row.get(0),
            )?;
            Ok((busy, unread))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        AssistantMessage,
        session::{SessionId, SessionStore},
    };

    #[tokio::test]
    async fn activity_checks_history_once_instead_of_once_per_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        for i in 0..100 {
            store
                .save(&Session::new(SessionId::new(format!("cli:{i}"))))
                .await
                .unwrap();
        }
        store.with_connection(|db| {
            db.execute_batch("WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<99)
                INSERT INTO runs(id,conversation_id,session_id,payload,terminal,revision) SELECT 'run:'||i,'cli:'||i,'cli:'||i,'{}',1,1 FROM n;
                INSERT INTO deliveries SELECT id,id,conversation_id,'{\"status\":\"Delivered\"}' FROM runs;
                INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload,status)
                    SELECT id,conversation_id,id,'hash',1,zeroblob(65536),'Done' FROM runs;")?;
            let mut query = db.prepare(BUSY_SQL)?;
            assert!(!query.query_row(["[]"], |row| row.get::<_,bool>(0))?);
            let steps = query.get_status(rusqlite::StatementStatus::VmStep);
            assert!(steps < 50_000, "Activity must not repeatedly scan historical runs: {steps} VM steps");
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn activity_migration_is_idempotent_and_preserves_unread_markers() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        for id in ["cli:seen", "cli:unseen", "cli:missing"] {
            let mut session = Session::new(SessionId::new(id));
            session.push(Message::Assistant(AssistantMessage { content: id.into() }));
            store.save(&session).await.unwrap();
            if id == "cli:seen" {
                store
                    .mark_chat_read(id, &read_revision(&session))
                    .await
                    .unwrap();
            }
        }
        store
            .with_connection(|db| {
                db.execute(
                    "DELETE FROM web_chat_activity WHERE session_id='cli:missing'",
                    [],
                )?;
                db.execute_batch("PRAGMA user_version=13;")?;
                Ok(())
            })
            .await
            .unwrap();
        drop(store);
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let states = store.with_connection(|db| {
            migrate(db)?;
            Ok(db.prepare("SELECT session_id,revision=read_revision FROM web_chat_activity ORDER BY session_id")?
                .query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }).await.unwrap();
        assert_eq!(
            states,
            vec![
                ("cli:missing".into(), true),
                ("cli:seen".into(), true),
                ("cli:unseen".into(), false)
            ]
        );
    }

    #[tokio::test]
    async fn activity_ignores_blocked_queue_but_not_running_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        store
            .save(&Session::new(SessionId::new("cli:queued")))
            .await
            .unwrap();
        store.with_connection(|db| {
            db.execute_batch("INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload,status,session_id)
                VALUES('input','cli:queued','request','hash',1,'{}','Queued','cli:queued');")?;
            Ok(())
        }).await.unwrap();
        assert_eq!(store.chat_activity(vec![]).await.unwrap(), (true, false));
        store
            .with_connection(|db| {
                db.execute(
                    "INSERT INTO inbox_controls VALUES('cli:queued','Stopped')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.chat_activity(vec![]).await.unwrap(), (false, false));
        assert_eq!(
            store
                .chat_activity(vec!["cli:queued".into()])
                .await
                .unwrap(),
            (true, false)
        );
    }

    #[tokio::test]
    async fn migration_baselines_old_history_but_new_answers_are_unread() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let mut session = Session::new(SessionId::new("cli:old"));
        session.push(Message::Assistant(AssistantMessage {
            content: "Old answer".into(),
        }));
        store.save(&session).await.unwrap();
        store
            .with_connection(|db| {
                db.execute_batch("DROP TABLE web_chat_activity; PRAGMA user_version=13;")?;
                Ok(())
            })
            .await
            .unwrap();
        drop(store);
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        assert_eq!(store.chat_activity(vec![]).await.unwrap(), (false, false));
        session.push(Message::Assistant(AssistantMessage {
            content: "New answer".into(),
        }));
        store.save(&session).await.unwrap();
        assert_eq!(store.chat_activity(vec![]).await.unwrap(), (false, true));
    }
}
