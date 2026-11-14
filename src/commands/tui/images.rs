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
    ensure!(app.input_images.len() < 4, "At most 4 images per message");
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
        let file = tokio::fs::File::open(&path)
            .await
            .context("Cannot open image")?;
        let mut bytes = Vec::new();
        file.take((crate::agent::image::MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        let image = tokio::task::spawn_blocking(move || Image::from_bytes(&bytes)).await??;
        attach(app, image)
    }
    .await;
    if let Err(error) = result {
        app.set_copy_notice(format!("Image: {error:#}"));
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
        app.set_copy_notice(format!("Clipboard image: {error:#}"));
    }
}

pub(super) fn key(key: crossterm::event::KeyEvent) -> bool {
    key.kind == crossterm::event::KeyEventKind::Press
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == crossterm::event::KeyCode::Char('v')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tui::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
