use anyhow::Result;
use image::{RgbaImage, imageops};
use windows::{
    core::HSTRING,
    Globalization::Language,
    Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
};

pub fn run_ocr(img: &RgbaImage, language_tag: &str) -> Result<String> {
    let prepped = prepare_for_ocr(img);
    let bitmap = rgba_to_software_bitmap(&prepped)?;
    let engine = create_engine(language_tag)?;
    let result = engine.RecognizeAsync(&bitmap)?.get()?;
    Ok(result.Text()?.to_string())
}

/// Scale up and optionally invert so WinRT OCR gets a large, dark-on-light image.
fn prepare_for_ocr(img: &RgbaImage) -> RgbaImage {
    // Scale up so the shorter dimension is at least 200 px.
    const MIN_DIM: u32 = 200;
    let min_side = img.width().min(img.height());
    let img: std::borrow::Cow<RgbaImage> = if min_side < MIN_DIM {
        let scale = (MIN_DIM + min_side - 1) / min_side;
        std::borrow::Cow::Owned(imageops::resize(
            img,
            img.width() * scale,
            img.height() * scale,
            imageops::FilterType::Nearest,
        ))
    } else {
        std::borrow::Cow::Borrowed(img)
    };

    // Detect whether the image has a dark background by sampling the mean luminance.
    // WinRT OCR is more reliable with dark text on a light background, so invert if needed.
    let mean_luma: f32 = img
        .pixels()
        .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
        .sum::<f32>()
        / (img.width() * img.height()) as f32;

    if mean_luma < 128.0 {
        let mut inv = img.into_owned();
        for p in inv.pixels_mut() {
            p[0] = 255 - p[0];
            p[1] = 255 - p[1];
            p[2] = 255 - p[2];
            // leave alpha unchanged
        }
        inv
    } else {
        img.into_owned()
    }
}

fn rgba_to_software_bitmap(img: &RgbaImage) -> Result<SoftwareBitmap> {
    let bgra: Vec<u8> = img
        .pixels()
        .flat_map(|p| {
            let [r, g, b, a] = p.0;
            [b, g, r, a]
        })
        .collect();

    let writer = DataWriter::new()?;
    writer.WriteBytes(&bgra)?;
    let ibuffer = writer.DetachBuffer()?;

    Ok(SoftwareBitmap::CreateCopyFromBuffer(
        &ibuffer,
        BitmapPixelFormat::Bgra8,
        img.width() as i32,
        img.height() as i32,
    )?)
}

fn create_engine(language_tag: &str) -> Result<OcrEngine> {
    if language_tag.is_empty() {
        return Ok(OcrEngine::TryCreateFromUserProfileLanguages()?);
    }
    let lang = Language::CreateLanguage(&HSTRING::from(language_tag))?;
    Ok(OcrEngine::TryCreateFromLanguage(&lang)?)
}

/// Returns (display_name, language_tag) pairs for the tray submenu.
/// Language availability depends on what Windows language packs are installed.
pub fn available_languages() -> Vec<(String, String)> {
    vec![
        ("Auto (System Language)".into(), String::new()),
        ("English".into(), "en".into()),
        ("German / Deutsch".into(), "de".into()),
        ("French / Français".into(), "fr".into()),
        ("Spanish / Español".into(), "es".into()),
        ("Chinese Simplified".into(), "zh-Hans".into()),
        ("Chinese Traditional".into(), "zh-Hant".into()),
        ("Japanese / 日本語".into(), "ja".into()),
        ("Korean / 한국어".into(), "ko".into()),
        ("Russian / Русский".into(), "ru".into()),
        ("Arabic / العربية".into(), "ar".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ocr_png() {
        let path = std::env::current_dir().unwrap().join("test.png");
        let img = image::open(&path)
            .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()))
            .to_rgba8();
        println!("original: {}x{}", img.width(), img.height());
        let prepped = prepare_for_ocr(&img);
        println!("prepared: {}x{}", prepped.width(), prepped.height());
        let result = run_ocr(&img, "en").expect("OCR failed");
        println!("OCR result: {:?}", result);
        assert!(
            result.to_lowercase().contains("ctrl"),
            "expected 'CTRL' in result, got: {result:?}"
        );
    }
}
