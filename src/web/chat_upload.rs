use super::chat::{Input, error};
use crate::agent::{
    FileAttachment, Image,
    file::{MAX_FILE_BYTES, MAX_REQUEST_FILE_BYTES, file_type},
};
use axum::{
    Json,
    extract::{FromRequest, Multipart, Request},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use std::path::Path;

pub(super) const BODY_LIMIT: usize = MAX_REQUEST_FILE_BYTES + 64 * 1024;
pub(super) const MAX_ATTACHMENTS: usize = 10;

pub(super) struct Upload {
    filename: String,
    mime: Option<String>,
    bytes: Vec<u8>,
}

pub(super) fn is_multipart(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("multipart/form-data")
        })
}

pub(super) async fn parse(request: Request) -> Result<(Input, Vec<Upload>), Response> {
    if !is_multipart(&request) {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .map_err(|_| error(StatusCode::PAYLOAD_TOO_LARGE, "Message body exceeds 64 KiB"))?;
        let request = Request::from_parts(parts, axum::body::Body::from(bytes));
        let Json(input) = Json::<Input>::from_request(request, &())
            .await
            .map_err(IntoResponse::into_response)?;
        return Ok((input, vec![]));
    }
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(IntoResponse::into_response)?;
    let mut fields = serde_json::Map::new();
    let mut uploads = vec![];
    let mut total = 0;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| error(e.status(), &e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let filename = field.file_name().map(str::to_owned);
        let mime = field.content_type().map(str::to_owned);
        let is_file = name == "files" && filename.is_some();
        if (!is_file
            && (!matches!(name.as_str(), "id" | "request_id" | "text")
                || fields.contains_key(&name)))
            || (is_file && uploads.len() >= MAX_ATTACHMENTS)
        {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Invalid fields or more than 10 attachments",
            ));
        }
        let limit = if is_file { MAX_FILE_BYTES } else { 32 * 1024 };
        let mut bytes = vec![];
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| error(e.status(), &e.to_string()))?
        {
            total += chunk.len();
            if bytes.len() + chunk.len() > limit || total > MAX_REQUEST_FILE_BYTES {
                return Err(error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Attachment or total upload is too large",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if is_file {
            uploads.push(Upload {
                filename: filename.unwrap(),
                mime,
                bytes,
            });
        } else {
            let value = String::from_utf8(bytes)
                .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid UTF-8 field"))?;
            fields.insert(name, value.into());
        }
    }
    let input = serde_json::from_value(fields.into())
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Missing or invalid message fields"))?;
    Ok((input, uploads))
}

pub(super) struct Attachments {
    pub files: Vec<FileAttachment>,
    pub images: Vec<Image>,
    discard_on_drop: bool,
}

impl Attachments {
    pub fn save(directory: &Path, uploads: Vec<Upload>) -> anyhow::Result<Self> {
        let mut saved = Self {
            files: Vec::new(),
            images: Vec::new(),
            discard_on_drop: true,
        };
        for upload in uploads {
            if file_type(&upload.filename, upload.mime.as_deref())?.starts_with("image/") {
                saved.images.push(Image::from_bytes(&upload.bytes)?);
            } else {
                saved.files.push(FileAttachment::save(
                    directory,
                    &upload.filename,
                    upload.mime.as_deref(),
                    &upload.bytes,
                )?);
            }
        }
        Ok(saved)
    }

    pub fn start_admission(&mut self) {
        // Cancellation or an unknown commit outcome must never delete durable evidence.
        self.discard_on_drop = false;
    }

    pub fn retain_accepted(&mut self, accepted: &[FileAttachment]) {
        // Admission can return an earlier request; discard this retry's duplicate files.
        self.files
            .retain(|file| !accepted.iter().any(|saved| saved.path == file.path));
        self.discard_on_drop = true;
    }
}

impl Drop for Attachments {
    fn drop(&mut self) {
        if !self.discard_on_drop {
            return;
        }
        for file in &self.files {
            if let Err(error) = std::fs::remove_file(&file.path) {
                log::warn!("Could not remove unaccepted chat upload: {error}");
            }
        }
    }
}
