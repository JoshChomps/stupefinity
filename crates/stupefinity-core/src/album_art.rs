//! The current track's cover art for the player view: downloaded and decoded off the
//! window thread, scaled once to the art box, and kept as a GDI bitmap for painting.

use std::sync::Mutex;
use tokio::runtime::Handle;
use tracing::warn;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, SRCCOPY,
};

/// Width and height of the player view's art box, in pixels.
pub const ART_SIZE: u32 = 260;

/// The cover for `url`, once it has loaded (`None` while loading or if it failed).
struct Cover {
    url: String,
    bitmap: Option<HBITMAP>,
}

// HBITMAP is a process-wide GDI handle, usable from any thread.
unsafe impl Send for Cover {}

static COVER: Mutex<Option<Cover>> = Mutex::new(None);

/// Starts loading `url` unless it is already loaded or loading; `on_ready` runs once it
/// can be painted.
pub fn request(url: Option<&str>, runtime: &Handle, on_ready: fn()) {
    let Some(url) = url else {
        return;
    };
    {
        let mut cover = COVER.lock().unwrap_or_else(|e| e.into_inner());
        if cover.as_ref().is_some_and(|c| c.url == url) {
            return;
        }
        if let Some(old) = cover.replace(Cover {
            url: url.to_string(),
            bitmap: None,
        }) {
            if let Some(bmp) = old.bitmap {
                unsafe { DeleteObject(bmp) };
            }
        }
    }

    let url = url.to_string();
    runtime.spawn(async move {
        let loaded = async {
            let bytes = reqwest::get(&url)
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let pixels = tokio::task::spawn_blocking(move || decode(&bytes, ART_SIZE)).await??;
            anyhow::Ok(pixels)
        };
        let pixels = match loaded.await {
            Ok(p) => p,
            Err(e) => {
                warn!("Could not load cover art: {e}");
                return;
            }
        };
        let Some(bitmap) = (unsafe { to_bitmap(&pixels, ART_SIZE) }) else {
            return;
        };
        let mut cover = COVER.lock().unwrap_or_else(|e| e.into_inner());
        match cover.as_mut() {
            // Still the current track's cover.
            Some(c) if c.url == url => c.bitmap = Some(bitmap),
            _ => unsafe {
                DeleteObject(bitmap);
            },
        }
        drop(cover);
        on_ready();
    });
}

/// Paints the cover for `url` at (`x`, `y`); returns false when it is not loaded yet.
///
/// # Safety
/// `hdc` must be a valid device context.
pub unsafe fn draw(hdc: HDC, url: Option<&str>, x: i32, y: i32) -> bool {
    let cover = COVER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(bitmap) = cover
        .as_ref()
        .filter(|c| Some(c.url.as_str()) == url)
        .and_then(|c| c.bitmap)
    else {
        return false;
    };
    let mem = CreateCompatibleDC(hdc);
    let old = SelectObject(mem, bitmap);
    let size = ART_SIZE as i32;
    BitBlt(hdc, x, y, size, size, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteDC(mem);
    true
}

/// Decodes a JPEG cover into `size`×`size` top-down BGRA pixels (the layout of a 32-bit
/// GDI DIB).
fn decode(bytes: &[u8], size: u32) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?;
    let mut pixels = image
        .resize_exact(size, size, image::imageops::FilterType::Triangle)
        .into_rgba8()
        .into_raw();
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok(pixels)
}

unsafe fn to_bitmap(pixels: &[u8], size: u32) -> Option<HBITMAP> {
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size as i32,
        // Negative: rows run top to bottom.
        biHeight: -(size as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..std::mem::zeroed()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(
        std::ptr::null_mut(),
        &info,
        DIB_RGB_COLORS,
        &mut bits,
        std::ptr::null_mut(),
        0,
    );
    if bitmap.is_null() || bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
    Some(bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_scales_to_bgra() {
        // A 2×2 red JPEG.
        let mut jpeg = Vec::new();
        image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]))
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let pixels = decode(&jpeg, 4).unwrap();
        assert_eq!(pixels.len(), 4 * 4 * 4);
        let (b, g, r, a) = (pixels[0], pixels[1], pixels[2], pixels[3]);
        assert!(r > 200 && g < 60 && b < 60 && a == 255, "{b} {g} {r} {a}");
        assert!(decode(b"not a jpeg", 4).is_err());
    }
}
