use std::io::Cursor;

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};

pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
// ponytail: store image bytes in session JSON; use an artifact store if long image histories grow too large.
pub struct Image {
    pub data_url: String,
    pub width: u32,
    pub height: u32,
}

// Keep image payloads out of progress/debug output.
impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Image {
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "Image exceeds 10 MiB");
        let format = image::guess_format(bytes).context("Unrecognized image format")?;
        ensure!(
            matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
            ),
            "Only PNG, JPEG and WebP images are supported"
        );
        let mut limits = Limits::default();
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        limits.max_alloc = Some(128 * 1024 * 1024);
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits);
        let decoder = reader.into_decoder().context("Invalid image")?;
        let (width, height) = decoder.dimensions();
        ensure!(
            width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 32_000_000,
            "Image must contain 1–32 million pixels"
        );
        // Decode for validation; retain the original bytes and dimensions for GUI coordinates.
        image::DynamicImage::from_decoder(decoder).context("Invalid image data")?;
        Ok(Self {
            data_url: format!(
                "data:{};base64,{}",
                format.to_mime_type(),
                STANDARD.encode(bytes)
            ),
            width,
            height,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample(format: ImageFormat) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(2, 3, image::Rgb([255, 0, 0]))
            .write_to(&mut bytes, format)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn validates_supported_images_preserving_bytes_and_dimensions() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let bytes = sample(format);
            let image = Image::from_bytes(&bytes).unwrap();
            assert_eq!((image.width, image.height), (2, 3));
            let (prefix, encoded) = image.data_url.split_once(',').unwrap();
            assert_eq!(prefix, format!("data:{};base64", format.to_mime_type()));
            assert_eq!(STANDARD.decode(encoded).unwrap(), bytes);
            assert!(!format!("{image:?}").contains(encoded));
        }
    }

    #[test]
    fn rejects_invalid_oversized_and_excessive_dimension_images() {
        for bytes in [
            vec![],
            b"not an image".to_vec(),
            b"\x89PNG\r\n\x1a\n".to_vec(),
            vec![0; MAX_IMAGE_BYTES + 1],
        ] {
            assert!(Image::from_bytes(&bytes).is_err());
        }
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::new(16_385, 1)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        assert!(Image::from_bytes(bytes.get_ref()).is_err());
    }
}
