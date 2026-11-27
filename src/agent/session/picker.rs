use super::{SessionId, SqliteSessionStore, branches};
use rusqlite::{functions::FunctionFlags, params};

pub(crate) const MAX_PAGE: usize = 100;
pub(crate) const MAX_QUERY: usize = 512;

pub(crate) enum Scope {
    Local(String),
    Remote {
        device: uuid::Uuid,
        directory: String,
    },
    Unfiltered,
}

pub(crate) struct Row {
    pub id: SessionId,
    pub root: String,
    pub title: Option<String>,
    pub pinned: bool,
    pub updated_at: Option<String>,
}

impl SqliteSessionStore {
    pub(crate) async fn picker_page(
        &self,
        scope: Scope,
        query: String,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<Row>> {
        self.init_titles().await?;
        let query = query.trim().to_lowercase();
        self.with_connection(move |db| {
            branches::init_branches(db)?;
            // SQLite's built-in lower() only handles ASCII.
            db.create_scalar_function("picker_contains", 2, FunctionFlags::SQLITE_DETERMINISTIC, |ctx| {
                Ok(ctx.get::<Option<String>>(0)?.is_some_and(|text| {
                    text.to_lowercase().contains(ctx.get_raw(1).as_str().unwrap_or_default())
                }))
            })?;
            let (device, directory) = match scope {
                Scope::Local(directory) => (None, Some(directory)),
                Scope::Remote { device, directory } => (Some(device.to_string()), Some(directory)),
                Scope::Unfiltered => (None, None),
            };
            let mut statement = db.prepare(
                "WITH eligible AS MATERIALIZED (
                    SELECT s.id,coalesce(b.root_id,s.id) AS root,s.updated_at,
                        json_extract(s.payload,'$.last_message_at') AS last_message_at
                    FROM sessions s LEFT JOIN web_chat_branches b ON b.session_id=s.id
                    WHERE s.id NOT GLOB 'companion:*' AND s.id NOT GLOB '*:guest'
                        AND NOT (s.id GLOB 'telegram:*' AND
                            substr(s.id,length(rtrim(s.id,replace(s.id,':','')))+1,1)='-')
                        AND json_extract(s.payload,'$.telegram_group') IS NULL
                        AND json_extract(s.payload,'$.webhook_instruction') IS NULL
                        AND (?1 IS NULL OR CASE WHEN ?2 IS NULL
                            THEN json_extract(s.payload,'$.tui_directory')=?1
                            ELSE s.id IN (SELECT rb.session_id FROM remote_bindings rb
                                WHERE rb.device_id=?2
                                    AND json_extract(rb.manifest,'$.project_root')=?1)
                        END)
                ), ranked AS (
                    SELECT e.*,t.title,p.root_id IS NOT NULL AS pinned,
                        max(e.updated_at) OVER (PARTITION BY e.root) AS activity,
                        max(e.last_message_at) OVER (PARTITION BY e.root) AS last_activity,
                        row_number() OVER (PARTITION BY e.root ORDER BY
                            (e.id=coalesce(selection.session_id,e.root)) DESC,e.updated_at DESC,e.id) AS rank
                    FROM eligible e LEFT JOIN sessions s ON s.id=e.id
                    LEFT JOIN web_chat_titles t ON t.conversation_id=e.root
                    LEFT JOIN web_chat_pins p ON p.root_id=e.root
                    LEFT JOIN web_chat_selection selection ON selection.root_id=e.root
                    WHERE ?3='' OR picker_contains(t.title,?3) OR picker_contains(e.id,?3)
                        OR EXISTS(SELECT 1 FROM json_each(s.payload,'$.messages') m
                            WHERE picker_contains(coalesce(json_extract(m.value,'$.User.content'),
                                json_extract(m.value,'$.Assistant.content')),?3))
                )
                SELECT id,root,title,pinned,last_activity FROM ranked
                WHERE rank=1 ORDER BY pinned DESC,activity DESC,root LIMIT ?4 OFFSET ?5",
            )?;
            let mut fallback = db.prepare(
                "SELECT json_extract(message.value,'$.User.content')
                 FROM sessions s,json_each(s.payload,'$.messages') message
                 WHERE s.id=?1 AND json_type(message.value,'$.User.content')='text'
                 ORDER BY CAST(message.key AS INTEGER)",
            )?;
            let mut rows = statement.query(params![directory, device, query, limit as i64, offset as i64])?;
            let mut page = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let mut title: Option<String> = row.get(2)?;
                if title.is_none() {
                    let mut messages = fallback.query([&id])?;
                    while let Some(message) = messages.next()? {
                        let content: String = message.get(0)?;
                        let candidate: String = content.split_whitespace().collect::<Vec<_>>()
                            .join(" ").chars().take(80).collect();
                        if !candidate.is_empty() {
                            title = Some(candidate);
                            break;
                        }
                    }
                }
                page.push(Row { id: SessionId::new(id), root: row.get(1)?, title,
                    pinned: row.get(3)?, updated_at: row.get(4)? });
            }
            Ok(page)
        }).await
    }
}
