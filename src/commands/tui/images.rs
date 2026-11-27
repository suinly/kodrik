use super::app::App;
use crate::{
    agent::Image,
    gateway::{IncomingMessage, InterfaceId},
};
use anyhow::{Context, ensure};
use tokio::io::AsyncReadExt;

fn accepts(app: &App) -> bool {
    app.sessions.is_none()
        && app.remote_grant.is_none()
        && app.approval.is_none()
        && app.audit_picker.is_none()
        && app.model_picker.is_none()
        && app.language_choice.is_none()
        && app.navigation.selected.is_none()
        && app.navigation.editing.is_none()
}

pub(super) fn message(app: &App, session: &str, text: String) -> IncomingMessage {
    let mut message = IncomingMessage::new(
        InterfaceId("cli".into()),
        session.strip_prefix("cli:").unwrap_or(session),
        text,
    );
    message.images = app.input_images.clone();
    message
}

fn attach(app: &mut App, image: Image) -> anyhow::Result<()> {
    ensure!(
        app.input_images.len() < 4,
        app.language.pick(
            "At most 4 images per message",
            "Не более 4 изображений в сообщении"
        )
    );
    app.snap_image_cursor(true);
    let start = app.input_cursor;
    app.next_image_id += 1;
    let id = app.next_image_id;
    app.insert_input(&format!("[Image {id}]"));
    let index = app
        .image_markers
        .partition_point(|(range, _)| range.start < start);
    app.image_markers
        .insert(index, (start..app.input_cursor, id));
    app.input_images.insert(index, image);
    Ok(())
}

pub(super) async fn paste(app: &mut App, text: &str) {
    if !accepts(app) {
        if app.sessions.is_some() || app.remote_grant.is_some() {
            return;
        }
        app.insert_input(text);
        return;
    }
    let raw = text.trim();
    let raw = raw
        .strip_prefix("'")
        .and_then(|s| s.strip_suffix("'"))
        .or_else(|| raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(raw);
    let raw = raw.replace("\\ ", " ");
    let path = std::path::PathBuf::from(&raw);
    let is_image = path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        ["png", "jpg", "jpeg", "webp"]
            .iter()
            .any(|ext| s.eq_ignore_ascii_case(ext))
    });
    if !is_image || !path.is_absolute() {
        app.insert_input(text);
        return;
    }
    let result = async {
        let file = tokio::fs::File::open(&path).await.context(
            app.language
                .pick("Cannot open image", "Не удалось открыть изображение"),
        )?;
        let mut bytes = Vec::new();
        file.take((crate::agent::image::MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        let image = tokio::task::spawn_blocking(move || Image::from_bytes(&bytes)).await??;
        attach(app, image)
    }
    .await;
    if let Err(error) = result {
        app.set_copy_notice(format!(
            "{}: {error:#}",
            app.language.pick("Image", "Изображение")
        ));
    }
}

pub(super) async fn clipboard(app: &mut App) {
    if !accepts(app) {
        return;
    }
    let result = async {
        let bytes = super::clipboard::image_bytes().await?;
        let image = tokio::task::spawn_blocking(move || Image::from_bytes(&bytes)).await??;
        attach(app, image)
    }
    .await;
    if let Err(error) = result {
        app.set_copy_notice(format!(
            "{}: {error:#}",
            app.language
                .pick("Clipboard image", "Изображение из буфера обмена")
        ));
    }
}

pub(super) fn key(key: crossterm::event::KeyEvent) -> bool {
    key.kind == crossterm::event::KeyEventKind::Press
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
        && matches!(
            key.code,
            crossterm::event::KeyCode::Char('v' | 'V' | 'м' | 'М')
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tui::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn ctrl_v_recognizes_russian_layout_without_intercepting_altgr() {
        assert!(key(KeyEvent::new(
            KeyCode::Char('м'),
            KeyModifiers::CONTROL
        )));
        assert!(!key(KeyEvent::new(KeyCode::Char('м'), KeyModifiers::NONE)));
        assert!(!key(KeyEvent::new(
            KeyCode::Char('м'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )));
    }

    #[tokio::test]
    async fn bilingual_image_errors_keep_draft_and_markers() {
        let dir = tempfile::tempdir().unwrap();
        for (language, prefix, open, limit) in [
            (
                crate::i18n::Language::En,
                "Image:",
                "Cannot open image",
                "At most 4 images",
            ),
            (
                crate::i18n::Language::Ru,
                "Изображение:",
                "Не удалось открыть изображение",
                "Не более 4 изображений",
            ),
        ] {
            let mut app = App {
                language,
                ..Default::default()
            };
            app.insert_input("draft");
            paste(&mut app, dir.path().join("missing.png").to_str().unwrap()).await;
            let notice = &app.copy_notice.as_ref().unwrap().0;
            assert!(notice.starts_with(prefix), "{notice}");
            assert!(notice.contains(open), "{notice}");
            assert_eq!(app.input, "draft");
            let image =
                Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
                    .unwrap();
            for _ in 0..4 {
                attach(&mut app, image.clone()).unwrap();
            }
            assert!(app.input.contains("[Image 1]"));
            let error = attach(&mut app, image).unwrap_err().to_string();
            assert!(error.contains(limit), "{error}");
        }
    }

    #[test]
    fn images_without_complete_inline_markers_keep_the_count() {
        for (text, count, expected) in [
            ("plain", 0, "plain"),
            ("photo", 1, "photo\n[Images: 1]"),
            ("", 1, "\n[Images: 1]"),
            ("[Image x]", 1, "[Image x]\n[Images: 1]"),
            ("[Image 0]", 1, "[Image 0]\n[Images: 1]"),
            ("[Image 1", 1, "[Image 1\n[Images: 1]"),
            ("[Image 3]", 2, "[Image 3]\n[Images: 2]"),
        ] {
            assert_eq!(super::super::waiting_summary(text, count), expected);
        }
    }

    #[test]
    fn sent_images_keep_only_inline_markers() {
        let image = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
            .unwrap();
        for (prefix, count, expected) in [
            ("", 1, "[Image 3]"),
            ("Compare ", 2, "Compare [Image 3][Image 4]"),
        ] {
            let mut app = App::default();
            app.next_image_id = 2;
            app.insert_input(prefix);
            for _ in 0..count {
                attach(&mut app, image.clone()).unwrap();
            }
            assert_eq!(
                app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Some(expected.into())
            );
            assert_eq!(app.waiting_messages, [expected]);
            let message = message(&app, "cli:main", expected.into());
            assert_eq!(message.images.len(), count);
            super::super::reconcile_waiting(
                &mut app,
                &[serde_json::json!({"text": expected, "photos": count})],
            );
            assert_eq!(app.waiting_messages, [expected]);
            app.consume_pending_inputs();
            assert_eq!(app.messages[0].content, expected);
        }
    }

    #[test]
    fn restored_images_keep_only_inline_markers() {
        let image = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
            .unwrap();
        let mut session = super::super::Session::new(super::super::SessionId::new("inline-images"));
        for text in ["[Image 3]", "Compare [Image 3][Image 4]"] {
            session.push(super::super::Message::User(crate::agent::UserMessage {
                browser_client_id: None,
                browser_context: None,
                content: text.into(),
                images: vec![image.clone(); if text.starts_with("Compare") { 2 } else { 1 }],
                files: vec![],
            }));
        }
        let mut app = App::default();
        super::super::restore_history(&mut app, &session);
        assert_eq!(app.messages[0].content, "[Image 3]");
        assert_eq!(app.messages[1].content, "Compare [Image 3][Image 4]");
    }

    #[test]
    fn atomic_image_deletion_keeps_ids_and_literal_text() {
        let mut app = App::default();
        let image = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
            .unwrap();
        app.insert_input("[Image 1]я");
        attach(&mut app, image.clone()).unwrap();
        attach(&mut app, image.clone()).unwrap();
        app.update(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, "[Image 1]я[Image 1]".len());
        app.update(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "[Image 1]я[Image 2]");
        assert_eq!(app.input_images.len(), 1);
        app.input_cursor = app.input.len();
        attach(&mut app, image).unwrap();
        assert_eq!(app.input, "[Image 1]я[Image 2][Image 3]");
        app.input_cursor = "[Image 1]я".len();
        app.update(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.input, "[Image 1]я[Image 3]");
        assert_eq!(app.input_images.len(), 1);
    }

    #[test]
    fn shifted_markers_delete_the_matching_payload_and_survive_failed_send() {
        let mut app = App::default();
        let first = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
            .unwrap();
        let second = Image::from_bytes(&crate::agent::image::tests::sample(
            image::ImageFormat::Jpeg,
        ))
        .unwrap();
        attach(&mut app, first.clone()).unwrap();
        app.input_cursor = 0;
        attach(&mut app, second.clone()).unwrap();
        assert_eq!(app.input, "[Image 2][Image 1]");
        assert_eq!(app.input_images[0].data_url, second.data_url);
        app.input_cursor = 0;
        app.insert_input("я ");
        app.update(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "я[Image 2][Image 1]");
        app.update(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.input, "я[Image 1]");
        assert_eq!(app.input_images[0].data_url, first.data_url);
        let text = app
            .update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        // Submission failure restores this exact draft, retaining attachment metadata.
        app.input = text;
        app.input_cursor = app.input.len();
        app.update(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "я");
        assert!(
            message(&app, "cli:main", app.input.clone())
                .images
                .is_empty()
        );
        app.clear_input_images();
        attach(&mut app, first).unwrap();
        assert_eq!(app.input, "я[Image 1]");
    }

    #[test]
    fn wrapped_image_markers_cannot_be_split_by_cursor_or_paste() {
        let mut app = App::default();
        attach(
            &mut app,
            Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
                .unwrap(),
        )
        .unwrap();
        app.input_width = 4;
        app.update(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, 0);
        app.update(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.input_cursor == 0 || app.input_cursor == 9);
        app.input_cursor = 4;
        app.insert_input("hello");
        assert_eq!(app.input, "[Image 1]hello");
        app.input_cursor = 0;
        app.update(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.input_cursor, 9);
        app.update(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "hello");
        assert!(app.input_images.is_empty());
    }

    #[test]
    fn image_draft_is_not_replaced_by_history() {
        let mut app = App::default();
        app.push_user_message("old".into());
        attach(
            &mut app,
            Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
                .unwrap(),
        )
        .unwrap();
        assert!(!app.recall_input(true));
        app.select_user_turn(0);
        assert!(app.navigation.selected.is_none());
        assert_eq!(app.input, "[Image 1]");
    }

    #[tokio::test]
    async fn pasted_path_attaches_validated_image_without_sending_it_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen shot.png");
        std::fs::write(
            &path,
            crate::agent::image::tests::sample(image::ImageFormat::Png),
        )
        .unwrap();
        let mut app = App::default();
        paste(&mut app, &format!("'{}'", path.display())).await;
        assert_eq!(app.input, "[Image 1]");
        assert_eq!(app.input_images.len(), 1);
        assert_eq!(
            (app.input_images[0].width, app.input_images[0].height),
            (2, 3)
        );
        assert_eq!(
            app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some("[Image 1]".into())
        );
        let message = message(&app, "cli:main", "".into());
        assert_eq!(message.images.len(), 1);
        assert!(message.text.is_empty());
    }

    #[tokio::test]
    async fn invalid_image_does_not_destroy_draft_and_plain_paste_stays_literal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.png");
        std::fs::write(&path, "not an image").unwrap();
        let mut app = App::default();
        app.insert_input("draft");
        paste(&mut app, path.to_str().unwrap()).await;
        assert_eq!(app.input, "draft");
        assert!(app.input_images.is_empty());
        assert!(app.copy_notice.is_some());
        paste(&mut app, "\nhello world").await;
        assert_eq!(app.input, "draft\nhello world");
    }

    #[test]
    fn image_markers_are_numbered_and_inserted_at_the_cursor() {
        let mut app = App::default();
        let image = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
            .unwrap();
        app.insert_input("before after");
        app.input_cursor = "before ".len();
        attach(&mut app, image.clone()).unwrap();
        attach(&mut app, image).unwrap();
        assert_eq!(app.input, "before [Image 1][Image 2]after");
        assert_eq!(app.input_cursor, "before [Image 1][Image 2]".len());
        assert_eq!(app.input_images.len(), 2);
    }

    #[test]
    fn attachment_limit_and_clear_keep_the_composer_usable() {
        let mut app = App::default();
        let image = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png))
            .unwrap();
        for _ in 0..4 {
            attach(&mut app, image.clone()).unwrap();
        }
        let draft = app.input.clone();
        assert!(attach(&mut app, image.clone()).is_err());
        assert_eq!(app.input, draft);
        app.update(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit);
        assert!(app.input_images.is_empty());
        assert!(app.input.is_empty());
        attach(&mut app, image).unwrap();
        assert_eq!(app.input, "[Image 1]");
    }

    #[tokio::test]
    async fn dialogs_and_editing_do_not_accept_new_images() {
        let mut app = App::default();
        app.language_choice = Some(0);
        paste(&mut app, "/tmp/screenshot.png").await;
        assert!(app.input_images.is_empty());
        assert!(app.input.is_empty());
    }
}
