use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutgoingMessage {
    Photo {
        media: MediaSource,
        caption: Option<String>,
    },
    PhotoAlbum {
        /// 2–10 photos in display order.
        media: Vec<MediaSource>,
        /// Shared caption, shown on the first photo only.
        caption: Option<String>,
    },
    Video {
        media: MediaSource,
        caption: Option<String>,
        /// JPEG thumbnail URL required for guest URL videos; unused for ordinary sends.
        thumbnail_url: Option<String>,
    },
    Document {
        media: MediaSource,
        caption: Option<String>,
    },
    Audio {
        media: MediaSource,
        caption: Option<String>,
    },
    Poll {
        question: String,
        options: Vec<String>,
        multiple_answers: Option<bool>,
        anonymous: Option<bool>,
    },
    Location {
        latitude: f64,
        longitude: f64,
    },
    Post {
        title: String,
        body: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum MediaSource {
    File {
        path: String,
    },
    Url {
        url: String,
    },
    Telegram {
        file_id: String,
    },
    #[serde(skip_deserializing)]
    #[schemars(skip)]
    Upload {
        filename: String,
        data: Vec<u8>,
    },
}

impl OutgoingMessage {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Photo { media, caption }
            | Self::Video { media, caption, .. }
            | Self::Document { media, caption }
            | Self::Audio { media, caption } => {
                let limit = if matches!(self, Self::Photo { .. }) {
                    10_000_000
                } else {
                    50_000_000
                };
                validate_media(media, caption.as_deref(), limit)?;
                if let Self::Video {
                    thumbnail_url: Some(url),
                    ..
                } = self
                {
                    validate_url(url)?;
                }
            }
            Self::PhotoAlbum { media, caption } => {
                anyhow::ensure!(
                    (2..=10).contains(&media.len()),
                    "Photo album must have 2–10 photos"
                );
                for source in media {
                    validate_media(source, caption.as_deref(), 10_000_000)?;
                }
            }
            Self::Poll {
                question, options, ..
            } => {
                bounded(question, "Poll question", 300)?;
                anyhow::ensure!(
                    (1..=12).contains(&options.len()),
                    "Poll must have 1–12 options"
                );
                for option in options {
                    bounded(option, "Poll option", 100)?;
                }
            }
            Self::Location {
                latitude,
                longitude,
            } => {
                anyhow::ensure!(
                    latitude.is_finite() && (-90.0..=90.0).contains(latitude),
                    "Latitude must be between -90 and 90"
                );
                anyhow::ensure!(
                    longitude.is_finite() && (-180.0..=180.0).contains(longitude),
                    "Longitude must be between -180 and 180"
                );
            }
            Self::Post { title, body } => {
                bounded(title, "Post title", 255)?;
                nonempty(body, "Post body")?;
            }
        }
        Ok(())
    }

    pub fn supports_guest(&self) -> bool {
        match self {
            Self::Poll { .. } | Self::PhotoAlbum { .. } => false,
            Self::Photo { media, .. }
            | Self::Video { media, .. }
            | Self::Document { media, .. }
            | Self::Audio { media, .. } => matches!(
                media,
                MediaSource::Url { .. } | MediaSource::Telegram { .. }
            ),
            _ => true,
        }
    }

    pub fn media_mut(&mut self) -> Option<&mut MediaSource> {
        match self {
            Self::Photo { media, .. }
            | Self::Video { media, .. }
            | Self::Document { media, .. }
            | Self::Audio { media, .. } => Some(media),
            _ => None,
        }
    }

    pub(crate) fn media_sources_mut(&mut self) -> &mut [MediaSource] {
        match self {
            Self::PhotoAlbum { media, .. } => media,
            _ => self
                .media_mut()
                .map(std::slice::from_mut)
                .unwrap_or_default(),
        }
    }
}

fn validate_media(media: &MediaSource, caption: Option<&str>, limit: usize) -> Result<()> {
    match media {
        MediaSource::File { path } => nonempty(path, "File path")?,
        MediaSource::Url { url } => validate_url(url)?,
        MediaSource::Telegram { file_id } => nonempty(file_id, "Telegram file ID")?,
        MediaSource::Upload { filename, data } => {
            nonempty(filename, "Upload filename")?;
            anyhow::ensure!(
                !data.is_empty() && data.len() <= limit,
                "Upload must contain 1–{limit} bytes"
            );
        }
    }
    if let Some(caption) = caption {
        anyhow::ensure!(
            caption.chars().count() <= 1024,
            "Caption exceeds 1024 characters"
        );
    }
    Ok(())
}

fn nonempty(value: &str, name: &str) -> Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "{name} must not be empty");
    Ok(())
}

fn bounded(value: &str, name: &str, limit: usize) -> Result<()> {
    nonempty(value, name)?;
    anyhow::ensure!(
        value.chars().count() <= limit,
        "{name} exceeds {limit} characters"
    );
    Ok(())
}

fn validate_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("Media URL is invalid"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Media URL must use HTTP(S)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn photo_album_validates_count_caption_and_every_source() {
        let source = json!({"source":"telegram","file_id":"photo"});
        for (count, valid) in [(0, false), (1, false), (2, true), (10, true), (11, false)] {
            let message: OutgoingMessage = serde_json::from_value(json!({
                "type":"photo_album", "media":vec![source.clone();count],
                "caption":"я".repeat(1024),
            }))
            .unwrap();
            assert_eq!(message.validate().is_ok(), valid, "count: {count}");
            assert!(!message.supports_guest());
        }
        for (second, caption) in [
            (source.clone(), "я".repeat(1025)),
            (json!({"source":"telegram","file_id":" "}), String::new()),
            (
                json!({"source":"url","url":"file:///secret"}),
                String::new(),
            ),
            (json!({"source":"file","path":""}), String::new()),
        ] {
            let message: OutgoingMessage = serde_json::from_value(json!({
                "type":"photo_album", "media":[source.clone(),second], "caption":caption,
            }))
            .unwrap();
            assert!(message.validate().is_err());
        }
    }

    #[test]
    fn photo_album_enforces_photo_upload_limits_for_every_member() {
        for (filename, size, valid) in [
            ("photo.jpg", 0, false),
            ("photo.jpg", 1, true),
            ("photo.jpg", 10_000_000, true),
            ("photo.jpg", 10_000_001, false),
            (" ", 1, false),
        ] {
            let mut message: OutgoingMessage = serde_json::from_value(json!({
                "type":"photo_album", "media":[
                    {"source":"telegram","file_id":"first"},
                    {"source":"telegram","file_id":"second"}
                ], "caption":null
            }))
            .unwrap();
            message.media_sources_mut()[1] = MediaSource::Upload {
                filename: filename.into(),
                data: vec![1; size],
            };
            assert_eq!(
                message.validate().is_ok(),
                valid,
                "filename: {filename:?}, size: {size}"
            );
        }
    }

    #[test]
    fn native_checklists_are_not_supported() {
        assert!(
            serde_json::from_value::<OutgoingMessage>(
                json!({"type":"checklist","title":"Citrus","items":["Orange"]})
            )
            .is_err()
        );
    }

    #[test]
    fn validation_rejects_invalid_limits_and_coordinates() {
        for value in [
            json!({"type":"photo","media":{"source":"url","url":"file:///etc/passwd"}}),
            json!({"type":"photo","media":{"source":"telegram","file_id":""}}),
            json!({"type":"photo","media":{"source":"telegram","file_id":"x"},"caption":"x".repeat(1025)}),
            json!({"type":"poll","question":"?","options":[]}),
            json!({"type":"poll","question":"?","options":[" ","yes"]}),
            json!({"type":"location","latitude":91.0,"longitude":0.0}),
            json!({"type":"location","latitude":0.0,"longitude":181.0}),
            json!({"type":"post","title":"Title","body":" "}),
        ] {
            let message: OutgoingMessage = serde_json::from_value(value.clone()).unwrap();
            assert!(message.validate().is_err(), "accepted {value}");
        }
        assert!(
            OutgoingMessage::Location {
                latitude: f64::NAN,
                longitude: 0.0
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn uploads_are_internal_only() {
        assert!(
            serde_json::from_value::<MediaSource>(
                json!({"source":"upload","filename":"x","data":[1]})
            )
            .is_err()
        );
        let schema = serde_json::to_string(&schemars::schema_for!(OutgoingMessage)).unwrap();
        assert!(!schema.contains("\"upload\""));
    }

    #[test]
    fn validates_boundaries_and_reports_guest_support() {
        for (value, guest) in [
            (
                json!({"type":"photo","media":{"source":"telegram","file_id":"file"},"caption":"я".repeat(1024)}),
                true,
            ),
            (
                json!({"type":"video","media":{"source":"url","url":"https://example.com/video.mp4"},"thumbnail_url":"https://example.com/thumbnail.jpg"}),
                true,
            ),
            (
                json!({"type":"document","media":{"source":"file","path":"/tmp/document.pdf"}}),
                false,
            ),
            (
                json!({"type":"audio","media":{"source":"url","url":"http://example.com/audio.mp3"}}),
                true,
            ),
            (
                json!({"type":"poll","question":"я".repeat(300),"options":vec!["я".repeat(100);12]}),
                false,
            ),
            (
                json!({"type":"location","latitude":-90.0,"longitude":180.0}),
                true,
            ),
            (json!({"type":"post","title":"Title","body":"# Body"}), true),
        ] {
            let message: OutgoingMessage = serde_json::from_value(value).unwrap();
            message.validate().unwrap();
            assert_eq!(message.supports_guest(), guest);
        }
        let mut message = OutgoingMessage::Photo {
            media: MediaSource::Telegram {
                file_id: "file".into(),
            },
            caption: None,
        };
        *message.media_mut().unwrap() = MediaSource::Upload {
            filename: "image.jpg".into(),
            data: vec![1],
        };
        message.validate().unwrap();
        assert!(!message.supports_guest());
    }
}
