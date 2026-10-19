//! 供应商真 logo：favicon PNG 编译期内嵌 + 进程级位图缓存（chips 段的
//! 面板和无框额度条均使用真实图标，错误/等待状态也保留供应商身份）。
//!
//! 管线：`include_bytes!` → `png` 解码 RGBA8 → **预乘 alpha** → 最近邻重采样
//! 到 `target_px²`（**尺寸随 DPI**，见 `logo_bitmap(id, target_px)`）→ 32bpp
//! top-down `CreateDIBSection` 写 BGRA → 按 `(供应商, 尺寸)` 缓存 →
//! 绘制期 `AlphaBlend`（源已预乘，`AC_SRC_ALPHA`）1:1 贴到 tile 底板中央。
//!
//! 为什么在**构建缓存时**缩放：`AlphaBlend` 只做混合不做缩放——每个
//! `(供应商, target_px)` 只重采样一次并缓存，绘制端 1:1 贴、永不现场重采样。
//! 尺寸由绘制端按 DPI 算（`sc(TILE) − 2·sc(LOGO_PAD)`），高分屏不再显小；
//! `png` 解码不单独缓存（全表 × 少量尺寸，新尺寸多解一次成本可忽略，
//! 也省一层中间态的 Send/Sync 考量）。

use std::collections::HashMap;
use std::io::Cursor;
use std::mem::{size_of, zeroed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, GetDC, HBITMAP,
    ReleaseDC, HDC, HGDIOBJ, CreateCompatibleDC, SelectObject, DeleteDC, TransparentBlt,
};

use crate::model::ProviderId;

/// A binary alpha mask avoids blending the logo with the strip's magenta color key.
/// No artificial tile background or outline is needed. Opaque panels use AlphaBlend.
pub fn draw_strip_logo(dc: HDC, id: ProviderId, x: i32, y: i32, size: u32) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<(ProviderId, u32), isize>>> = OnceLock::new();
    if size == 0 || size > 1024 { return false; }
    let mut cache = CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    let raw = if let Some(&raw) = cache.get(&(id, size)) { raw } else {
        let Some(bytes) = logo_bytes(id) else { return false; };
        let Some((w, h, rgba)) = decode_rgba8(bytes) else { return false; };
        let mut pixels = resize_nearest(&rgba, w, h, size as usize, size as usize);
        for pixel in pixels.chunks_exact_mut(4) {
            if pixel[3] < 128 { pixel.copy_from_slice(&[255, 0, 255, 255]); }
            else { pixel[3] = 255; }
        }
        let Some(bitmap) = create_bgra_bitmap(&pixels, size) else { return false; };
        let raw = bitmap.0 as isize;
        cache.insert((id, size), raw);
        raw
    };
    drop(cache);
    unsafe {
        let src = CreateCompatibleDC(dc);
        let old = SelectObject(src, HGDIOBJ(raw as _));
        let ok = TransparentBlt(dc, x, y, size as i32, size as i32, src, 0, 0,
            size as i32, size as i32, crate::theme::CHROMA).as_bool();
        SelectObject(src, old);
        let _ = DeleteDC(src);
        ok
    }
}

/// PNG 文件头魔数（8 字节）。
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 该供应商 logo 的内嵌 PNG 字节（文件名 = provider id / config key）。
///
/// 用字符串匹配：未来新增 provider 未配资产自然落 `_ => None`（条上回退
/// 字母 tile），也不会因枚举新增而漏改编译报错。全表资产齐全。
pub fn logo_bytes(id: ProviderId) -> Option<&'static [u8]> {
    Some(match id.as_str() {
        "mimo" => include_bytes!("../../assets/logos/mimo.png"),
        "mimo-sg" => include_bytes!("../../assets/logos/mimo-sg.png"),
        "opencode-go" => include_bytes!("../../assets/logos/opencode-go.png"),
        "antigravity" => include_bytes!("../../assets/logos/antigravity.png"),
        "codex" => include_bytes!("../../assets/logos/codex.png"),
        "claude" => include_bytes!("../../assets/logos/claude.png"),
        "grok" => include_bytes!("../../assets/logos/grok.png"),
        "kimi-code" => include_bytes!("../../assets/logos/kimi-code.png"),
        "glm-zai" => include_bytes!("../../assets/logos/glm-zai.png"),
        "glm-zhipu" => include_bytes!("../../assets/logos/glm-zhipu.png"),
        "command-code" => include_bytes!("../../assets/logos/command-code.png"),
        "deepseek" => include_bytes!("../../assets/logos/deepseek.png"),
        "openrouter" => include_bytes!("../../assets/logos/openrouter.png"),
        "stepfun" => include_bytes!("../../assets/logos/stepfun.png"),
        _ => return None,
    })
}

/// 进程级位图缓存：键 = `(供应商, 目标边长 px)`——同 id 同尺寸命中，
/// **不同 DPI 尺寸各自成条目**（每张几 KB，**刻意不释放**）。
/// 句柄值以 `isize` 存放：`HBITMAP` 含裸指针不是 `Send`，直接进静态
/// `Mutex<HashMap>` 过不了 Sync。
fn bitmap_cache() -> &'static Mutex<HashMap<(ProviderId, u32), isize>> {
    static CACHE: OnceLock<Mutex<HashMap<(ProviderId, u32), isize>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（按需构建）该供应商 logo 在 `target_px` 见方下的预乘位图。
/// `target_px` 由绘制端按 DPI 算（`sc(TILE) − 2·sc(LOGO_PAD)`，钳 ≥8）；
/// 同 id 同尺寸命中缓存，不同尺寸各自构建一条。
/// 无资产 / 尺寸非法 / 解码失败 → `None`（绘制回退字母 tile）。
pub fn logo_bitmap(id: ProviderId, target_px: u32) -> Option<HBITMAP> {
    if target_px == 0 || target_px > 1024 {
        return None; // 防手滑/异常尺寸（正常链路钳在 ≥8 且远小于 1024）
    }
    let bytes = logo_bytes(id)?;
    let mut cache = bitmap_cache().lock().ok()?;
    if let Some(&raw) = cache.get(&(id, target_px)) {
        return Some(HBITMAP(raw as _));
    }
    let hbmp = build_logo_bitmap(bytes, target_px)?;
    cache.insert((id, target_px), hbmp.0 as isize);
    Some(hbmp)
}

/// 解码 → 预乘 → 最近邻缩放到 `target_px²` → DIB。任一步失败返回 `None`。
fn build_logo_bitmap(bytes: &[u8], target_px: u32) -> Option<HBITMAP> {
    // 先验 PNG 魔数：include_bytes 打进来的资产被截断/误放时快速失败，
    // 不进 png 解码器。
    if bytes.len() < 8 || bytes[..8] != PNG_MAGIC {
        return None;
    }
    let (w, h, mut rgba) = decode_rgba8(bytes)?;
    // 先预乘后缩放：最近邻是纯像素拷贝，顺序不影响结果（注明）。
    premultiply(&mut rgba);
    let px = target_px as usize;
    let small = resize_nearest(&rgba, w, h, px, px);
    create_bgra_bitmap(&small, target_px)
}

/// `png` 解码到 8bit RGBA8（EXPAND 展开调色板/低位深，STRIP_16 去 16bit）。
/// 输出行按源行主序紧密排列。
fn decode_rgba8(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let stride = info.line_size;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = buf.get(y * stride..)?;
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(row.get(..w * 4)?),
            png::ColorType::Rgb => {
                for x in 0..w {
                    let p = row.get(x * 3..x * 3 + 3)?;
                    rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
                }
            }
            png::ColorType::Grayscale => {
                for x in 0..w {
                    let v = *row.get(x)?;
                    rgba.extend_from_slice(&[v, v, v, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for x in 0..w {
                    let p = row.get(x * 2..x * 2 + 2)?;
                    rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
            }
            // EXPAND 后不应再出现调色板索引；真出现（异常资产）→ 放弃 logo。
            png::ColorType::Indexed => return None,
        }
    }
    Some((w, h, rgba))
}

/// 预乘 alpha（`AC_SRC_ALPHA` 的 AlphaBlend 要求源已预乘）：
/// `c' = c * a / 255`（+127 四舍五入）。
fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
        px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
        px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
    }
}

/// 最近邻重采样到目标尺寸（纯 Rust 双重循环，构建缓存时一次性完成——
/// `AlphaBlend` 绘制端 1:1，不做任何现场缩放）。
fn resize_nearest(src: &[u8], w: usize, h: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 4];
    if w == 0 || h == 0 || src.len() < w * h * 4 {
        return out;
    }
    for y in 0..dh {
        let sy = (y * h / dh).min(h - 1);
        for x in 0..dw {
            let sx = (x * w / dw).min(w - 1);
            let s = (sy * w + sx) * 4;
            let d = (y * dw + x) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

/// 32bpp **top-down** `CreateDIBSection`（`biHeight` 取负 → 内存首行即顶行，
/// 与源行序一致、无需翻转）；预乘 RGBA → DIB 的 BGRA 字节序
/// （小端 u32 = `A<<24 | R<<16 | G<<8 | B`）。位图边长 = `target_px`。
fn create_bgra_bitmap(rgba: &[u8], target_px: u32) -> Option<HBITMAP> {
    let side = target_px as i32;
    let px_count = (side * side) as usize;
    if rgba.len() < px_count * 4 {
        return None;
    }
    unsafe {
        let dc = GetDC(HWND::default());
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = side;
        bmi.bmiHeader.biHeight = -side; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let created = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0);
        let hbmp = match created {
            Ok(h) => h,
            Err(_) => {
                let _ = ReleaseDC(HWND::default(), dc);
                return None;
            }
        };
        let dst = std::slice::from_raw_parts_mut(bits as *mut u32, px_count);
        for (i, px) in rgba.chunks_exact(4).take(px_count).enumerate() {
            dst[i] = (px[3] as u32) << 24 | (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
        let _ = ReleaseDC(HWND::default(), dc);
        Some(hbmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_provider_logo_is_embedded_png() {
        let mut with_logo = 0usize;
        for id in ProviderId::ALL {
            match logo_bytes(id) {
                Some(b) => {
                    assert!(b.len() > 8, "{id:?} logo 字节过短");
                    assert_eq!(&b[..8], &PNG_MAGIC, "{id:?} 不是 PNG 文件");
                    with_logo += 1;
                }
                // 未列出的 id（未来新增 provider 未配资产）→ None，回退字母。
                None => {}
            }
        }
        // 全表资产齐全（assets/logos/ 文件名 = provider id）。
        assert_eq!(
            with_logo,
            ProviderId::ALL.len(),
            "全 {} 家供应商 logo 应全部内嵌",
            ProviderId::ALL.len()
        );
        assert!(logo_bytes(ProviderId::Mimo).is_some());
        assert!(logo_bytes(ProviderId::GlmZhipu).is_some());
    }

    #[test]
    fn every_provider_renders_on_the_frameless_color_key_strip() {
        use windows::Win32::Graphics::Gdi::{CreateCompatibleBitmap, CreateSolidBrush,
            FillRect, DeleteObject, GetPixel};
        use windows::Win32::Foundation::{COLORREF, RECT};
        unsafe {
            let screen=GetDC(HWND::default());
            let dc=CreateCompatibleDC(screen);
            let bmp=CreateCompatibleBitmap(screen,28,28);
            let old=SelectObject(dc,HGDIOBJ(bmp.0));
            let brush=CreateSolidBrush(COLORREF(crate::theme::CHROMA));
            let bounds=RECT{left:0,top:0,right:28,bottom:28};
            for provider in ProviderId::ALL {
                let _=FillRect(dc,&bounds,brush);
                assert!(draw_strip_logo(dc,provider,2,2,24),"{provider:?}");
                assert_eq!(GetPixel(dc,0,0).0,crate::theme::CHROMA,"no tile frame");
                assert!((2..26).any(|y|(2..26).any(|x|GetPixel(dc,x,y).0!=crate::theme::CHROMA)),
                    "{provider:?} artwork must be visible");
            }
            SelectObject(dc,old); let _=DeleteObject(HGDIOBJ(bmp.0));
            let _=DeleteObject(HGDIOBJ(brush.0)); let _=DeleteDC(dc);
            let _=ReleaseDC(HWND::default(),screen);
        }
    }

    #[test]
    fn logo_bitmap_caches_per_size_and_rejects_bad_px() {
        // 所有有资产的 id 都能走通 解码→预乘→按尺寸缩放→DIB 管线
        // （24px = DPI144 下的 sc(TILE)-2·sc(LOGO_PAD) 目标）。
        for id in ProviderId::ALL {
            if logo_bytes(id).is_some() {
                assert!(
                    logo_bitmap(id, 24).is_some(),
                    "{id:?} logo 位图构建失败（PNG 解码/DIB）"
                );
            }
        }
        // 同 id 同尺寸 → 缓存命中（句柄相等）。
        let a = logo_bitmap(ProviderId::Antigravity, 24).expect("Antigravity 24px");
        let b = logo_bitmap(ProviderId::Antigravity, 24).expect("Antigravity 24px（缓存）");
        assert_eq!(a.0, b.0, "同 id 同尺寸应命中缓存而非重复构建");
        // 不同尺寸 → 各自成条目（两张都是存活位图，句柄必然不同）。
        let small = logo_bitmap(ProviderId::Antigravity, 14).expect("Antigravity 14px");
        assert_ne!(
            small.0, a.0,
            "不同尺寸应各自构建缓存条目，而不是互相命中"
        );
        // 非法尺寸 → None（绘制回退字母）。
        assert!(logo_bitmap(ProviderId::Mimo, 0).is_none());
        assert!(logo_bitmap(ProviderId::Mimo, 4096).is_none());
    }
}
