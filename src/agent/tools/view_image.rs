use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt};

use anyhow::{Context, ensure};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::agent::{
    Image,
    image::MAX_IMAGE_BYTES,
    tools::{Tool, ToolOutput},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewImageArgs {
    /// Local image path, absolute or relative to Kodrik's working directory.
    pub(crate) path: String,
}

pub struct ViewImageTool;

#[async_trait::async_trait]
impl Tool for ViewImageTool {
    fn replay_policy(&self) -> super::ReplayPolicy {
        super::ReplayPolicy::Safe
    }

    fn name(&self) -> String {
        "view_image".into()
    }

    fn description(&self) -> String {
        "View a local PNG, JPEG or WebP image, including screenshots saved by other tools. Sends the image to the model at its original dimensions, with path, width and height metadata. Maximum 10 MiB and 32 million pixels. Requires a vision-capable model. Use this to see images; reading a filename or base64 through exec_command does not display them.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(ViewImageArgs).to_value()
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let args: ViewImageArgs = serde_json::from_value(properties)?;
        ensure!(!args.path.trim().is_empty(), "path must not be empty");
        tokio::task::spawn_blocking(move || {
            // O_NONBLOCK prevents FIFOs from hanging before the regular-file check.
            let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK)
                .open(&args.path).context("Cannot open image")?;
            let metadata = file.metadata()?;
            ensure!(metadata.is_file(), "Image path must be a regular file");
            ensure!(metadata.len() <= MAX_IMAGE_BYTES as u64, "Image exceeds 10 MiB");
            let mut bytes = Vec::new();
            file.take(MAX_IMAGE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
            let image = Image::from_bytes(&bytes)?;
            Ok(ToolOutput {
                output: serde_json::json!({"path":args.path,"width":image.width,"height":image.height}),
                images: vec![image],
            })
        }).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::image::tests::sample;
    use serde_json::json;

    #[tokio::test]
    async fn reads_image_bytes_without_exposing_them_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screenshot.untrusted-extension");
        let bytes = sample(image::ImageFormat::Png);
        std::fs::write(&path, &bytes).unwrap();
        let result = ViewImageTool.execute(json!({"path":path})).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(result.output, json!({"path":path,"width":2,"height":3}));
        assert_eq!(result.images.len(), 1);
        assert_eq!(
            result.images[0].data_url,
            Image::from_bytes(&bytes).unwrap().data_url
        );
        assert!(!result.output.to_string().contains("base64"));
    }

    #[tokio::test]
    async fn rejects_invalid_arguments_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join("invalid.png");
        std::fs::write(&invalid, "not PNG").unwrap();
        let large = dir.path().join("large.png");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        for args in [
            json!({}),
            json!({"path":""}),
            json!({"path":invalid}),
            json!({"path":large}),
            json!({"path":dir.path()}),
            json!({"path":dir.path().join("missing")}),
            json!({"path":invalid,"extra":true}),
        ] {
            assert!(ViewImageTool.execute(args).await.is_err());
        }
        let fifo = dir.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(ViewImageTool.execute(json!({"path":fifo})).await.is_err());
    }
}
