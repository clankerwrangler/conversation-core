//! pi 1.5 image handling for `read` (`utils/mime.js`, `image-process.js`, `image-resize-core.js`),
//! with the `image` crate in place of Photon: same sniffing, limits and encoding search.

use base64::Engine;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};
use std::io::Cursor;

/// 4.5MB of base64 payload, below Anthropic's 5MB limit.
const MAX_BYTES: usize = 4_718_592;
const MAX_WIDTH: u32 = 2000;
const MAX_HEIGHT: u32 = 2000;
const JPEG_QUALITY: u8 = 80;

pub struct Processed {
    pub data: String,
    pub mime: &'static str,
    pub hints: Vec<String>,
}

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// `detectSupportedImageMimeType`.
pub fn detect_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return if b.len() > 3 && b[3] == 0xf7 {
            None
        } else {
            Some("image/jpeg")
        };
    }
    if b.starts_with(&PNG_SIGNATURE) {
        return (is_png(b) && !is_animated_png(b)).then_some("image/png");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if b.starts_with(b"RIFF") && b.len() >= 12 && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if b.starts_with(b"BM") && is_bmp(b) {
        return Some("image/bmp");
    }
    None
}

fn at(b: &[u8], i: usize) -> u32 {
    u32::from(b.get(i).copied().unwrap_or(0))
}

fn u32_be(b: &[u8], i: usize) -> u32 {
    (at(b, i) << 24) | (at(b, i + 1) << 16) | (at(b, i + 2) << 8) | at(b, i + 3)
}

fn u32_le(b: &[u8], i: usize) -> u32 {
    at(b, i) | (at(b, i + 1) << 8) | (at(b, i + 2) << 16) | (at(b, i + 3) << 24)
}

fn u16_le(b: &[u8], i: usize) -> u32 {
    at(b, i) | (at(b, i + 1) << 8)
}

fn is_png(b: &[u8]) -> bool {
    b.len() >= 16 && u32_be(b, 8) == 13 && &b[12..16] == b"IHDR"
}

fn is_animated_png(b: &[u8]) -> bool {
    let mut offset = 8usize;
    while offset + 8 <= b.len() {
        let len = u32_be(b, offset) as usize;
        let kind = &b[offset + 4..offset + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" {
            return false;
        }
        let next = offset + 8 + len + 4;
        if next <= offset || next > b.len() {
            return false;
        }
        offset = next;
    }
    false
}

fn is_bmp(b: &[u8]) -> bool {
    if b.len() < 26 {
        return false;
    }
    let file_size = u32_le(b, 2);
    let pixels = u32_le(b, 10);
    let dib = u32_le(b, 14);
    if file_size != 0 && file_size < 26 {
        return false;
    }
    if u64::from(pixels) < 14 + u64::from(dib) {
        return false;
    }
    if file_size != 0 && pixels >= file_size {
        return false;
    }
    let (planes, bits) = if dib == 12 {
        (u16_le(b, 22), u16_le(b, 24))
    } else if (40..=124).contains(&dib) {
        if b.len() < 30 {
            return false;
        }
        (u16_le(b, 26), u16_le(b, 28))
    } else {
        return false;
    };
    planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits)
}

fn format_of(mime: &str) -> Option<ImageFormat> {
    match mime {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        "image/bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

/// Decode, applying EXIF orientation for JPEG and WebP as pi's `applyExifOrientation` does.
fn decode(bytes: &[u8], mime: &str) -> Option<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.set_format(format_of(mime)?);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    if matches!(mime, "image/jpeg" | "image/webp")
        && let Some(o) = orientation
    {
        img.apply_orientation(o);
    }
    Some(img)
}

fn encode_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let rgb = img.to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&rgb)
        .ok()?;
    Some(out)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `processImage` with auto-resize on: BMP becomes PNG, then the image is fitted within
/// 2000x2000 and 4.5MB of base64.
pub fn process(bytes: &[u8], mime: &'static str) -> Result<Processed, String> {
    let mut hints = Vec::new();
    let converted_from_bmp = mime == "image/bmp";
    let (bytes, mime): (std::borrow::Cow<[u8]>, &'static str) = if converted_from_bmp {
        let png = decode(bytes, mime)
            .and_then(|img| encode_png(&img))
            .ok_or("[Image omitted: could not be converted to a supported inline image format.]")?;
        (png.into(), "image/png")
    } else {
        (bytes.into(), mime)
    };
    let resized = resize(&bytes, mime)
        .ok_or("[Image omitted: could not be resized below the inline image size limit.]")?;
    if converted_from_bmp {
        hints.push(format!(
            "[Image converted from image/bmp to {}.]",
            resized.mime
        ));
    }
    if resized.was_resized {
        let scale = f64::from(resized.original.0) / f64::from(resized.size.0);
        hints.push(format!(
            "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {scale:.2} to map to original image.]",
            resized.original.0, resized.original.1, resized.size.0, resized.size.1
        ));
    }
    Ok(Processed {
        data: resized.data,
        mime: resized.mime,
        hints,
    })
}

struct Resized {
    data: String,
    mime: &'static str,
    original: (u32, u32),
    size: (u32, u32),
    was_resized: bool,
}

/// JavaScript `Math.round` for non-negative values.
fn js_round(x: f64) -> u32 {
    (x + 0.5).floor() as u32
}

/// `resizeImageInProcess`.
fn resize(input: &[u8], mime: &'static str) -> Option<Resized> {
    let input_b64_size = input.len().div_ceil(3) * 4;
    let img = decode(input, mime)?;
    let (ow, oh) = (img.width(), img.height());
    if ow <= MAX_WIDTH && oh <= MAX_HEIGHT && input_b64_size < MAX_BYTES {
        return Some(Resized {
            data: b64(input),
            mime,
            original: (ow, oh),
            size: (ow, oh),
            was_resized: false,
        });
    }
    let (mut w, mut h) = (ow, oh);
    if w > MAX_WIDTH {
        h = js_round(f64::from(h) * f64::from(MAX_WIDTH) / f64::from(w));
        w = MAX_WIDTH;
    }
    if h > MAX_HEIGHT {
        w = js_round(f64::from(w) * f64::from(MAX_HEIGHT) / f64::from(h));
        h = MAX_HEIGHT;
    }
    let mut qualities = vec![JPEG_QUALITY];
    for q in [85, 70, 55, 40] {
        if !qualities.contains(&q) {
            qualities.push(q);
        }
    }
    loop {
        let r = img.resize_exact(w.max(1), h.max(1), FilterType::Lanczos3);
        let png = b64(&encode_png(&r)?);
        let mut jpegs = Vec::new();
        for q in &qualities {
            jpegs.push(b64(&encode_jpeg(&r, *q)?));
        }
        let first = jpegs.remove(0);
        let preferred: Vec<(String, &'static str)> = if first.len() < png.len() {
            vec![(first, "image/jpeg"), (png, "image/png")]
        } else {
            vec![(png, "image/png"), (first, "image/jpeg")]
        };
        let candidates = preferred
            .into_iter()
            .chain(jpegs.into_iter().map(|j| (j, "image/jpeg")));
        for (data, m) in candidates {
            if data.len() < MAX_BYTES {
                return Some(Resized {
                    data,
                    mime: m,
                    original: (ow, oh),
                    size: (w, h),
                    was_resized: true,
                });
            }
        }
        if w == 1 && h == 1 {
            return None;
        }
        let nw = if w == 1 {
            1
        } else {
            ((f64::from(w) * 0.75).floor() as u32).max(1)
        };
        let nh = if h == 1 {
            1
        } else {
            ((f64::from(h) * 0.75).floor() as u32).max(1)
        };
        if nw == w && nh == h {
            return None;
        }
        w = nw;
        h = nh;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_and_resizes() {
        let img = DynamicImage::new_rgb8(3000, 1000);
        let png = encode_png(&img).unwrap_or_default();
        assert_eq!(detect_mime(&png), Some("image/png"));
        let p = process(&png, "image/png")
            .ok()
            .map(|p| (p.mime, p.hints))
            .unwrap_or(("", vec![]));
        assert_eq!(
            p.1,
            vec!["[Image: original 3000x1000, displayed at 2000x667. Multiply coordinates by 1.50 to map to original image.]".to_string()]
        );
        let small = encode_png(&DynamicImage::new_rgb8(10, 10)).unwrap_or_default();
        let p = process(&small, "image/png")
            .ok()
            .map(|p| (p.data, p.hints.len()))
            .unwrap_or_default();
        assert_eq!(p, (b64(&small), 0));
        assert_eq!(detect_mime(b"hello"), None);
    }
}
