use super::{Chat, WebState, error, internal};
use crate::agent::{
    inbox::{BusyMode, SubmissionRequest, admit_submission},
    message::Message,
    session::{Session, SessionId, SessionStore, SqliteSessionStore},
};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(super) async fn init(store: &SqliteSessionStore) -> anyhow::Result<()> {
    store
        .with_connection(|db| {
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
        })
        .await
}

fn root(db: &Connection, id: &str) -> anyhow::Result<String> {
    Ok(db
        .query_row(
            "SELECT root_id FROM web_chat_branches WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| id.to_owned()))
}
fn members(db: &Connection, root: &str) -> anyhow::Result<Vec<String>> {
    let mut ids = vec![root.to_owned()];
    ids.extend(
        db.prepare("SELECT session_id FROM web_chat_branches WHERE root_id=?1 ORDER BY rowid")?
            .query_map([root], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?,
    );
    Ok(ids)
}
fn idle(db: &Connection, ids: &[String]) -> anyhow::Result<bool> {
    for id in ids {
        if db.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND terminal=0)
             OR EXISTS(SELECT 1 FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id
                 WHERE s.status='Queued' AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1)
             OR EXISTS(SELECT 1 FROM deliveries d JOIN runs r ON r.id=d.run_id
                 WHERE r.session_id=?1 AND json_extract(d.payload,'$.status') IN ('Pending','Sending'))",
            [id], |r| r.get::<_,bool>(0),
        )? { return Ok(false); }
    }
    Ok(true)
}
fn select_branch(db: &Connection, root: &str, id: &str) -> anyhow::Result<()> {
    db.execute("INSERT INTO web_chat_selection VALUES(?1,?2) ON CONFLICT(root_id) DO UPDATE SET session_id=excluded.session_id", [root,id])?;
    db.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')", [])?;
    Ok(())
}

#[derive(Clone)]
struct Branch {
    id: String,
    root: String,
    parent: String,
    index: usize,
}
async fn branches(chat: &Chat) -> anyhow::Result<Vec<Branch>> {
    chat.sessions.with_connection(|db| {
        Ok(db.prepare("SELECT session_id,root_id,parent_id,fork_index FROM web_chat_branches ORDER BY rowid")?.query_map([], |r| Ok(Branch{id:r.get(0)?,root:r.get(1)?,parent:r.get(2)?,index:r.get::<_, i64>(3)? as usize}))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await
}
fn anchor(mut id: String, index: usize, branches: &[Branch]) -> String {
    while let Some(branch) = branches
        .iter()
        .find(|branch| branch.id == id && branch.index >= index)
    {
        id = branch.parent.clone();
    }
    id
}
impl Chat {
    pub(super) async fn root_id(&self, id: &str) -> anyhow::Result<String> {
        let id = id.to_owned();
        self.sessions.with_connection(move |db| root(db, &id)).await
    }
    pub(super) async fn decorate_versions(
        &self,
        id: &str,
        snapshot: &mut Value,
    ) -> anyhow::Result<()> {
        let branches = branches(self).await?;
        if let Some(messages) = snapshot["messages"].as_array_mut() {
            for message in messages.iter_mut().filter(|m| m["role"] == "user") {
                let Some(index) = message["id"]
                    .as_str()
                    .and_then(|id| id.rsplit(':').next())
                    .and_then(|index| index.parse::<usize>().ok())
                else {
                    continue;
                };
                let origin = anchor(id.to_owned(), index, &branches);
                let mut versions = vec![origin.clone()];
                versions.extend(
                    branches
                        .iter()
                        .filter(|branch| branch.parent == origin && branch.index == index)
                        .map(|branch| branch.id.clone()),
                );
                if versions.len() < 2 {
                    continue;
                }
                let mut current = id.to_owned();
                while !versions.contains(&current) {
                    let Some(parent) = branches.iter().find(|branch| branch.id == current) else {
                        break;
                    };
                    current = parent.parent.clone();
                }
                message["version"] = json!(
                    versions
                        .iter()
                        .position(|version| version == &current)
                        .unwrap_or(0)
                );
                message["versions"] = json!(versions);
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
pub(super) struct Search {
    #[serde(default)]
    q: String,
    #[serde(default = "page_size")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}
fn page_size() -> usize {
    30
}

async fn page_sessions(
    chat: &Chat,
    limit: usize,
    offset: usize,
) -> anyhow::Result<(Vec<Session>, Vec<String>)> {
    chat.sessions.with_connection(move |db| {
        let mut groups: HashMap<String,(bool,i64)> = HashMap::new();
        let mut statement = db.prepare("SELECT s.id,coalesce(b.root_id,s.id),s.updated_at,
            json_type(s.payload,'$.telegram_group') IS NOT NULL AND json_type(s.payload,'$.telegram_group')!='null',
            p.root_id IS NOT NULL FROM sessions s
            LEFT JOIN web_chat_branches b ON b.session_id=s.id
            LEFT JOIN web_chat_pins p ON p.root_id=coalesce(b.root_id,s.id)")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            if row.get::<_,bool>(3)? || !super::personal_session_id(&id) { continue; }
            let root: String = row.get(1)?;
            let updated: i64 = row.get(2)?;
            let pinned: bool = row.get(4)?;
            groups.entry(root).and_modify(|entry| entry.1=entry.1.max(updated)).or_insert((pinned,updated));
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|(a, av),(b,bv)| bv.cmp(av).then_with(|| a.cmp(b)));
        let roots: Vec<_> = groups.into_iter().skip(offset).take(limit).map(|(id,_)|id).collect();
        let mut statement=db.prepare("SELECT s.id,s.payload FROM sessions s
            LEFT JOIN web_chat_branches b ON b.session_id=s.id
            WHERE coalesce(b.root_id,s.id) IN (SELECT value FROM json_each(?1))
            ORDER BY s.updated_at DESC,s.id")?;
        let mut rows=statement.query([serde_json::to_string(&roots)?])?;
        let mut sessions=vec![];
        while let Some(row)=rows.next()? {
            let session: Session=serde_json::from_slice(&row.get::<_,Vec<u8>>(1)?)?;
            anyhow::ensure!(session.id().as_str()==row.get::<_,String>(0)?,"Session ID mismatch");
            sessions.push(session);
        }
        Ok((sessions,roots))
    }).await
}

pub(super) async fn list(
    Extension(state): Extension<WebState>,
    Query(search): Query<Search>,
) -> Result<Json<Value>, Response> {
    if search.q.len() > 32768 {
        return Err(error(StatusCode::BAD_REQUEST, "Search is too long"));
    }
    if !(1..=100).contains(&search.limit) || search.offset > i64::MAX as usize {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid conversation page"));
    }
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let query = search.q.trim().to_lowercase();
    let (sessions, page_roots) = if query.is_empty() {
        let (sessions, roots) = page_sessions(chat, search.limit, search.offset)
            .await
            .map_err(internal)?;
        (sessions, Some(roots))
    } else {
        (chat.sessions.list().await.map_err(internal)?, None)
    };
    let branches = branches(chat).await.map_err(internal)?;
    let titles = chat.conversation_titles().await.map_err(internal)?;
    let pins: HashSet<String> = chat
        .sessions
        .with_connection(|db| {
            Ok(db
                .prepare("SELECT root_id FROM web_chat_pins")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .map_err(internal)?;
    let selection: HashMap<String, String> = chat
        .sessions
        .with_connection(|db| {
            Ok(db
                .prepare("SELECT root_id,session_id FROM web_chat_selection")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .map_err(internal)?;
    // shortcut: search scans saved histories, add an index when archive latency warrants it.
    let runs = chat.runs.lock().await;
    let jobs = chat.title_jobs.lock().await;
    let mut seen = HashSet::new();
    let roots: HashMap<_, _> = branches
        .iter()
        .map(|b| (b.id.as_str(), b.root.as_str()))
        .collect();
    let mut groups: HashMap<&str, Vec<&Session>> = HashMap::new();
    for session in sessions
        .iter()
        .filter(|session| super::personal_session(session))
    {
        let id = session.id().as_str();
        groups
            .entry(roots.get(id).copied().unwrap_or(id))
            .or_default()
            .push(session);
    }
    let mut result = vec![];
    for session in sessions
        .iter()
        .filter(|session| super::personal_session(session))
    {
        let id = session.id().as_str();
        let root = roots.get(id).copied().unwrap_or(id);
        if !seen.insert(root) {
            continue;
        }
        let selected = selection.get(root).map(String::as_str).unwrap_or(root);
        let group = &groups[root];
        let mut shown = group
            .iter()
            .find(|s| s.id().as_str() == selected)
            .copied()
            .unwrap_or(session);
        let title = titles
            .get(root)
            .cloned()
            .or_else(|| super::fallback_title(shown));
        let title_default = title.is_none();
        let title = title.unwrap_or_else(|| "Новый диалог".into());
        let mut snippet = None;
        let mut match_message = None;
        if !query.is_empty() {
            // Prefer the selected branch, then search preserved versions too.
            let hit = std::iter::once(shown)
                .chain(group.iter().copied().filter(|s| s.id() != shown.id()))
                .find_map(|s| {
                    s.messages().iter().enumerate().find_map(|(index, m)| {
                        let text = match m {
                            Message::User(m) => &m.content,
                            Message::Assistant(m) => &m.content,
                            _ => return None,
                        };
                        let lower = text.to_lowercase();
                        let offset = lower.find(&query)?;
                        // Unicode lowercasing may change byte lengths; count characters for a safe excerpt.
                        let start = lower[..offset].chars().count().saturating_sub(40);
                        let excerpt: String = text.chars().skip(start).take(180).collect();
                        Some((s, index, excerpt))
                    })
                });
            if let Some((s, index, text)) = hit {
                shown = s;
                snippet = Some(text);
                match_message = Some(format!("{}:{index}", s.id().as_str()));
            } else if !title.to_lowercase().contains(&query) {
                continue;
            }
        }
        let ids = group
            .iter()
            .map(|s| s.id().as_str().to_owned())
            .collect::<Vec<_>>();
        let busy = group.iter().any(|s| runs.contains(s.id().as_str()))
            || !chat
                .sessions
                .with_connection(move |db| idle(db, &ids))
                .await
                .map_err(internal)?;
        let updated = group.iter().filter_map(|s| s.last_message_at()).max();
        result.push(json!({"id":shown.id().as_str(),"root_id":root,"title":title,"title_default":title_default,"title_pending":jobs.contains(root),"pinned":pins.contains(root),"busy":busy,"updated_at":updated,"snippet":snippet,"match_message":match_message}));
    }
    if let Some(roots) = page_roots {
        result.sort_by_key(|session| {
            roots
                .iter()
                .position(|root| Some(root.as_str()) == session["root_id"].as_str())
        });
    } else {
        result.sort_by_key(|session| !session["pinned"].as_bool().unwrap_or(false));
        result = result
            .into_iter()
            .skip(search.offset)
            .take(search.limit)
            .collect();
    }
    Ok(Json(json!(result)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pin {
    pinned: bool,
}
pub(super) async fn pin(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Pin>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let _runs = chat.runs.lock().await;
    chat.session(&id).await?;
    chat.sessions.with_connection(move |db| {
        let tx = db.transaction()?;
        let root = root(&tx,&id)?;
        if input.pinned {
            tx.execute("INSERT OR IGNORE INTO web_chat_pins VALUES(?1)",[&root])?;
        } else {
            tx.execute("DELETE FROM web_chat_pins WHERE root_id=?1",[&root])?;
        }
        tx.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')",[])?;
        tx.commit()?;
        Ok(())
    }).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Title {
    title: String,
}
pub(super) async fn rename(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Title>,
) -> Result<Json<Value>, Response> {
    let title = input.title.trim().to_owned();
    if title.is_empty() || title.chars().count() > 200 || title.chars().any(char::is_control) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Название должно содержать от 1 до 200 символов без переводов строк",
        ));
    }
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let _runs = chat.runs.lock().await;
    chat.session(&id).await?;
    let saved = title.clone();
    chat.sessions.with_connection(move |db| {
        let tx=db.transaction()?;
        let root=root(&tx,&id)?;
        tx.execute("INSERT INTO web_chat_titles VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET title=excluded.title",params![root,saved])?;
        tx.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')", [])?;
        tx.commit()?;
        Ok(())
    }).await.map_err(internal)?;
    Ok(Json(json!({"title":title})))
}
pub(super) async fn select(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let _runs = chat.runs.lock().await;
    chat.session(&id).await?;
    chat.sessions
        .with_connection(move |db| {
            let tx = db.transaction()?;
            select_branch(&tx, &root(&tx, &id)?, &id)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    request_id: String,
    text: String,
}
pub(super) async fn edit(
    Extension(state): Extension<WebState>,
    Path((id, index)): Path<(String, usize)>,
    Json(input): Json<Edit>,
) -> Result<Json<Value>, Response> {
    // Keep durable admission and worker startup alive if the browser disconnects.
    tokio::spawn(async move {
        let chat = state.chat().map_err(IntoResponse::into_response)?;
        if !uuid::Uuid::parse_str(&input.request_id)
            .is_ok_and(|uuid| uuid.to_string() == input.request_id)
        {
            return Err(error(StatusCode::BAD_REQUEST, "Invalid request ID"));
        }
        if input.text.len() > 32768 {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Message exceeds 32768 bytes",
            ));
        }
        let mut runs = chat.runs.lock().await;
        let session = chat.session(&id).await?;
        let Some(Message::User(user)) = session.messages().get(index) else {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Only user messages can be edited",
            ));
        };
        if input.text.trim().is_empty() && user.files.is_empty() && user.images.is_empty() {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Message must contain text or attachments",
            ));
        }
        let branch_id = input.request_id.clone();
        let existing = chat
            .sessions
            .load(&SessionId::new(&branch_id))
            .await
            .map_err(internal)?
            .is_some();
        if !existing && runs.len() >= 4 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many active conversations",
            ));
        }
        for file in &user.files {
            file.bytes().await.map_err(internal)?;
        }
        let mut message = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("web".into()),
            &branch_id,
            &input.text,
        );
        message.request_id = input.request_id.clone();
        message.files = user.files.clone();
        message.images = user.images.clone();
        let mut request =
            SubmissionRequest::new(&SessionId::new(&branch_id), message, BusyMode::RejectIfBusy);
        chat.runner.prepare_submission(&mut request);
        // Edits are replacement user turns, including texts resembling slash commands.
        request.control = false;
        let hash = request.hash().map_err(internal)?;
        let branch = session
            .fork_before(SessionId::new(&branch_id), index)
            .map_err(|e| error(StatusCode::BAD_REQUEST, &e.to_string()))?;
        let active = runs.clone();
        let saved_id = branch_id.clone();
        let outcome = chat
            .sessions
            .with_connection(move |db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let root = root(&tx, &id)?;
                let prior: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT parent_id,fork_index FROM web_chat_branches WHERE session_id=?1",
                        [&saved_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let parent = anchor(id.clone(), index, &{
                    let mut stmt = tx.prepare(
                        "SELECT session_id,root_id,parent_id,fork_index FROM web_chat_branches",
                    )?;
                    let rows = stmt
                        .query_map([], |r| {
                            Ok(Branch {
                                id: r.get(0)?,
                                root: r.get(1)?,
                                parent: r.get(2)?,
                                index: r.get::<_, i64>(3)? as usize,
                            })
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    rows
                });
                if let Some((old_parent, old_index)) = prior {
                    anyhow::ensure!(
                        old_parent == parent && old_index == index as i64,
                        crate::agent::inbox::AdmissionRejected("Edit request conflict")
                    );
                } else {
                    anyhow::ensure!(
                        !existing,
                        crate::agent::inbox::AdmissionRejected("Edit request conflict")
                    );
                    let ids = members(&tx, &root)?;
                    anyhow::ensure!(
                        !ids.iter().any(|id| active.contains(id)) && idle(&tx, &ids)?,
                        crate::agent::inbox::AdmissionRejected("Conversation is busy")
                    );
                    tx.execute(
                        "INSERT INTO sessions VALUES(?1,?2,?3)",
                        params![
                            saved_id,
                            serde_json::to_vec(&branch)?,
                            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO web_chat_branches VALUES(?1,?2,?3,?4)",
                        params![saved_id, root, parent, index as i64],
                    )?;
                }
                let record = admit_submission(&tx, request, hash)?;
                select_branch(&tx, &root, &saved_id)?;
                tx.commit()?;
                Ok(record)
            })
            .await
            .map_err(|e| {
                if e.is::<crate::agent::inbox::AdmissionRejected>() {
                    error(StatusCode::CONFLICT, &e.to_string())
                } else {
                    internal(e)
                }
            })?;
        if outcome.status == crate::agent::inbox::SubmissionStatus::Queued
            && !runs.contains(&branch_id)
        {
            runs.insert(branch_id.clone());
            let worker = chat.clone();
            let id = SessionId::new(&branch_id);
            tokio::spawn(async move {
                worker.work(id, false).await;
            });
        }
        Ok(Json(json!({"id":branch_id})))
    })
    .await
    .map_err(|e| internal(e.into()))?
}

pub(super) async fn delete(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    tokio::spawn(async move {
        let chat=state.chat().map_err(IntoResponse::into_response)?;
        let runs=chat.runs.lock().await;
        chat.session(&id).await?;
        let active=runs.clone();
        let ids=chat.sessions.with_connection(move |db| {
            let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let root=root(&tx,&id)?;
            let ids=crate::agent::subagents::with_children(&tx,members(&tx,&root)?)?;
            anyhow::ensure!(!ids.iter().any(|id|active.contains(id)) && idle(&tx,&ids)?,crate::agent::inbox::AdmissionRejected("Conversation is busy"));
            tx.execute("DELETE FROM web_chat_selection WHERE root_id=?1",[&root])?;
            tx.execute("DELETE FROM web_chat_pins WHERE root_id=?1",[&root])?;
            tx.execute("DELETE FROM web_chat_branches WHERE root_id=?1",[&root])?;
            for id in &ids {
                tx.execute("DELETE FROM tool_executions WHERE run_id IN (SELECT id FROM runs WHERE session_id=?1)",[id])?;
                tx.execute("DELETE FROM run_controls WHERE run_id IN (SELECT id FROM runs WHERE session_id=?1)",[id])?;
                tx.execute("DELETE FROM deliveries WHERE run_id IN (SELECT id FROM runs WHERE session_id=?1)",[id])?;
                tx.execute("DELETE FROM submissions WHERE coalesce(session_id,(SELECT session_id FROM active_sessions WHERE conversation_id=submissions.conversation_id),conversation_id)=?1 OR run_id IN (SELECT id FROM runs WHERE session_id=?1)",[id])?;
                tx.execute("DELETE FROM inbox_controls WHERE conversation_id IN (SELECT conversation_id FROM active_sessions WHERE session_id=?1) OR (conversation_id=?1 AND NOT EXISTS(SELECT 1 FROM active_sessions WHERE conversation_id=?1))",[id])?;
                tx.execute("DELETE FROM active_sessions WHERE session_id=?1",[id])?;
                tx.execute("DELETE FROM runs WHERE session_id=?1",[id])?;
                tx.execute("DELETE FROM compaction_jobs WHERE session_id=?1",[id])?;
                tx.execute("DELETE FROM sessions WHERE id=?1",[id])?;
            }
            tx.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key IN ('run_view_revision','web_titles_revision')", [])?;
            tx.commit()?;Ok(ids)
        }).await.map_err(|e|if e.is::<crate::agent::inbox::AdmissionRejected>() {error(StatusCode::CONFLICT,&e.to_string())} else {internal(e)})?;
        for id in ids {
            match tokio::fs::remove_dir_all(chat.attachment_directory.join(id)).await {
                Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(internal(e.into()))
            }
        }
        Ok(StatusCode::NO_CONTENT)
    }).await.map_err(|e|internal(e.into()))?
}

#[cfg(test)]
mod shared_store_tests {
    use super::*;
    use crate::{
        agent::{
            AssistantMessage, UserMessage,
            inbox::SubmissionStatus,
            run_state::{Checkpoint, RunPhase},
        },
        gateway::{IncomingMessage, InterfaceId},
        web::{Settings, auth::AuthStore},
    };

    fn state(chat: &Chat, data: &std::path::Path) -> WebState {
        WebState {
            streams: std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            settings: Settings {
                trusted_proxies: Vec::new(),
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://localhost/".parse().unwrap(),
            },
            auth: AuthStore::new(data),
            chat: Some(chat.clone()),
        }
    }

    async fn session(chat: &Chat, text: &str) -> Session {
        let mut session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        session.push(Message::User(UserMessage {
            content: text.into(),
            files: vec![],
            images: vec![],
        }));
        session.push(Message::Assistant(AssistantMessage {
            content: "saved answer".into(),
        }));
        chat.sessions.save(&session).await.unwrap();
        session
    }

    #[tokio::test]
    async fn shared_session_work_blocks_web_edit_delete_and_marks_list_busy() {
        for work in ["queued", "running", "Pending", "Sending"] {
            let dir = tempfile::tempdir().unwrap();
            let chat = crate::web::chat_tests::chat(dir.path()).await;
            let session = session(&chat, "Telegram topic").await;
            let channel = SessionId::new("telegram:bot:42");
            chat.sessions.select(&channel, session.id()).await.unwrap();
            if work == "queued" {
                let message =
                    IncomingMessage::new(InterfaceId("telegram:bot".into()), "42", "queued");
                chat.sessions
                    .submit(SubmissionRequest::new(&channel, message, BusyMode::Steer))
                    .await
                    .unwrap();
            } else {
                let mut checkpoint = Checkpoint::new(&chat.sessions, &channel, &session);
                checkpoint
                    .save(
                        &session,
                        if work == "running" {
                            RunPhase::NeedModel
                        } else {
                            RunPhase::Completed
                        },
                        None,
                    )
                    .await
                    .unwrap();
                if work == "Sending" {
                    chat.sessions.with_connection(|db| {
                        db.execute("UPDATE deliveries SET payload=CAST(json_set(payload,'$.status','Sending') AS BLOB)", [])?;
                        Ok(())
                    }).await.unwrap();
                }
            }
            let state = state(&chat, dir.path());
            let listed = list(
                Extension(state.clone()),
                Query(Search {
                    q: String::new(),
                    limit: 30,
                    offset: 0,
                }),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(listed[0]["title"], "Telegram topic", "{work}");
            assert_eq!(listed[0]["busy"], true, "{work}");
            let response = edit(
                Extension(state.clone()),
                Path((session.id().as_str().into(), 0)),
                Json(Edit {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    text: "edit".into(),
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(response.status(), StatusCode::CONFLICT, "{work}");
            let response = delete(Extension(state), Path(session.id().as_str().into()))
                .await
                .unwrap_err();
            assert_eq!(response.status(), StatusCode::CONFLICT, "{work}");
            assert!(chat.sessions.load(session.id()).await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn deleting_shared_session_keeps_other_history_from_same_telegram_channel() {
        let dir = tempfile::tempdir().unwrap();
        let chat = crate::web::chat_tests::chat(dir.path()).await;
        let deleted = session(&chat, "delete this").await;
        let kept = session(&chat, "keep this").await;
        let channel = SessionId::new("telegram:bot:42");
        let mut run_ids = Vec::new();
        for session in [&deleted, &kept] {
            chat.sessions.select(&channel, session.id()).await.unwrap();
            let mut checkpoint = Checkpoint::new(&chat.sessions, &channel, session);
            checkpoint
                .save(session, RunPhase::Completed, None)
                .await
                .unwrap();
            let mut record = chat
                .sessions
                .submit(SubmissionRequest::new(
                    &channel,
                    IncomingMessage::new(
                        InterfaceId("telegram:bot".into()),
                        "42",
                        "accepted input",
                    ),
                    BusyMode::Steer,
                ))
                .await
                .unwrap();
            record.status = SubmissionStatus::Done;
            record.run_id = Some(checkpoint.record.id.clone());
            record.session_id = Some(session.id().as_str().into());
            let run_id = checkpoint.record.id.clone();
            chat.sessions.with_connection(move |db| {
                db.execute("UPDATE submissions SET status='Done',run_id=?1,session_id=?2,payload=?3 WHERE id=?4", params![record.run_id,record.session_id,serde_json::to_vec(&record)?,record.id])?;
                db.execute("UPDATE deliveries SET payload=CAST(json_set(payload,'$.status','Sent') AS BLOB) WHERE run_id=?1", [&run_id])?;
                db.execute("INSERT INTO run_controls VALUES(?1,'old cancellation')", [&run_id])?;
                Ok(())
            }).await.unwrap();
            run_ids.push(checkpoint.record.id);
        }
        let alias = SessionId::new("cli:delete");
        chat.sessions.select(&alias, deleted.id()).await.unwrap();
        chat.sessions.with_connection(|db| {
            db.execute_batch("INSERT INTO inbox_controls VALUES('cli:delete','stopped'),('telegram:bot:42','stopped')")?;
            Ok(())
        }).await.unwrap();
        assert_eq!(
            delete(
                Extension(state(&chat, dir.path())),
                Path(deleted.id().as_str().into())
            )
            .await
            .unwrap(),
            StatusCode::NO_CONTENT
        );
        assert!(chat.sessions.load(deleted.id()).await.unwrap().is_none());
        assert_eq!(
            chat.sessions
                .load(kept.id())
                .await
                .unwrap()
                .unwrap()
                .messages()
                .len(),
            2
        );
        assert_eq!(chat.sessions.active_id(&channel).await.unwrap(), *kept.id());
        assert_eq!(chat.sessions.active_id(&alias).await.unwrap(), alias);
        chat.sessions.with_connection(move |db| {
            for table in ["runs", "run_controls", "deliveries", "submissions"] {
                let column = if table == "runs" { "id" } else { "run_id" };
                assert_eq!(db.query_row(&format!("SELECT count(*) FROM {table} WHERE {column}=?1"), [&run_ids[0]], |row| row.get::<_, i64>(0))?, 0, "{table}");
                assert_eq!(db.query_row(&format!("SELECT count(*) FROM {table} WHERE {column}=?1"), [&run_ids[1]], |row| row.get::<_, i64>(0))?, 1, "{table}");
            }
            assert_eq!(db.query_row("SELECT count(*) FROM inbox_controls WHERE conversation_id='cli:delete'", [], |row|row.get::<_, i64>(0))?, 0);
            assert_eq!(db.query_row("SELECT count(*) FROM inbox_controls WHERE conversation_id='telegram:bot:42'", [], |row|row.get::<_, i64>(0))?, 1);
            Ok(())
        }).await.unwrap();
    }
}
