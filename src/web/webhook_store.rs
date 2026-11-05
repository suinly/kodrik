use crate::agent::{command_audit::CommandAuditMode, session::SqliteSessionStore};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub(crate) enum WebhookError {
    InvalidInput,
    NotFound,
    LimitReached,
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "Name must contain 1–200 bytes; instruction 1–16384 bytes",
            Self::NotFound => "Webhook not found",
            Self::LimitReached => "Webhook limit reached",
        })
    }
}
impl std::error::Error for WebhookError {}

pub(crate) async fn init(store: &SqliteSessionStore) -> anyhow::Result<()> {
    store.with_connection(|db| {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS webhooks (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, instruction TEXT NOT NULL,
            token_hash TEXT NOT NULL, enabled INTEGER NOT NULL, created_at TEXT NOT NULL,
            command_audit_mode TEXT NOT NULL DEFAULT 'review' CHECK(command_audit_mode IN ('trusted','review','ask')));
            CREATE TABLE IF NOT EXISTS webhook_receipts (
            webhook_id TEXT NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
            request_key TEXT NOT NULL, body_hash TEXT NOT NULL,
            session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL,
            accepted_at TEXT NOT NULL,
            PRIMARY KEY(webhook_id,request_key));
            CREATE INDEX IF NOT EXISTS webhook_receipts_time ON webhook_receipts(webhook_id,accepted_at);
            CREATE INDEX IF NOT EXISTS webhook_receipts_session ON webhook_receipts(webhook_id,session_id,accepted_at);")?;
        upgrade_audit_mode(&tx)?;
        tx.commit()?;
        Ok(())
    }).await
}

pub(crate) fn upgrade_audit_mode(db: &rusqlite::Connection) -> anyhow::Result<()> {
    let has_mode: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('webhooks') WHERE name='command_audit_mode')",
        [], |row| row.get(0),
    )?;
    if !has_mode {
        db.execute_batch("ALTER TABLE webhooks ADD COLUMN command_audit_mode TEXT NOT NULL DEFAULT 'review' CHECK(command_audit_mode IN ('trusted','review','ask'))")?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Webhook {
    id: String,
    name: String,
    instruction: String,
    enabled: bool,
    created_at: String,
    command_audit_mode: CommandAuditMode,
}

impl Webhook {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            instruction: row.get(2)?,
            enabled: row.get(3)?,
            created_at: row.get(4)?,
            command_audit_mode: row.get::<_, String>(5)?.parse().map_err(
                |error: anyhow::Error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        error.into(),
                    )
                },
            )?,
        })
    }
}

pub(crate) struct WebhookStore {
    sessions: SqliteSessionStore,
    base_url: reqwest::Url,
}

impl WebhookStore {
    pub(crate) fn new(sessions: SqliteSessionStore, base_url: reqwest::Url) -> Self {
        Self { sessions, base_url }
    }

    fn with_url(&self, hook: Webhook) -> Value {
        let url = self
            .base_url
            .join(&format!("api/webhooks/{}", hook.id))
            .expect("Webhook URL");
        let mut value = serde_json::to_value(hook).expect("Webhook serialization");
        value["url"] = json!(url.as_str());
        value
    }

    pub(crate) async fn list(&self) -> anyhow::Result<Value> {
        let hooks = self.sessions.with_connection(|db| {
            Ok(db.prepare("SELECT id,name,instruction,enabled,created_at,command_audit_mode FROM webhooks ORDER BY created_at DESC,id")?
                .query_map([], Webhook::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await?;
        Ok(json!(
            hooks
                .into_iter()
                .map(|hook| self.with_url(hook))
                .collect::<Vec<_>>()
        ))
    }

    pub(crate) async fn get(&self, id: String) -> anyhow::Result<Value> {
        let hook = self
            .sessions
            .with_connection(move |db| {
                Ok(db
                    .query_row(
                        "SELECT id,name,instruction,enabled,created_at,command_audit_mode FROM webhooks WHERE id=?1",
                        [id],
                        Webhook::from_row,
                    )
                    .optional()?)
            })
            .await?
            .ok_or(WebhookError::NotFound)?;
        Ok(self.with_url(hook))
    }

    pub(crate) async fn create(
        &self,
        name: String,
        instruction: String,
        enabled: bool,
        command_audit_mode: CommandAuditMode,
    ) -> anyhow::Result<Value> {
        validate(Some(&name), Some(&instruction))?;
        let hook = Webhook {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().into(),
            instruction,
            enabled,
            created_at: chrono::Utc::now().to_rfc3339(),
            command_audit_mode,
        };
        let token = new_token();
        let digest = hash(&token);
        let hook = self
            .sessions
            .with_connection(move |db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if tx.query_row("SELECT count(*) FROM webhooks", [], |row| {
                    row.get::<_, i64>(0)
                })? >= 32
                {
                    return Err(WebhookError::LimitReached.into());
                }
                tx.execute(
                    "INSERT INTO webhooks(id,name,instruction,token_hash,enabled,created_at,command_audit_mode) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        hook.id,
                        hook.name,
                        hook.instruction,
                        digest,
                        hook.enabled,
                        hook.created_at,
                        hook.command_audit_mode.code()
                    ],
                )?;
                tx.commit()?;
                Ok(hook)
            })
            .await?;
        let mut result = self.with_url(hook);
        result["token"] = json!(token);
        Ok(result)
    }

    pub(crate) async fn update(
        &self,
        id: String,
        name: Option<String>,
        instruction: Option<String>,
        enabled: Option<bool>,
        command_audit_mode: Option<CommandAuditMode>,
    ) -> anyhow::Result<Value> {
        validate(name.as_deref(), instruction.as_deref())?;
        let hook = self.sessions.with_connection(move |db| {
            Ok(db.query_row("UPDATE webhooks SET name=coalesce(?2,name),instruction=coalesce(?3,instruction),enabled=coalesce(?4,enabled),command_audit_mode=coalesce(?5,command_audit_mode) WHERE id=?1 RETURNING id,name,instruction,enabled,created_at,command_audit_mode",
                params![id, name.as_deref().map(str::trim), instruction, enabled, command_audit_mode.map(CommandAuditMode::code)], Webhook::from_row).optional()?)
        }).await?.ok_or(WebhookError::NotFound)?;
        Ok(self.with_url(hook))
    }

    pub(crate) async fn rotate(&self, id: String) -> anyhow::Result<Value> {
        let token = new_token();
        let digest = hash(&token);
        let changed = self
            .sessions
            .with_connection(move |db| {
                Ok(db.execute(
                    "UPDATE webhooks SET token_hash=?2 WHERE id=?1",
                    params![id, digest],
                )?)
            })
            .await?;
        if changed == 0 {
            return Err(WebhookError::NotFound.into());
        }
        Ok(json!({"token":token}))
    }

    pub(crate) async fn delete(&self, id: String) -> anyhow::Result<()> {
        let changed = self
            .sessions
            .with_connection(move |db| Ok(db.execute("DELETE FROM webhooks WHERE id=?1", [id])?))
            .await?;
        if changed == 0 {
            return Err(WebhookError::NotFound.into());
        }
        Ok(())
    }

    pub(crate) async fn events(&self, id: String) -> anyhow::Result<Value> {
        let events = self.sessions.with_connection(move |db| {
            Ok(db.prepare("SELECT w.session_id,w.accepted_at,coalesce(s.status,'Deleted') FROM webhook_receipts w
                LEFT JOIN submissions s ON s.conversation_id=w.session_id
                    AND s.request_id='webhook:'||w.webhook_id||':'||w.request_key
                WHERE w.webhook_id=?1 ORDER BY w.accepted_at DESC LIMIT 30")?
                .query_map([id], |row| Ok(json!({"conversation_id":row.get::<_,Option<String>>(0)?,"accepted_at":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?})))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }).await?;
        Ok(json!(events))
    }
}

fn validate(name: Option<&str>, instruction: Option<&str>) -> Result<(), WebhookError> {
    if name.is_some_and(|name| name.trim().is_empty() || name.len() > 200)
        || instruction.is_some_and(|instruction| {
            instruction.trim().is_empty() || instruction.len() > 16 * 1024
        })
    {
        return Err(WebhookError::InvalidInput);
    }
    Ok(())
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub(crate) fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
