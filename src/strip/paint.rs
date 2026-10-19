//! GDI 绘制原语与文本小工具（移植自 blue `app.rs` 的绘制段）。
//!
//! 色键（`SetLayeredWindowAttributes(LWA_COLORKEY)`）环境下文字必须走
//! 非抗锯齿字体配合细阴影，避免色键污染。实心面板单独使用 ClearType。

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, GetTextExtentPoint32W, HDC,
    HFONT, HGDIOBJ, PS_SOLID, RoundRect, SelectObject, SetBkMode, SetTextColor, TextOutW,
    TRANSPARENT,
};
use windows::core::w;

/// 创建条/面板字体。
///
/// `NONANTIALIASED_QUALITY`(3) 是**色键环境的硬要求**：抗锯齿产生的半透明
/// 边缘像素会与色键混合，被 `LWA_COLORKEY` 透明掉后文字边缘出现洋红色碎点
/// ——blue 真机踩坑后的结论，原样照搬（原因注释一并保留）。
pub fn create_font(height: i32, weight: i32) -> HFONT {
    create_font_quality(height, weight, 3)
}

/// ClearType is safe on the opaque panel; the color-key strip keeps its own font.
pub fn create_panel_font(height: i32, weight: i32) -> HFONT {
    create_font_quality(height, weight, 5)
}

fn create_font_quality(height: i32, weight: i32, quality: u32) -> HFONT {
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0, // OUT_DEFAULT_PRECIS
            0, // CLIP_DEFAULT_PRECIS
            quality,
            0, // DEFAULT_PITCH
            w!("Segoe UI"),
        )
    }
}

/// 释放槽位里的字体句柄并清零（`recreate_fonts` 及实例销毁时调用，
/// 及时 DeleteObject 防 GDI 句柄泄漏）。
pub fn delete_font(slot: &mut i64) {
    if *slot != 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(*slot as _));
        }
        *slot = 0;
    }
}

/// 状态圆点：画真正的圆（椭圆填充），不是方块（README 承诺"圆点"）。
pub fn draw_status_dot(dc: HDC, cx: i32, cy: i32, size: i32, color: u32) {
    let size = size.max(2);
    unsafe {
        let br = CreateSolidBrush(COLORREF(color));
        let old = SelectObject(dc, HGDIOBJ(br.0));
        let _ = Ellipse(dc, cx, cy - size / 2, cx + size, cy + size / 2 + 1);
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(br.0));
    }
}

/// 1px dark shadow for legible, unthickened taskbar text.
/// 任务栏背景不可知（亮/暗/壁纸），亮字暗晕在任何背景上都可读。
pub fn draw_text_halo(dc: HDC, x: i32, y: i32, s: &str, fg: u32, outline: u32) {
    if s.is_empty() {
        return;
    }
    unsafe {
        let mut wcs: Vec<u16> = s.encode_utf16().collect();
        wcs.push(0);
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(outline));
        // A single pixel shadow preserves small glyph shapes without a thick halo.
        for (dx, dy) in [(0, 1)] {
            let _ = TextOutW(dc, x + dx, y + dy, &wcs);
        }
        let _ = SetTextColor(dc, COLORREF(fg));
        let _ = TextOutW(dc, x, y, &wcs);
    }
}

/// 无光晕单次文字：chips 图标块内的字形用——块底是实心色，
/// 8 向光晕会把 16px 小字糊成一坨。
pub fn draw_text_plain(dc: HDC, x: i32, y: i32, s: &str, color: u32) {
    if s.is_empty() {
        return;
    }
    unsafe {
        let mut wcs: Vec<u16> = s.encode_utf16().collect();
        wcs.push(0);
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(color));
        let _ = TextOutW(dc, x, y, &wcs);
    }
}

/// 圆角矩形底座（实心填充 + 1px 描边）：容器盒 / 字母 tile / 血条共用。
/// `corner` = GDI `RoundRect` 的角椭圆尺寸（= 半径 × 2）。画完恢复并
/// 释放 pen/brush（GDI 句柄用完即删，防泄漏）。
pub fn round_rect_filled(dc: HDC, x: i32, y: i32, w: i32, h: i32, fill: u32, border: u32, corner: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    unsafe {
        let br = CreateSolidBrush(COLORREF(fill));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border));
        let old_br = SelectObject(dc, br);
        let old_pen = SelectObject(dc, pen);
        let _ = RoundRect(dc, x, y, x + w, y + h, corner, corner);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_br);
        let _ = DeleteObject(HGDIOBJ(pen.0));
        let _ = DeleteObject(HGDIOBJ(br.0));
    }
}

/// 血条：先画整宽轨道，再按 `frac_bp`（万分比 0..=10000，与 `ChipRow::bar_frac`
/// 同单位）画填充（同圆角 = 端头半圆）；`None` 只画空轨道（缺失窗口），
/// `Some` 填充色与该行数值文字同色。
pub fn draw_bar(dc: HDC, x: i32, cy: i32, w: i32, h: i32, track: u32, frac_bp: Option<u32>, fill: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let y = cy - h / 2;
    // 角椭圆 = 血条高 → 半圆端头。
    round_rect_filled(dc, x, y, w, h, track, track, h.max(2));
    if let Some(bp) = frac_bp
        && bp > 0
    {
        // 有值至少画 1px（极小占比也不至于"看着像没有"）。
        let fw = ((i64::from(w) * i64::from(bp) / 10_000) as i32).clamp(1, w);
        round_rect_filled(dc, x, y, fw, h, fill, fill, h.max(2));
    }
}

/// 用当前选入 DC 的字体测量字符串像素尺寸（宽度预算 / 折叠 / 面板度量都靠它）。
pub fn text_size(dc: HDC, s: &str) -> (i32, i32) {
    if s.is_empty() {
        return (0, 0);
    }
    unsafe {
        let buf: Vec<u16> = s.encode_utf16().collect();
        let mut size = windows::Win32::Foundation::SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &buf, &mut size);
        (size.cx, size.cy)
    }
}

/// 把字符串写入 Win32 定长缓冲（托盘 tooltip / 气泡文本），
/// 按 UTF-16 单元截断并保证 NUL 结尾。
pub fn fill_wchars(buf: &mut [u16], s: &str, max: usize) {
    let cap = buf.len().min(max);
    for (i, c) in s.encode_utf16().take(cap.saturating_sub(1)).enumerate() {
        buf[i] = c;
    }
    if cap > 0 {
        buf[cap - 1] = 0;
    }
}

/// 中间省略截断（纯函数）：超过 `max` 字符 → 首 5 字符 + `...` + 尾
/// `max - 8` 字符，结果恰好 `max` 字符。面板账号（邮箱）用。
#[cfg(test)]
pub fn short_account(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 9 {
        return s.to_string();
    }
    let head: String = chars[..5].iter().collect();
    let tail: String = chars[chars.len() - (max - 8)..].iter().collect();
    format!("{head}...{tail}")
}

/// 尾部省略截断（纯函数）：超长错误信息/任意文本画不下时截断 + `…`。
/// 不超过 `max_chars` 一律原样返回（含"正好等于"的边界）。
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 重置倒计时格式化（纯函数）：秒 → `Xs` / `Xm` / `Xh` / `Xd`。
/// 面板里窗口百分比后面的暗色小字（如 `2h`）。
pub fn format_reset(sec: i64) -> String {
    let sec = sec.max(0);
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m", sec / 60)
    } else if sec < 86400 {
        format!("{}h", sec / 3600)
    } else {
        format!("{}d", sec / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_account_keeps_short_and_truncates_long_to_exact_max() {
        // 22 字符以内原样
        assert_eq!(short_account("ye267x15@gmail.com", 22), "ye267x15@gmail.com");
        let exact: String = "a".repeat(22);
        assert_eq!(short_account(&exact, 22), exact);
        // 23 字符 → 首5 + ... + 尾14 = 恰好 22 字符
        let long = format!("{}@example.com", "x".repeat(30));
        let out = short_account(&long, 22);
        assert_eq!(out.chars().count(), 22);
        assert!(out.starts_with(&long[..5]));
        assert!(out.contains("..."));
        let tail: String = long
            .chars()
            .rev()
            .take(14)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        assert!(out.ends_with(&tail));
    }

    #[test]
    fn ellipsize_cuts_at_boundary() {
        assert_eq!(ellipsize("短错误", 28), "短错误");
        let s: String = "错".repeat(28);
        assert_eq!(ellipsize(&s, 28), s); // 正好 28 → 不截
        let s29 = format!("{s}错"); // 29 字符
        let out = ellipsize(&s29, 28); // 超 1 → 27 字符 + … = 28
        assert_eq!(out.chars().count(), 28);
        assert!(out.ends_with('…'));
        assert!(!out.contains("错错") || out.chars().filter(|c| *c == '错').count() == 27);
    }

    #[test]
    fn format_reset_units_step_at_boundaries() {
        assert_eq!(format_reset(0), "0s");
        assert_eq!(format_reset(45), "45s");
        assert_eq!(format_reset(59), "59s");
        assert_eq!(format_reset(60), "1m");
        assert_eq!(format_reset(120), "2m");
        assert_eq!(format_reset(3599), "59m");
        assert_eq!(format_reset(3600), "1h");
        assert_eq!(format_reset(7200), "2h");
        assert_eq!(format_reset(86399), "23h");
        assert_eq!(format_reset(86400), "1d");
        assert_eq!(format_reset(172800), "2d");
        // 负值（上游时钟偏差）钳到 0
        assert_eq!(format_reset(-5), "0s");
    }

    #[test]
    fn fill_wchars_truncates_and_nul_terminates() {
        let mut buf = [0u16; 8];
        fill_wchars(&mut buf, "你好世界abc", 8);
        // 7 个 UTF-16 单元 + NUL
        assert_eq!(buf[7], 0);
        assert_eq!(
            String::from_utf16(&buf[..7]).unwrap(),
            "你好世界abc"
        );
        let mut tiny = [0u16; 2];
        fill_wchars(&mut tiny, "xyz", 99);
        assert_eq!(tiny[0], 'x' as u16);
        assert_eq!(tiny[1], 0);
    }
}
