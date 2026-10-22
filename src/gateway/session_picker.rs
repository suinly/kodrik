use super::session_commands::{personal_session, session_titles};
use super::*;
use crate::interfaces::InlineButton;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const PAGE_SIZE: usize = 10;

struct Entry {
    root: String,
    id: SessionId,
    title: String,
    updated: Option<DateTime<Utc>>,
    pinned: bool,
    busy: bool,
    current: bool,
}

fn token(message: &IncomingMessage, root: &str) -> String {
    let bytes = serde_json::to_vec(&(
        &message.source.interface.0,
        &message.source.conversation_id,
        root,
    ))
    .unwrap();
    format!("{:x}", Sha256::digest(bytes))[..32].to_owned()
}

fn button(text: impl Into<String>, data: impl Into<String>) -> InlineButton {
    InlineButton {
        text: text.into(),
        data: data.into(),
    }
}

impl GatewayRunner {
    async fn picker_entries(&self, message: &IncomingMessage) -> anyhow::Result<Vec<Entry>> {
        let language = self.message_language(message);
        let conversation = SessionId::new(format!(
            "{}:{}",
            message.source.interface, message.source.conversation_id
        ));
        let runner = self.runner_for(&conversation)?;
        let sessions: Vec<_> = runner
            .list_sessions()
            .await?
            .into_iter()
            .filter(personal_session)
            .collect();
        let titles = session_titles(runner).await?;
        let current = runner.load_session(conversation).await?.id().clone();
        let (roots, selected, pins, busy) = if let Some(store) = runner.sqlite_store() {
            store.with_connection(|db| {
                let exists = |table: &str| -> rusqlite::Result<bool> { db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |row| row.get(0)) };
                let pairs = |sql: &str| -> rusqlite::Result<HashMap<String, String>> {
                    db.prepare(sql)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
                };
                let roots = if exists("web_chat_branches")? { pairs("SELECT session_id,root_id FROM web_chat_branches")? } else { HashMap::new() };
                let selected = if exists("web_chat_selection")? { pairs("SELECT root_id,session_id FROM web_chat_selection")? } else { HashMap::new() };
                let pins: HashSet<String> = if exists("web_chat_pins")? { db.prepare("SELECT root_id FROM web_chat_pins")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()? } else { HashSet::new() };
                let busy = db.prepare("SELECT session_id FROM runs WHERE terminal=0 UNION SELECT coalesce(s.session_id,a.session_id,s.conversation_id) FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id WHERE s.status='Queued' UNION SELECT r.session_id FROM deliveries d JOIN runs r ON r.id=d.run_id WHERE json_extract(d.payload,'$.status') IN ('Pending','Sending')")?.query_map([], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
                Ok((roots, selected, pins, busy))
            }).await?
        } else {
            Default::default()
        };
        let mut groups: HashMap<String, Vec<_>> = HashMap::new();
        for session in &sessions {
            let root = roots
                .get(session.id().as_str())
                .map(String::as_str)
                .unwrap_or(session.id().as_str());
            groups.entry(root.to_owned()).or_default().push(session);
        }
        let mut entries = Vec::new();
        for (root, group) in groups {
            let active = group.iter().any(|s| s.id() == &current);
            let preferred = if active {
                current.as_str()
            } else {
                selected.get(&root).map(String::as_str).unwrap_or(&root)
            };
            let shown = group
                .iter()
                .find(|s| s.id().as_str() == preferred)
                .unwrap_or(&group[0]);
            let title = titles.get(&root).cloned().unwrap_or_else(|| {
                shown
                    .messages()
                    .iter()
                    .find_map(|message| match message {
                        crate::agent::message::Message::User(user)
                            if !user.content.trim().is_empty() =>
                        {
                            Some(
                                user.content
                                    .split_whitespace()
                                    .collect::<Vec<_>>()
                                    .join(" ")
                                    .chars()
                                    .take(80)
                                    .collect(),
                            )
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| language.pick("New session", "Новый диалог").into())
            });
            entries.push(Entry {
                id: shown.id().clone(),
                title,
                updated: group.iter().filter_map(|s| s.last_message_at()).max(),
                pinned: pins.contains(&root),
                busy: group.iter().any(|s| busy.contains(s.id().as_str())),
                current: active,
                root,
            });
        }
        entries.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated.cmp(&a.updated))
                .then(a.root.cmp(&b.root))
        });
        Ok(entries)
    }

    pub(super) async fn send_session_picker(
        &self,
        message: &IncomingMessage,
        args: &[&str],
    ) -> anyhow::Result<()> {
        let interface = self
            .interfaces
            .iter()
            .find(|i| i.id() == message.source.interface)
            .context("Telegram interface unavailable")?;
        let language = self.message_language(message);
        let page = match args {
            [] => 1,
            [page] if page.parse::<usize>().is_ok_and(|page| page > 0) => page.parse()?,
            _ => {
                interface
                    .send(
                        &message.source.conversation_id,
                        language.pick(
                            "Use /sessions [page number], starting at 1.",
                            "Используйте /sessions [номер страницы], начиная с 1.",
                        ),
                    )
                    .await?;
                return Ok(());
            }
        };
        let entries = self.picker_entries(message).await?;
        if page > entries.len().div_ceil(PAGE_SIZE).max(1) {
            interface
                .send(
                    &message.source.conversation_id,
                    language.pick(
                        "Page not found. Open /sessions.",
                        "Страница не найдена. Откройте /sessions.",
                    ),
                )
                .await?;
            return Ok(());
        }
        let (text, buttons) = self.picker_view(message, &entries, page, None);
        interface
            .send_buttons(&message.source.conversation_id, &text, &buttons)
            .await?;
        Ok(())
    }

    fn picker_view(
        &self,
        message: &IncomingMessage,
        entries: &[Entry],
        page: usize,
        notice: Option<&str>,
    ) -> (String, Vec<Vec<InlineButton>>) {
        let language = self.message_language(message);
        let pages = entries.len().div_ceil(PAGE_SIZE).max(1);
        let page = page.clamp(1, pages);
        let mut text = format!(
            "{} — {page}/{pages}\n{}: {}",
            language.pick("Sessions", "Диалоги"),
            language.pick("Current", "Текущий"),
            entries
                .iter()
                .find(|entry| entry.current)
                .map(|entry| entry.title.as_str())
                .unwrap_or(language.pick("none selected", "не выбран"))
        );
        if entries.is_empty() {
            text.push_str(language.pick(
                "\n\nNo saved sessions yet.",
                "\n\nСохранённых диалогов пока нет.",
            ));
        }
        if let Some(notice) = notice {
            text.push_str(&format!("\n\n{notice}"));
        }
        let timezone = self
            .telegram_spaces
            .as_ref()
            .and_then(|spaces| {
                spaces
                    .config
                    .companion
                    .timezone
                    .parse::<chrono_tz::Tz>()
                    .ok()
            })
            .unwrap_or(chrono_tz::UTC);
        let today = Utc::now().with_timezone(&timezone).date_naive();
        let mut rows = Vec::new();
        for entry in entries.iter().skip((page - 1) * PAGE_SIZE).take(PAGE_SIZE) {
            let date = entry
                .updated
                .map(|date| {
                    let date = date.with_timezone(&timezone).date_naive();
                    if date == today {
                        language.pick("today", "сегодня").into()
                    } else if Some(date) == today.pred_opt() {
                        language.pick("yesterday", "вчера").into()
                    } else {
                        date.format("%d.%m.%y").to_string()
                    }
                })
                .unwrap_or_else(|| language.pick("new", "новый").into());
            let title: String = entry
                .title
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(42)
                .collect();
            let label = format!(
                "{}{}{}{title} · {date}",
                if entry.current { "✓ " } else { "" },
                if entry.pinned { "📌 " } else { "" },
                if entry.busy { "⏳ " } else { "" }
            );
            rows.push(vec![button(
                label,
                format!("cs:s:{}:{page}", token(message, &entry.root)),
            )]);
        }
        let mut navigation = Vec::new();
        if page > 1 {
            navigation.push(button(
                language.pick("‹ Back", "‹ Назад"),
                format!("cs:p:{}", page - 1),
            ));
        }
        if page < pages {
            navigation.push(button(
                language.pick("Next ›", "Далее ›"),
                format!("cs:p:{}", page + 1),
            ));
        }
        if !navigation.is_empty() {
            rows.push(navigation);
        }
        rows.push(vec![button(
            language.pick("＋ New session", "＋ Новый диалог"),
            "cs:n",
        )]);
        (text, rows)
    }

    pub(super) async fn handle_session_callback(
        &self,
        message: &IncomingMessage,
    ) -> anyhow::Result<()> {
        let callback = message
            .callback
            .as_ref()
            .context("Missing session callback")?;
        let interface = self
            .interfaces
            .iter()
            .find(|i| i.id() == message.source.interface)
            .context("Telegram interface unavailable")?;
        interface.answer_callback(&callback.id, "").await?;
        if message.is_group
            || message.guest_query_id.is_some()
            || !message
                .source
                .conversation_id
                .parse::<u64>()
                .is_ok_and(|id| id > 0)
        {
            return Ok(());
        }
        let language = self.message_language(message);
        let conversation = SessionId::new(format!(
            "{}:{}",
            message.source.interface, message.source.conversation_id
        ));
        let runner = self.runner_for(&conversation)?;
        let mut entries = self.picker_entries(message).await?;
        let mut page = 1;
        let mut notice = None;
        let mut close_menu = false;
        let action: Vec<_> = callback.data.split(':').collect();
        match action.as_slice() {
            ["cs", "p", value] if value.parse::<usize>().is_ok_and(|page| page > 0) => {
                page = value.parse()?
            }
            ["cs", "s", key, value] if value.parse::<usize>().is_ok_and(|page| page > 0) => {
                page = value.parse()?;
                if let Some(entry) = entries
                    .iter()
                    .find(|entry| token(message, &entry.root) == *key)
                {
                    let busy = entry.busy
                        || entries.iter().any(|entry| entry.current && entry.busy)
                        || runner.conversation_busy(&conversation).await
                        || if let Some(store) = runner.sqlite_store() {
                            store.inbox_busy(conversation.as_str()).await?
                        } else {
                            false
                        };
                    let result = if busy {
                        Err(anyhow::anyhow!("Session is busy"))
                    } else {
                        runner.select_session(&conversation, &entry.id).await
                    };
                    match result {
                        Ok(_) => {
                            notice = Some(format!(
                                "{}: {}",
                                language.pick("Selected session", "Выбран диалог"),
                                entry.title
                            ));
                            close_menu = true;
                        }
                        Err(error) => {
                            log::warn!("Telegram session selection rejected: {error:#}");
                            notice = Some(language.pick("Could not switch sessions. It may be busy; wait until it finishes or send /stop.", "Не удалось переключить диалог. Он может быть занят; дождитесь завершения или отправьте /stop.").into());
                        }
                    }
                } else {
                    notice = Some(language.pick("The session was deleted or is unavailable. The list has been refreshed.", "Диалог удалён или недоступен. Список обновлён.").into());
                }
                entries = self.picker_entries(message).await?;
            }
            ["cs", "n"] => {
                let mut control = message.clone();
                control.callback = None;
                control.text = "/new".into();
                runner.apply_control(&conversation, &control).await?;
                notice = Some(
                    language
                        .pick("New session created.", "Новый диалог создан.")
                        .into(),
                );
                close_menu = true;
                entries = self.picker_entries(message).await?;
            }
            _ => {
                notice = Some(
                    language
                        .pick(
                            "This menu is outdated. The list has been refreshed.",
                            "Меню устарело. Список обновлён.",
                        )
                        .into(),
                )
            }
        }
        let (text, buttons) = if close_menu {
            (notice.unwrap_or_default(), Vec::new())
        } else {
            self.picker_view(message, &entries, page, notice.as_deref())
        };
        match interface
            .edit_buttons(
                &message.source.conversation_id,
                &callback.message_id,
                &text,
                &buttons,
            )
            .await
        {
            Err(error)
                if error
                    .downcast_ref::<crate::interfaces::SendRejection>()
                    .is_some_and(|rejection| {
                        rejection.error_code == 400
                            && (rejection.description.contains("message to edit not found")
                                || rejection.description.contains("message can't be edited"))
                    }) =>
            {
                interface
                    .send_buttons(&message.source.conversation_id, &text, &buttons)
                    .await?;
            }
            result => result?,
        }
        Ok(())
    }
}
