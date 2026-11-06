use std::io::Cursor;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use serde_json::json;
use tiechui_plugin_api::model::Content;
use tiechui_plugin_api::tools::{ToolError, ToolOutput};
use tokio_util::sync::CancellationToken;

use crate::check;

pub(crate) fn mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[255, 216, 255]) && b.get(3) != Some(&247) {
        return Some("image/jpeg");
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR") {
        let mut at = 8;
        while at + 8 <= b.len() {
            let length = u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
            let kind = &b[at + 4..at + 8];
            if kind == b"acTL" {
                return None;
            }
            if kind == b"IDAT" {
                break;
            }
            at = at.checked_add(length)?.checked_add(12)?;
        }
        return Some("image/png");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if b.starts_with(b"BM") && b.len() >= 26 {
        let u16_at = |at| u16::from_le_bytes([b[at], b[at + 1]]);
        let u32_at = |at| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let size = u32_at(2);
        let offset = u32_at(10);
        let header = u32_at(14);
        if (size != 0 && size < 26)
            || u64::from(offset) < 14 + u64::from(header)
            || (size != 0 && offset >= size)
        {
            return None;
        }
        let (planes, bits) = if header == 12 {
            (u16_at(22), u16_at(24))
        } else if (40..=124).contains(&header) && b.len() >= 30 {
            (u16_at(26), u16_at(28))
        } else {
            return None;
        };
        if planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits) {
            return Some("image/bmp");
        }
    }
    None
}
fn png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png).ok()?;
    Some(bytes.into_inner())
}
pub(crate) fn read(
    bytes: Vec<u8>,
    mime: &str,
    cancel: &CancellationToken,
    auto_resize: bool,
) -> Result<ToolOutput, ToolError> {
    check(cancel)?;
    let mut note = format!("Read image file [{mime}]");
    let mut output_mime = mime;
    let converted_from = (mime == "image/bmp").then_some(mime);
    if !auto_resize && mime != "image/bmp" {
        return Ok(inline(bytes, mime, note));
    }
    let decoded = (|| -> image::ImageResult<DynamicImage> {
        let mut decoder = ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()?
            .into_decoder()?;
        let orientation = decoder.orientation()?;
        let mut image = DynamicImage::from_decoder(decoder)?;
        image.apply_orientation(orientation);
        Ok(DynamicImage::ImageRgba8(image.to_rgba8()))
    })();
    let mut data = bytes;
    if mime == "image/bmp" {
        match decoded.as_ref().ok().and_then(png) {
            Some(bytes) => {
                data = bytes;
                output_mime = "image/png";
                note =
                    "Read image file [image/png]\n[Image converted from image/bmp to image/png.]"
                        .into();
            }
            None => {
                return Ok(omitted(format!(
                    "{note}\n[Image omitted: could not be converted to a supported inline image format.]"
                )));
            }
        }
    }
    if !auto_resize {
        return Ok(inline(data, output_mime, note));
    }
    let Ok(image) = decoded else {
        return Ok(omitted(format!(
            "{note}\n[Image omitted: could not be resized below the inline image size limit.]"
        )));
    };
    let (width, height) = (image.width(), image.height());
    if width > 2000 || height > 2000 || data.len().div_ceil(3) * 4 >= 4_718_592 {
        let (mut w, mut h) = (width, height);
        if w > 2000 {
            h = ((h as f64 * 2000.0 / w as f64).round() as u32).max(1);
            w = 2000;
        }
        if h > 2000 {
            w = ((w as f64 * 2000.0 / h as f64).round() as u32).max(1);
            h = 2000;
        }
        let mut candidate = None;
        loop {
            check(cancel)?;
            let resized = image.resize_exact(w, h, FilterType::Lanczos3);
            if let Some(bytes) = png(&resized)
                && bytes.len().div_ceil(3) * 4 < 4_718_592
            {
                candidate = Some((bytes, "image/png"));
            }
            if candidate.is_none() {
                for quality in [80, 85, 70, 55, 40] {
                    check(cancel)?;
                    let mut bytes = Vec::new();
                    if JpegEncoder::new_with_quality(&mut bytes, quality)
                        .encode_image(&resized.to_rgb8())
                        .is_ok()
                        && bytes.len().div_ceil(3) * 4 < 4_718_592
                    {
                        candidate = Some((bytes, "image/jpeg"));
                        break;
                    }
                }
            }
            if candidate.is_some() || (w == 1 && h == 1) {
                break;
            }
            w = ((w as f64 * 0.75).floor() as u32).max(1);
            h = ((h as f64 * 0.75).floor() as u32).max(1);
        }
        let Some((bytes, mime)) = candidate else {
            return Ok(omitted(format!(
                "{note}\n[Image omitted: could not be resized below the inline image size limit.]"
            )));
        };
        data = bytes;
        note = format!("Read image file [{mime}]");
        if let Some(from) = converted_from {
            note += &format!("\n[Image converted from {from} to {mime}.]");
        }
        note += &format!(
            "\n[Image: original {width}x{height}, displayed at {w}x{h}. Multiply coordinates by {:.2} to map to original image.]",
            width as f64 / w as f64
        );
        output_mime = mime;
    }
    check(cancel)?;
    Ok(inline(data, output_mime, note))
}
fn inline(data: Vec<u8>, output_mime: &str, note: String) -> ToolOutput {
    let data = STANDARD.encode(data);
    ToolOutput {
        content: vec![
            Content::Text {
                text: note.clone(),
                opaque: None,
            },
            Content::Image {
                mime_type: output_mime.into(),
                data: data.clone(),
            },
        ],
        structured_content: Some(
            json!({"type":"image","data":data,"mimeType":output_mime,"note":note}),
        ),
        ..Default::default()
    }
}
fn omitted(text: String) -> ToolOutput {
    let mut out = ToolOutput::text(text.clone());
    out.structured_content = Some(json!(text));
    out
}
