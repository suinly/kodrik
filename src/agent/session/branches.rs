use super::{Session, SessionId, SqliteSessionStore, sqlite};
use crate::{
    agent::{
        AgentRunner,
        inbox::{
            AdmissionRejected, BusyMode, SubmissionRequest, active_session, admit_submission,
            session_busy_in,
        },
        message::Message,
    },
    gateway::{IncomingMessage, InterfaceId},
};
use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MessageVersions {
    pub ids: Vec<String>,
    pub current: usize,
}

#[derive(Clone)]
pub(crate) struct Branch {
    pub id: String,
    pub root: String,
    pub parent: String,
    pub index: usize,
}

pub(crate) fn read_branches(db: &Connection) -> anyhow::Result<Vec<Branch>> {
    Ok(db
        .prepare(
            "SELECT session_id,root_id,parent_id,fork_index FROM web_chat_branches ORDER BY rowid",
        )?
        .query_map([], |r| {
            Ok(Branch {
                id: r.get(0)?,
                root: r.get(1)?,
                parent: r.get(2)?,
                index: r.get::<_, i64>(3)? as usize,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn anchor(mut id: String, index: usize, branches: &[Branch]) -> String {
    while let Some(branch) = branches
        .iter()
        .find(|branch| branch.id == id && branch.index >= index)
    {
        id = branch.parent.clone();
    }
    id
}

pub(crate) fn message_versions(
    id: &str,
    index: usize,
    branches: &[Branch],
) -> Option<MessageVersions> {
    let origin = anchor(id.to_owned(), index, branches);
    let mut ids = vec![origin.clone()];
    ids.extend(
        branches
            .iter()
            .filter(|branch| branch.parent == origin && branch.index == index)
            .map(|branch| branch.id.clone()),
    );
    if ids.len() < 2 {
        return None;
    }
    let mut current = id.to_owned();
    while !ids.contains(&current) {
        let Some(parent) = branches.iter().find(|branch| branch.id == current) else {
            break;
        };
        current = parent.parent.clone();
    }
    Some(MessageVersions {
        current: ids
            .iter()
            .position(|version| version == &current)
            .unwrap_or(0),
        ids,
    })
}

pub(crate) fn init_branches(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS web_chat_branches (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id),
            root_id TEXT NOT NULL REFERENCES sessions(id),
            parent_id TEXT NOT NULL REFERENCES sessions(id),
            fork_index INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS web_branches_root ON web_chat_branches(root_id);
         CREATE TABLE IF NOT EXISTS web_chat_selection (
            root_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE);
         CREATE TABLE IF NOT EXISTS web_chat_pins (
            root_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE);",
    )?;
    Ok(())
}

pub(crate) fn root(db: &Connection, id: &str) -> anyhow::Result<String> {
    Ok(db
        .query_row(
            "SELECT root_id FROM web_chat_branches WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| id.to_owned()))
}

pub(crate) fn select_branch(db: &Connection, root: &str, id: &str) -> anyhow::Result<()> {
    db.execute("INSERT INTO web_chat_selection VALUES(?1,?2) ON CONFLICT(root_id) DO UPDATE SET session_id=excluded.session_id", [root,id])?;
    db.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')", [])?;
    Ok(())
}

fn record_branch(
    db: &Connection,
    root: &str,
    source: &str,
    id: &str,
    index: usize,
) -> anyhow::Result<()> {
    let parent = anchor(source.to_owned(), index, &read_branches(db)?);
    db.execute(
        "INSERT INTO web_chat_branches VALUES(?1,?2,?3,?4)",
        params![id, root, parent, index as i64],
    )?;
    select_branch(db, root, id)
}

impl SqliteSessionStore {
    pub(crate) async fn message_versions(
        &self,
        session: &Session,
    ) -> anyhow::Result<HashMap<usize, MessageVersions>> {
        let id = session.id().as_str().to_owned();
        let indices = session
            .messages()
            .iter()
            .enumerate()
            .filter_map(|(index, message)| matches!(message, Message::User(_)).then_some(index))
            .chain(std::iter::once(session.messages().len()))
            .collect::<Vec<_>>();
        self.with_connection(move |db| {
            init_branches(db)?;
            let branches = read_branches(db)?;
            Ok(indices
                .into_iter()
                .enumerate()
                .filter_map(|(user_turn, index)| {
                    message_versions(&id, index, &branches).map(|versions| (user_turn, versions))
                })
                .collect())
        })
        .await
    }
}

impl AgentRunner {
    pub(crate) async fn list_conversations(&self) -> anyhow::Result<Vec<Session>> {
        let Some(store) = self.sqlite_store() else {
            return self.list_sessions().await;
        };
        store.with_connection(|db| {
            init_branches(db)?;
            let tx = db.transaction()?;
            let ids = tx.prepare(
                "SELECT id FROM (
                    SELECT s.id,
                        max(s.updated_at) OVER (PARTITION BY coalesce(b.root_id,s.id)) AS activity,
                        row_number() OVER (
                            PARTITION BY coalesce(b.root_id,s.id)
                            ORDER BY CASE WHEN s.id=selected.session_id THEN 0 ELSE 1 END,
                                s.updated_at DESC,s.id
                        ) AS version_rank
                    FROM sessions s
                    LEFT JOIN web_chat_branches b ON b.session_id=s.id
                    LEFT JOIN web_chat_selection selected ON selected.root_id=coalesce(b.root_id,s.id)
                    WHERE substr(s.id,1,10)!='companion:'
                ) WHERE version_rank=1 ORDER BY activity DESC,id",
            )?.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let sessions = ids.into_iter().map(|id| {
                sqlite::load(&tx, &id)?.context("Conversation session not found")
            }).collect::<anyhow::Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(sessions)
        }).await
    }

    pub(crate) async fn edit_message(
        &self,
        conversation: &SessionId,
        source: &SessionId,
        user_turn: usize,
        text: String,
    ) -> anyhow::Result<(Session, IncomingMessage)> {
        ensure!(!text.trim().is_empty(), "Message must contain text");
        ensure!(text.len() <= 32768, "Message exceeds 32768 bytes");
        ensure!(
            !self.conversation_busy(conversation).await,
            AdmissionRejected("Conversation is busy")
        );
        let guard = self.lock_conversation(conversation).await?;
        let store = self
            .sqlite_store()
            .context("Editing messages requires SQLite")?;
        let mut request = SubmissionRequest::new(
            conversation,
            IncomingMessage::new(
                InterfaceId("cli".into()),
                conversation.as_str().strip_prefix("cli:").unwrap_or("main"),
                text,
            ),
            BusyMode::RejectIfBusy,
        );
        self.prepare_submission(&mut request);
        request.control = false;
        let session = self
            .sessions
            .load(source)
            .await?
            .context("Session not found")?;
        let (index, user) = session
            .messages()
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                Message::User(user) => Some((index, user)),
                _ => None,
            })
            .nth(user_turn)
            .context("User message not found")?;
        for file in &user.files {
            file.bytes().await?;
        }
        request.message.files = user.files.clone();
        request.message.images = user.images.clone();
        let branch = session.fork_before(SessionId::new(&request.request_id), index)?;
        let source = source.clone();
        let operation = store.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure!(active_session(&tx, &request.conversation_id)? == source.as_str(), AdmissionRejected("The active session changed"));
            let source_pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND terminal=0)
                 OR EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued')",
                [&request.conversation_id], |r| r.get(0),
            )?;
            ensure!(!source_pending, AdmissionRejected("Conversation is busy"));
            init_branches(&tx)?;
            let root = root(&tx, source.as_str())?;
            let mut members = vec![root.clone()];
            members.extend(tx.prepare("SELECT session_id FROM web_chat_branches WHERE root_id=?1")?.query_map([&root], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
            for member in members {
                let delivering: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deliveries d JOIN runs r ON r.id=d.run_id WHERE r.session_id=?1 AND json_extract(d.payload,'$.status') IN ('Pending','Sending'))", [&member], |r| r.get(0))?;
                ensure!(!session_busy_in(&tx, &member)? && !delivering, AdmissionRejected("Conversation is busy"));
            }
            sqlite::save(&tx, &branch)?;
            record_branch(&tx, &root, source.as_str(), branch.id().as_str(), index)?;
            sqlite::select(&tx, &request.conversation_id, branch.id().as_str())?;
            let hash = request.hash()?;
            let record = admit_submission(&tx, request, hash, Some(branch.id().as_str()))?;
            tx.commit()?;
            Ok((branch, record.request.message))
        });
        crate::agent::run_state::CURRENT_EXECUTION
            .scope(guard.ownership.clone(), operation)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        Agent, AgentEvent, ContextBuilder, FileAttachment, Image, LlmContext,
        inbox::{BusyMode, SubmissionRequest, SubmissionStatus},
        llm::LlmProvider,
        message::{AssistantMessage, Message, UserMessage},
        session::{SessionStore, SqliteSessionStore},
        tools::{Tool, ToolRegistry},
    };
    use std::sync::Arc;

    struct NoModel;

    #[async_trait::async_trait]
    impl LlmProvider for NoModel {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            panic!("Admitting an edit must not invoke the model")
        }
    }

    fn runner(store: &SqliteSessionStore) -> AgentRunner {
        AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(store.clone()),
        )
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            browser_context: None,
            content: text.into(),
            files: vec![],
            images: vec![],
        })
    }

    #[test]
    fn shared_message_versions_resolve_inherited_and_nested_turns() {
        let branches = [
            ("one", "root", 2),
            ("two", "root", 2),
            ("nested", "one", 4),
            ("earlier", "root", 0),
        ]
        .map(|(id, parent, index)| Branch {
            id: id.into(),
            root: "root".into(),
            parent: parent.into(),
            index,
        });
        assert_eq!(
            message_versions("nested", 2, &branches),
            Some(MessageVersions {
                ids: vec!["root".into(), "one".into(), "two".into()],
                current: 1
            })
        );
        assert_eq!(
            message_versions("nested", 0, &branches),
            Some(MessageVersions {
                ids: vec!["root".into(), "earlier".into()],
                current: 0
            })
        );
        assert_eq!(
            message_versions("nested", 4, &branches),
            Some(MessageVersions {
                ids: vec!["one".into(), "nested".into()],
                current: 1
            })
        );
        assert_eq!(message_versions("root", 9, &branches), None);
    }

    #[tokio::test]
    async fn saved_and_pending_replacements_have_shared_message_versions() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(directory.path().into()).unwrap();
        let conversation = SessionId::new("cli:main");
        let mut original = Session::new(SessionId::new("root"));
        original.push(user("first"));
        original.push(Message::Assistant(AssistantMessage {
            content: "answer".into(),
        }));
        original.push(user("second"));
        original.set_compaction(2, "First turn summary".into());
        store.save(&original).await.unwrap();
        store.select(&conversation, original.id()).await.unwrap();
        let runner = runner(&store);
        let (mut branch, message) = runner
            .edit_message(&conversation, original.id(), 1, "second edited".into())
            .await
            .unwrap();
        assert_eq!(branch.compaction(), Some((2, "First turn summary")));
        let saved = store.load(branch.id()).await.unwrap().unwrap();
        assert_eq!(saved.compaction(), Some((2, "First turn summary")));
        let expected = MessageVersions {
            ids: vec!["root".into(), branch.id().as_str().into()],
            current: 1,
        };
        assert_eq!(
            store.message_versions(&branch).await.unwrap().get(&1),
            Some(&expected),
            "the durable prefix already exposes the pending replacement's versions"
        );
        runner
            .cancel_queued_input(&conversation, &message.request_id)
            .await
            .unwrap();
        branch.push(user("second edited"));
        store.save(&branch).await.unwrap();
        assert_eq!(
            store.message_versions(&branch).await.unwrap().get(&1),
            Some(&expected)
        );
        let original_versions = store.message_versions(&original).await.unwrap();
        assert_eq!(
            original_versions.get(&1),
            Some(&MessageVersions {
                current: 0,
                ..expected
            })
        );
        assert!(!original_versions.contains_key(&0));
        let (sibling, _) = runner
            .edit_message(&conversation, branch.id(), 1, "second alternative".into())
            .await
            .unwrap();
        assert_eq!(
            store.message_versions(&sibling).await.unwrap().get(&1),
            Some(&MessageVersions {
                ids: vec![
                    "root".into(),
                    branch.id().as_str().into(),
                    sibling.id().as_str().into()
                ],
                current: 2
            })
        );
    }

    #[tokio::test]
    async fn conversation_list_groups_versions_and_selection_is_shared_and_persistent() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(directory.path().into()).unwrap();
        let conversation = SessionId::new("cli:main");
        let mut source = Session::new(SessionId::new("root"));
        source.push(user("original"));
        let branch = source
            .fork_before(SessionId::new("old-version"), 0)
            .unwrap();
        for session in [
            &source,
            &branch,
            &Session::new(SessionId::new("other")),
            &Session::new(SessionId::new("companion:test")),
        ] {
            store.save(session).await.unwrap();
        }
        store.with_connection(|db| {
            init_branches(db)?;
            record_branch(db, "root", "root", "old-version", 0)?;
            db.execute("UPDATE sessions SET updated_at=CASE id WHEN 'root' THEN 300 WHEN 'old-version' THEN 100 WHEN 'other' THEN 200 ELSE 400 END", [])?;
            Ok(())
        }).await.unwrap();
        store.select(&conversation, branch.id()).await.unwrap();
        let runner = runner(&store);
        let ids = |sessions: Vec<Session>| {
            sessions
                .into_iter()
                .map(|session| session.id().as_str().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(runner.list_conversations().await.unwrap()),
            ["old-version", "other"],
            "group ordering follows family activity while the selected version is retained"
        );
        runner
            .select_session(&conversation, source.id())
            .await
            .unwrap();
        let reopened = SqliteSessionStore::open(directory.path().into()).unwrap();
        assert_eq!(
            ids(super::tests::runner(&reopened)
                .list_conversations()
                .await
                .unwrap()),
            ["root", "other"]
        );
        let selected: String = reopened
            .with_connection(|db| {
                Ok(db.query_row(
                    "SELECT session_id FROM web_chat_selection WHERE root_id='root'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(selected, "root");
        reopened
            .with_connection(|db| {
                db.execute(
                    "UPDATE web_chat_selection SET session_id='other' WHERE root_id='root'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            ids(runner.list_conversations().await.unwrap()),
            ["root", "other"],
            "a selected ID outside the family must not hide its latest existing member"
        );
        runner
            .select_session(&conversation, branch.id())
            .await
            .unwrap();
        assert_eq!(
            ids(super::tests::runner(&reopened)
                .list_conversations()
                .await
                .unwrap()),
            ["old-version", "other"]
        );
    }

    #[tokio::test]
    async fn edited_turn_is_durable_and_preserves_original_history_and_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(directory.path().into()).unwrap();
        let conversation = SessionId::new("cli:main");
        let mut source = Session::new(conversation.clone());
        source.initialize_system_prompt("original system prompt");
        source.push(user("first"));
        source.push(Message::Assistant(AssistantMessage {
            content: "original answer".into(),
        }));
        let file = FileAttachment::save(
            &directory.path().join("files"),
            "note.txt",
            None,
            b"attachment",
        )
        .unwrap();
        source.push(Message::User(UserMessage {
            browser_context: None,
            content: "second".into(),
            files: vec![file.clone()],
            images: vec![Image {
                data_url: "data:image/png;base64,cGl4ZWw=".into(),
                width: 1,
                height: 1,
            }],
        }));
        source.push(Message::Assistant(AssistantMessage {
            content: "future answer".into(),
        }));
        store.save(&source).await.unwrap();
        let original = serde_json::to_value(&source).unwrap();
        let runner = runner(&store);
        let (branch, message) = runner
            .edit_message(&conversation, source.id(), 1, "/new".into())
            .await
            .unwrap();
        assert_ne!(branch.id(), source.id());
        assert_eq!(branch.messages().len(), 2);
        assert_eq!(branch.system_prompt(), Some("original system prompt"));
        assert_ne!(branch.generation(), source.generation());
        assert_eq!(message.text, "/new");
        assert_eq!(message.files[0].bytes().await.unwrap(), b"attachment");
        assert_eq!(message.images[0].data_url, "data:image/png;base64,cGl4ZWw=");
        assert_eq!(
            serde_json::to_value(store.load(source.id()).await.unwrap().unwrap()).unwrap(),
            original
        );
        let reopened = SqliteSessionStore::open(directory.path().into()).unwrap();
        assert_eq!(
            reopened.active_id(&conversation).await.unwrap(),
            *branch.id()
        );
        assert_eq!(
            reopened
                .load(branch.id())
                .await
                .unwrap()
                .unwrap()
                .messages()
                .len(),
            2
        );
        let pending = reopened
            .next_submission(&conversation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.status, SubmissionStatus::Queued);
        assert_eq!(pending.session_id.as_deref(), Some(branch.id().as_str()));
        assert!(!pending.request.control);
        assert_eq!(pending.request.message.text, "/new");
        assert_eq!(pending.request.message.files[0].path, file.path);
        let branch_id = branch.id().as_str().to_owned();
        let metadata = reopened.with_connection(move |db| {
            Ok(db.query_row("SELECT b.root_id,b.parent_id,b.fork_index,s.session_id FROM web_chat_branches b JOIN web_chat_selection s ON s.root_id=b.root_id WHERE b.session_id=?1", [branch_id], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, i64>(2)?,r.get::<_, String>(3)?)))?)
        }).await.unwrap();
        assert_eq!(
            metadata,
            (
                "cli:main".into(),
                "cli:main".into(),
                2,
                branch.id().as_str().into()
            )
        );
        runner
            .cancel_queued_input(&conversation, &message.request_id)
            .await
            .unwrap();
        let (earlier_branch, _) = runner
            .edit_message(&conversation, branch.id(), 0, "earlier replacement".into())
            .await
            .unwrap();
        assert!(earlier_branch.messages().is_empty());
        let earlier_id = earlier_branch.id().as_str().to_owned();
        let parent: String = reopened
            .with_connection(move |db| {
                Ok(db.query_row(
                    "SELECT parent_id FROM web_chat_branches WHERE session_id=?1",
                    [earlier_id],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            parent, "cli:main",
            "editing a shared prefix anchors the new version to its original parent"
        );
    }

    #[tokio::test]
    async fn stale_busy_invalid_and_failed_edits_do_not_change_the_source() {
        for case in [
            "stale",
            "busy",
            "shared-busy",
            "owned",
            "invalid",
            "empty",
            "oversized",
            "attachment",
            "failure",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = SqliteSessionStore::open(directory.path().into()).unwrap();
            let conversation = SessionId::new("cli:main");
            let mut source = Session::new(conversation.clone());
            let mut files = vec![];
            if case == "attachment" {
                let file = FileAttachment::save(
                    &directory.path().join("files"),
                    "missing.txt",
                    None,
                    b"lost attachment",
                )
                .unwrap();
                std::fs::remove_file(&file.path).unwrap();
                files.push(file);
            }
            source.push(Message::User(UserMessage {
                browser_context: None,
                content: "original".into(),
                files,
                images: vec![],
            }));
            store.save(&source).await.unwrap();
            let _ownership = if case == "owned" {
                Some(
                    store
                        .execution_lock("session", source.id().as_str())
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            if case == "shared-busy" {
                let other = SessionId::new("other-channel");
                store.select(&other, source.id()).await.unwrap();
                store
                    .submit(SubmissionRequest::new(
                        &other,
                        IncomingMessage::new(
                            InterfaceId("test".into()),
                            "other",
                            "queued elsewhere",
                        ),
                        BusyMode::Steer,
                    ))
                    .await
                    .unwrap();
            }
            if case == "busy" {
                store
                    .submit(SubmissionRequest::new(
                        &conversation,
                        IncomingMessage::new(
                            crate::gateway::InterfaceId("cli".into()),
                            "main",
                            "queued",
                        ),
                        BusyMode::Steer,
                    ))
                    .await
                    .unwrap();
            }
            if case == "failure" {
                store.with_connection(|db| {
                    db.execute_batch("CREATE TRIGGER reject_edit BEFORE INSERT ON submissions BEGIN SELECT RAISE(ABORT,'forced failure'); END;")?;
                    Ok(())
                }).await.unwrap();
            }
            let result = runner(&store)
                .edit_message(
                    &conversation,
                    &SessionId::new(if case == "stale" { "other" } else { "cli:main" }),
                    usize::from(case == "invalid"),
                    if case == "empty" {
                        " ".into()
                    } else if case == "oversized" {
                        "x".repeat(32769)
                    } else {
                        "replacement".into()
                    },
                )
                .await;
            assert!(result.is_err(), "{case}");
            assert_eq!(
                store.active_id(&conversation).await.unwrap(),
                conversation,
                "{case}"
            );
            assert_eq!(store.list().await.unwrap().len(), 1, "{case}");
            assert_eq!(
                serde_json::to_value(store.load(&conversation).await.unwrap().unwrap()).unwrap(),
                serde_json::to_value(&source).unwrap(),
                "{case}"
            );
            assert_eq!(
                store
                    .next_submission(&conversation)
                    .await
                    .unwrap()
                    .is_some(),
                case == "busy",
                "{case}"
            );
        }
    }
}
