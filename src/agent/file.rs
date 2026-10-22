use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

// Telegram's hosted Bot API downloads at most 20 MB.
pub const MAX_FILE_BYTES: usize = 20_000_000;
pub const MAX_REQUEST_FILE_BYTES: usize = 50_000_000;

#[derive(Debug, Clone, Deserialize, Serialize)]
// ponytail: retain attachments while saved sessions may reference them; add reference-based GC if disk use grows.
pub struct FileAttachment {
    pub filename: String,
    pub mime_type: String,
    pub path: PathBuf,
    pub size: usize,
    sha256: String,
}

impl FileAttachment {
    pub fn save(
        directory: &Path,
        filename: &str,
        mime: Option<&str>,
        bytes: &[u8],
    ) -> anyhow::Result<Self> {
        let filename = filename.rsplit(['/', '\\']).next().unwrap_or_default();
        ensure!(
            !filename.is_empty()
                && filename.len() <= 255
                && !filename.chars().any(char::is_control),
            "Invalid attachment filename"
        );
        let mime_type = file_type(filename, mime)?;
        ensure!(
            !mime_type.starts_with("image/"),
            "Images must use image input"
        );
        ensure!(!bytes.is_empty(), "File is empty");
        ensure!(bytes.len() <= MAX_FILE_BYTES, "File exceeds 20 MB");
        if mime_type == "application/pdf" {
            ensure!(bytes.starts_with(b"%PDF-"), "Invalid PDF file");
        } else if mime_type.starts_with("text/")
            || matches!(
                mime_type,
                "application/json" | "application/xml" | "application/javascript"
            )
        {
            std::str::from_utf8(bytes).context("Text file is not valid UTF-8")?;
        }
        std::fs::create_dir_all(directory)?;
        let mut stored = tempfile::NamedTempFile::new_in(directory)?;
        stored.write_all(bytes)?;
        stored.as_file().sync_all()?;
        let path = directory.join(uuid::Uuid::new_v4().to_string());
        stored.persist_noclobber(&path)?;
        std::fs::File::open(directory)?.sync_all()?;
        Ok(Self {
            filename: filename.to_owned(),
            mime_type: mime_type.to_owned(),
            path,
            size: bytes.len(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }

    pub(crate) async fn data_url(&self) -> anyhow::Result<String> {
        let bytes = self.bytes().await?;
        Ok(format!(
            "data:{};base64,{}",
            self.mime_type,
            STANDARD.encode(bytes)
        ))
    }

    pub(crate) async fn bytes(&self) -> anyhow::Result<Vec<u8>> {
        ensure!(
            self.size > 0 && self.size <= MAX_FILE_BYTES,
            "Invalid stored attachment size"
        );
        let file = tokio::fs::File::open(&self.path)
            .await
            .with_context(|| format!("Cannot read attachment {}", self.filename))?;
        ensure!(
            file.metadata().await?.len() == self.size as u64,
            "Stored attachment size changed: {}",
            self.filename
        );
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() == self.size,
            "Stored attachment size changed: {}",
            self.filename
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == self.sha256,
            "Stored attachment contents changed: {}",
            self.filename
        );
        Ok(bytes)
    }
}

pub(crate) fn file_type(filename: &str, mime: Option<&str>) -> anyhow::Result<&'static str> {
    if filename.eq_ignore_ascii_case("Dockerfile") || filename.eq_ignore_ascii_case("Makefile") {
        return Ok("text/plain");
    }
    let extension = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let formats = [
        ("pdf", "application/pdf"),
        ("doc dot", "application/msword"),
        (
            "docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        ("odt", "application/vnd.oasis.opendocument.text"),
        ("rtf", "application/rtf"),
        ("pages", "application/vnd.apple.pages"),
        ("pot ppa pps ppt pwz wiz", "application/vnd.ms-powerpoint"),
        (
            "pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        ("key keynote", "application/vnd.apple.keynote"),
        ("xla xlb xlc xlm xls xlt xlw", "application/vnd.ms-excel"),
        (
            "xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        ("csv", "text/csv"),
        ("tsv", "text/tsv"),
        ("iif", "text/x-iif"),
        ("md markdown", "text/markdown"),
        ("json", "application/json"),
        ("htm html", "text/html"),
        ("xml", "text/xml"),
        ("jpg jpeg", "image/jpeg"),
        ("png", "image/png"),
        ("webp", "image/webp"),
        (
            "asm bat c cc conf cpp css cxx def dic eml h hh ics ifb in js list log mht mhtml mime mjs nws pl py rst s sql srt text txt vcf vtt rs go ts tsx jsx java kt kts swift lua r jl rb php sh bash zsh ksh tex cs scala ps1 diff patch ex exs erl hs clj groovy dart awk hbs mustache ejs jinja jinja2 liquid erb twig pug jade tmpl cmake dockerfile gradle ini properties proto sass scss less hcl tf toml graphql gql ndjson json5 yaml yml astro",
            "text/plain",
        ),
    ];
    if let Some((_, mime)) = formats
        .iter()
        .find(|(extensions, _)| extensions.split_whitespace().any(|ext| ext == extension))
    {
        return Ok(mime);
    }
    // Only infer a type from MIME when Telegram omitted a filename extension.
    if extension.is_empty()
        && let Some(mime) = mime
        && let Some((_, supported)) = formats.iter().find(|(_, supported)| *supported == mime)
    {
        return Ok(supported);
    }
    anyhow::bail!(
        "Unsupported file format. Send a PDF, document, presentation, spreadsheet, text/code file, or PNG/JPEG/WebP image."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_private_original_bytes_and_rejects_bad_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"%PDF-1.7\ntest";
        let file = FileAttachment::save(dir.path(), "../../report.PDF", None, bytes).unwrap();
        assert_eq!(file.filename, "report.PDF");
        assert_eq!(file.mime_type, "application/pdf");
        assert_eq!(
            file.data_url().await.unwrap(),
            "data:application/pdf;base64,JVBERi0xLjcKdGVzdA=="
        );
        assert!(file.path.starts_with(dir.path()));
        assert_eq!(std::fs::read(&file.path).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let restored: FileAttachment =
            serde_json::from_str(&serde_json::to_string(&file).unwrap()).unwrap();
        assert_eq!(
            restored.data_url().await.unwrap(),
            file.data_url().await.unwrap()
        );
        std::fs::write(&file.path, b"%PDF-1.7\nxxxx").unwrap();
        assert!(
            file.data_url().await.is_err(),
            "same-size changes must not alter saved conversation evidence"
        );
        std::fs::write(&file.path, bytes).unwrap();
        std::fs::write(&file.path, b"modified").unwrap();
        assert!(file.data_url().await.is_err());
        std::fs::remove_file(&file.path).unwrap();
        assert!(file.data_url().await.is_err());
        for (name, data) in [
            ("test.zip", &b"zip"[..]),
            ("test.pdf", &b"bad"[..]),
            ("test.txt", &b""[..]),
            ("test.txt", &b"\xff"[..]),
        ] {
            assert!(
                FileAttachment::save(dir.path(), name, None, data).is_err(),
                "{name}"
            );
        }
        assert!(
            FileAttachment::save(dir.path(), "big.txt", None, &vec![b'x'; MAX_FILE_BYTES + 1])
                .is_err()
        );
    }

    #[test]
    fn recognizes_supported_formats_and_generic_telegram_mime() {
        for (name, mime, expected) in [
            (
                "report.pdf",
                Some("application/octet-stream"),
                "application/pdf",
            ),
            ("notes.md", None, "text/markdown"),
            (
                "sheet.xlsx",
                None,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "slides.pptx",
                None,
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            (
                "doc.docx",
                None,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            ("main.rs", None, "text/plain"),
            ("Dockerfile", Some("text/x-dockerfile"), "text/plain"),
            ("Makefile", Some("text/x-makefile"), "text/plain"),
        ] {
            assert_eq!(file_type(name, mime).unwrap(), expected);
        }
        assert!(file_type("audio.mp3", Some("audio/mpeg")).is_err());
        assert!(file_type("malware.exe", Some("text/plain")).is_err());
    }
}
