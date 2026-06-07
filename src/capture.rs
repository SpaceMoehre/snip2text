use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
    GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    HGDIOBJ, RGBQUAD, SRCCOPY,
};

/// Captures a region of the screen. Returns pixels in softbuffer format (0x00RRGGBB).
pub fn capture_region(x: i32, y: i32, width: u32, height: u32) -> Result<Vec<u32>> {
    unsafe {
        let hdc_screen = GetDC(HWND(std::ptr::null_mut()));
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let hbitmap = CreateCompatibleBitmap(hdc_screen, width as i32, height as i32);
        let old = SelectObject(hdc_mem, HGDIOBJ(hbitmap.0));

        let _ = BitBlt(
            hdc_mem, 0, 0, width as i32, height as i32,
            hdc_screen, x, y, SRCCOPY,
        );

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD::default()],
        };

        let n = (width * height) as usize;
        let mut raw = vec![0u8; n * 4];
        GetDIBits(
            hdc_mem,
            hbitmap,
            0,
            height,
            Some(raw.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(hdc_mem, old);
        let _ = DeleteObject(HGDIOBJ(hbitmap.0));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(HWND(std::ptr::null_mut()), hdc_screen);

        // GDI gives BGRA; softbuffer wants 0x00RRGGBB
        let pixels = raw
            .chunks_exact(4)
            .map(|c| (c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32)
            .collect();

        Ok(pixels)
    }
}

/// Extracts a sub-region from a full-screen pixel buffer and returns it as an RGBA image.
pub fn extract_rgba(
    pixels: &[u32],
    full_width: u32,
    rx: u32, ry: u32, rw: u32, rh: u32,
) -> image::RgbaImage {
    let mut img = image::RgbaImage::new(rw, rh);
    for row in 0..rh {
        for col in 0..rw {
            let src = ((ry + row) * full_width + (rx + col)) as usize;
            let p = pixels[src];
            let r = ((p >> 16) & 0xFF) as u8;
            let g = ((p >> 8) & 0xFF) as u8;
            let b = (p & 0xFF) as u8;
            img.put_pixel(col, row, image::Rgba([r, g, b, 255]));
        }
    }
    img
}
