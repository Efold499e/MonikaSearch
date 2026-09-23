//! 文件类型图标（SHGetFileInfoW 按 扩展名/目录 提取系统真实图标 → PNG data URL）
//! 与图片文件缩略图（解码后缩到 ≤160px PNG data URL）。

use std::io::Cursor;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_USEFILEATTRIBUTES,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

fn png_data_url_from_hicon(hicon: *mut core::ffi::c_void, size: i32) -> Option<String> {
    unsafe {
        let mut info: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut info) == 0 {
            return None;
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                ..std::mem::zeroed()
            },
            ..std::mem::zeroed()
        };
        let mut px = vec![0u8; (size * size * 4) as usize];
        let hdc = CreateCompatibleDC(std::ptr::null_mut());
        let got = GetDIBits(
            hdc,
            info.hbmColor,
            0,
            size as u32,
            px.as_mut_ptr() as *mut core::ffi::c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(info.hbmColor);
        let _ = DeleteObject(info.hbmMask);
        DeleteDC(hdc);
        if got == 0 {
            return None;
        }
        // BGRA -> RGBA
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2);
            // 预乘还原（图标色位图为预乘 alpha 时 edges 会发暗，这里做除法还原）
            let a = p[3] as u32;
            if a > 0 && a < 255 {
                p[0] = ((p[0] as u32 * 255 + a / 2) / a).min(255) as u8;
                p[1] = ((p[1] as u32 * 255 + a / 2) / a).min(255) as u8;
                p[2] = ((p[2] as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        let img = image::RgbaImage::from_raw(size as u32, size as u32, px)?;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some(format!("data:image/png;base64,{}", B64.encode(png)))
    }
}

/// 提取系统图标。ext 为空串 → 文件夹图标；否则按扩展名（文件无需存在）。
pub fn icon_data_url(ext: &str) -> Option<String> {
    let (probe, attrs) = if ext.is_empty() {
        ("MonikaFolder".to_string(), FILE_ATTRIBUTE_DIRECTORY)
    } else {
        (format!("monika.{}", ext.to_lowercase()), FILE_ATTRIBUTE_NORMAL)
    };
    let mut wide: Vec<u16> = probe.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        SHGetFileInfoW(
            wide.as_mut_ptr(),
            attrs,
            &mut sfi,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if ok == 0 || sfi.hIcon.is_null() {
        return None;
    }
    let url = png_data_url_from_hicon(sfi.hIcon, 32);
    unsafe { DestroyIcon(sfi.hIcon) };
    url
}

/// 图片缩略图（≤160px PNG）。文件 >20MB 或解码失败返回 None。
pub fn image_thumbnail(path: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 20 * 1024 * 1024 {
        return None;
    }
    let img = image::open(path).ok()?;
    let thumb = img.thumbnail(160, 160);
    let mut png = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(format!("data:image/png;base64,{}", B64.encode(png)))
}
