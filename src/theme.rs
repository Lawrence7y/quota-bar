//! 条/面板配色（沿用 bt-battery-bar：任务栏上恒为亮字暗晕，不做主题探测）。

/// Windows COLORREF: 0x00BBGGRR
pub type Rgb = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
}

/// 分层窗口色键（此色被透明掉，不绘制 UI 内容）。
pub const CHROMA: Rgb = rgb(0xFF, 0x00, 0xFF);

/// chips 样式"余额"¥ 图标块的边框/字符色（QuotaDock 预览里的蓝色币标）。
pub const COIN: Rgb = rgb(60, 140, 255);

#[derive(Clone, Copy)]
pub struct Theme {
    pub fg: Rgb,
    pub fg_dim: Rgb,
    pub fg_outline: Rgb,
    pub dot_ok: Rgb,
    pub dot_mid: Rgb,
    pub dot_low: Rgb,
    pub dot_off: Rgb,
    pub pill_ok: Rgb,
    pub pill_mid: Rgb,
    pub pill_low: Rgb,
    pub pill_off_bg: Rgb,
    pub pill_off_fg: Rgb,
    pub panel_bg: Rgb,
    pub panel_row_alt: Rgb,
    pub panel_border: Rgb,
}

pub fn detect() -> Theme {
    Theme {
        fg: rgb(0xFF, 0xFF, 0xFF),
        fg_dim: rgb(0xB8, 0xB8, 0xC0),
        fg_outline: rgb(0x14, 0x14, 0x18),
        dot_ok: rgb(0x88, 0xAA, 0x9A),
        dot_mid: rgb(0xB7, 0xA2, 0x79),
        dot_low: rgb(0xCF, 0x96, 0x91),
        dot_off: rgb(0xC8, 0xC8, 0xCE),
        pill_ok: rgb(0x88, 0xAA, 0x9A),
        pill_mid: rgb(0xB7, 0xA2, 0x79),
        pill_low: rgb(0xCF, 0x96, 0x91),
        pill_off_bg: rgb(0x4A, 0x4A, 0x50),
        pill_off_fg: rgb(0xF2, 0xF2, 0xF5),
        panel_bg: rgb(0x23, 0x25, 0x2A),
        panel_row_alt: rgb(0x2A, 0x2C, 0x32),
        panel_border: rgb(0x38, 0x3C, 0x44),
    }
}

/// 已用百分比 → 状态色（remaining = 100 - used）。
/// used 低=绿，中=黄，≥(100 - low_threshold)=红；无数据=灰。
pub fn level_color(t: &Theme, used: Option<f64>, low_threshold: u8) -> Rgb {
    match used {
        None => t.dot_off,
        Some(u) => {
            if u >= 100.0 - low_threshold as f64 {
                t.dot_low
            } else if u >= 50.0 {
                t.dot_mid
            } else {
                t.dot_ok
            }
        }
    }
}

/// The transparent taskbar needs stronger accents than the opaque detail panel.
pub fn strip_level_color(used: Option<f64>, low_threshold: u8) -> Rgb {
    let mut t = detect();
    t.dot_ok = rgb(0x62, 0xC7, 0x7C);
    t.dot_mid = rgb(0xD9, 0xA4, 0x41);
    t.dot_low = rgb(0xE2, 0x7E, 0x7C);
    level_color(&t, used, low_threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_packs_colorref_bgr() {
        assert_eq!(rgb(0xE8, 0x3A, 0x3A), 0x003A_3A_E8);
        assert_eq!(CHROMA, 0x00FF_00_FF);
    }

    #[test]
    fn level_color_thresholds() {
        let t = detect();
        assert_eq!(level_color(&t, None, 10), t.dot_off);
        assert_eq!(level_color(&t, Some(0.0), 10), t.dot_ok);
        assert_eq!(level_color(&t, Some(60.0), 10), t.dot_mid);
        assert_eq!(level_color(&t, Some(95.0), 10), t.dot_low);
        // 恰好 90（remaining=10=阈值）也算低
        assert_eq!(level_color(&t, Some(90.0), 10), t.dot_low);
    }
}
