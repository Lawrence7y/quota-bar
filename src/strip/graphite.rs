//! Graphite list presentation. Data remains projected by the existing account and
//! DisplayMode filters; timestamps always come from the individual snapshot.
use windows::Win32::Foundation::{COLORREF, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;

use super::{geometry::Metrics, layout::local_hm, logos, paint};
use crate::model::{ProviderId, ProviderSnapshot, WindowSlot};
use crate::theme::{self, Theme};

pub const PAD: i32 = 18;
pub const WIDTH: i32 = 460;
pub const MAX_HEIGHT: i32 = 560;
pub const HEADER: i32 = 72;
pub const FOOTER: i32 = 52;
pub const DETAIL_LINE: i32 = 20;
pub const DIM: u32 = theme::rgb(0xA8, 0xB2, 0xBF);

#[derive(Debug)]
pub struct Detail {
    pub label: String,
    pub value: String,
    pub updated: String,
    pub reset: String,
    pub progress: Option<u32>,
    pub color: u32,
    pub error: bool,
}

#[derive(Debug)]
pub struct Row {
    pub provider: ProviderId,
    pub account: String,
    pub level: String,
    pub details: Vec<Detail>,
}

pub fn rows(snapshots: &[ProviderSnapshot], threshold: u8) -> Vec<Row> {
    rows_at(snapshots, threshold, crate::model::now_sec() as i64)
}

pub fn rows_at(snapshots: &[ProviderSnapshot], threshold: u8, now: i64) -> Vec<Row> {
    let t = theme::detect();
    snapshots
        .iter()
        .map(|snapshot| {
            let updated = if snapshot.fetched_at == 0 {
                String::new()
            } else {
                format!("{} 更新", local_hm(snapshot.fetched_at))
            };
            let mut details = Vec::new();
            for slot in WindowSlot::ALL {
                let Some(window) = snapshot.window(slot) else {
                    continue;
                };
                let pct = window.pct();
                let remaining = window
                    .reset_in_sec
                    .map(|s| s.max(0.0) as i64)
                    .or_else(|| window.resets_at.map(|at| (at - now).max(0)));
                details.push(Detail {
                    label: match slot {
                        WindowSlot::Rolling => "5小时",
                        WindowSlot::Weekly => "周",
                        WindowSlot::Monthly => "月",
                    }
                    .into(),
                    value: pct
                        .map(|p| format!("{p:.0}%"))
                        .unwrap_or_else(|| "--".into()),
                    updated: updated.clone(),
                    reset: remaining
                        .map(|s| format!("{}后重置", paint::format_reset(s)))
                        .unwrap_or_default(),
                    progress: pct.map(|p| (p.clamp(0.0, 100.0) * 100.0).round() as u32),
                    color: theme::level_color(&t, pct, threshold),
                    error: false,
                });
            }
            if let Some(balance) = snapshot.balance {
                details.push(Detail {
                    label: "余额".into(),
                    value: balance.format(),
                    updated: updated.clone(),
                    reset: if snapshot.use_balance {
                        "超量扣余额".into()
                    } else {
                        String::new()
                    },
                    progress: None,
                    color: t.dot_off,
                    error: false,
                });
            }
            if snapshot.error.is_some() {
                details.push(Detail {
                    label: "!".into(),
                    value: snapshot
                        .message
                        .as_deref()
                        .or(snapshot.error.as_deref())
                        .unwrap_or("额度暂不可用")
                        .into(),
                    updated: String::new(),
                    reset: String::new(),
                    progress: None,
                    color: t.dot_low,
                    error: true,
                });
            }
            if details.is_empty() {
                details.push(Detail {
                    label: "额度".into(),
                    value: "--".into(),
                    updated: String::new(),
                    reset: "等待获取额度".into(),
                    progress: None,
                    color: t.dot_off,
                    error: false,
                });
            }
            Row {
                provider: snapshot.provider,
                account: snapshot.account_label.clone().unwrap_or_default(),
                level: snapshot.level.clone().unwrap_or_default(),
                details,
            }
        })
        .collect()
}

pub fn detail_height(detail: &Detail, m: &Metrics) -> i32 {
    m.st(DETAIL_LINE + if detail.progress.is_some() { 8 } else { 0 } + 4)
}

fn inline_context(detail: &Detail) -> String {
    match (detail.updated.is_empty(), detail.reset.is_empty()) {
        (false, false) => format!("{} · {}", detail.updated, detail.reset),
        (false, true) => detail.updated.clone(),
        (true, false) => detail.reset.clone(),
        (true, true) => String::new(),
    }
}

pub fn row_height(row: &Row, m: &Metrics, narrow: bool) -> i32 {
    let identity = m.st(24
        + if row.account.is_empty() { 0 } else { 18 }
        + if row.level.is_empty() { 0 } else { 18 });
    let details: i32 = row.details.iter().map(|d| detail_height(d, m)).sum();
    m.st(20)
        + if narrow {
            identity + m.st(8) + details
        } else {
            identity.max(details)
        }
}

pub fn body_height(rows: &[Row], m: &Metrics, narrow: bool) -> i32 {
    if rows.is_empty() {
        m.st(72)
    } else {
        rows.iter().map(|r| row_height(r, m, narrow)).sum()
    }
}

pub fn narrow(width: i32, m: &Metrics) -> bool {
    width < m.st(400)
}

/// Squircle corners with a 24-DIP extent. The region also clips window hit testing.
pub fn corner_points(w: i32, h: i32, radius: i32) -> Vec<POINT> {
    let r = radius.min(w / 2).min(h / 2).max(1) as f64;
    let mut points = Vec::with_capacity(100);
    for (cx, cy, start) in [
        (w as f64 - r, r, -90.0_f64),
        (w as f64 - r, h as f64 - r, 0.0),
        (r, h as f64 - r, 90.0),
        (r, r, 180.0),
    ] {
        for i in 0..=24 {
            let a = (start + f64::from(i) * 90.0 / 24.0).to_radians();
            let c = a.cos();
            let s = a.sin();
            points.push(POINT {
                x: (cx + r * c.signum() * c.abs().sqrt()).round() as i32,
                y: (cy + r * s.signum() * s.abs().sqrt()).round() as i32,
            });
        }
    }
    points
}

pub fn region(w: i32, h: i32, radius: i32) -> HRGN {
    unsafe { CreatePolygonRgn(&corner_points(w, h, radius), WINDING) }
}

pub fn fill(dc: HDC, rect: RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let _ = FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

pub fn text(dc: HDC, font: i64, rect: RECT, value: &str, color: u32, right: bool) {
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font as _));
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(color));
        let mut buffer: Vec<u16> = value.encode_utf16().collect();
        let mut rect = rect;
        let _ = DrawTextW(
            dc,
            &mut buffer,
            &mut rect,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | if right { DT_RIGHT } else { DT_LEFT },
        );
        SelectObject(dc, old);
    }
}

pub fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub fn logo(dc: HDC, provider: ProviderId, x: i32, y: i32, size: i32) {
    let Some(bitmap) = logos::logo_bitmap(provider, size as u32) else {
        return;
    };
    unsafe {
        let src = CreateCompatibleDC(dc);
        let old = SelectObject(src, HGDIOBJ(bitmap.0));
        let _ = AlphaBlend(
            dc,
            x,
            y,
            size,
            size,
            src,
            0,
            0,
            size,
            size,
            BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            },
        );
        SelectObject(src, old);
        let _ = DeleteDC(src);
    }
}

pub fn paint_row(
    dc: HDC,
    row: &Row,
    m: &Metrics,
    width: i32,
    top: i32,
    fonts: (i64, i64, i64),
    t: &Theme,
) {
    let pad = m.st(PAD);
    let line = m.st(DETAIL_LINE);
    let is_narrow = narrow(width, m);
    let identity_w = if is_narrow {
        width - pad * 2
    } else {
        m.st(142)
    };
    let y = top + m.st(10);
    let icon = m.sc(24);
    logo(dc, row.provider, pad, y, icon);
    let name_x = pad + icon + m.st(8);
    let name = match row.provider {
        ProviderId::Codex => "ChatGPT",
        ProviderId::Mimo => "MiMo",
        ProviderId::MimoSg => "MiMo 新加坡",
        ProviderId::Antigravity => "Antigravity",
        ProviderId::StepFun => "StepFun",
        _ => row.provider.display_name(),
    };
    text(
        dc,
        fonts.0,
        rect(name_x, y, identity_w - icon - m.st(8), line),
        name,
        t.fg,
        false,
    );
    let mut identity_h = m.st(24);
    for value in [&row.account, &row.level] {
        if !value.is_empty() {
            text(
                dc,
                fonts.2,
                rect(
                    name_x,
                    y + identity_h,
                    identity_w - icon - m.st(8),
                    m.st(20),
                ),
                value,
                DIM,
                false,
            );
            identity_h += m.st(18);
        }
    }
    let x = if is_narrow {
        pad
    } else {
        pad + identity_w + m.st(14)
    };
    let w = width - pad - x;
    let mut dy = if is_narrow {
        y + identity_h + m.st(8)
    } else {
        y
    };
    for detail in &row.details {
        let label_w = m.st(42);
        let value_w = if detail.error {
            w - label_w
        } else {
            m.st(76).min(w - label_w)
        };
        text(
            dc,
            fonts.2,
            rect(x, dy, label_w, line),
            &detail.label,
            t.fg,
            false,
        );
        text(
            dc,
            fonts.1,
            rect(x + label_w, dy, value_w, line),
            &detail.value,
            if detail.error { t.dot_low } else { t.fg },
            false,
        );
        let context = inline_context(detail);
        if !context.is_empty() {
            // Timestamp is placed directly after the measured amount, not after the provider name.
            let old = unsafe { SelectObject(dc, HGDIOBJ(fonts.1 as _)) };
            let used = paint::text_size(dc, &detail.value).0.min(value_w);
            unsafe {
                SelectObject(dc, old);
            }
            let tx = x + label_w + used + m.st(8);
            text(
                dc,
                fonts.2,
                rect(tx, dy, (x + w - tx).max(0), line),
                &context,
                DIM,
                false,
            );
        }
        if let Some(progress) = detail.progress {
            paint::draw_bar(
                dc,
                x,
                dy + line + m.st(4),
                w,
                m.sc(4),
                t.panel_border,
                Some(progress),
                detail.color,
            );
        }
        dy += detail_height(detail, m);
    }
    let bottom = top + row_height(row, m, is_narrow) - 1;
    fill(dc, rect(pad, bottom, width - pad * 2, 1), t.panel_border);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Money, MoneyUnit, UsageWindow};
    #[test]
    fn each_account_keeps_its_fetch_time_and_missing_is_not_zero() {
        let mut first = ProviderSnapshot::new(ProviderId::Codex);
        first.fetched_at = 1_700_000_000;
        first.rolling = Some(UsageWindow::new_percent(0.0));
        first.weekly = Some(UsageWindow::default());
        let mut second = first.clone();
        second.fetched_at += 60;
        let result = rows(&[first, second], 10);
        assert_eq!(result[0].details[0].value, "0%");
        assert_eq!(result[0].details[0].progress, Some(0));
        assert_eq!(result[0].details[1].value, "--");
        assert_eq!(result[0].details[1].progress, None);
        assert_ne!(result[0].details[0].updated, result[1].details[0].updated);
    }
    #[test]
    fn projected_balance_and_error_rows_remain_visible() {
        let mut raw = ProviderSnapshot::new(ProviderId::Mimo);
        raw.monthly = Some(UsageWindow::new_percent(90.0));
        raw.balance = Some(Money {
            amount: 12.34,
            unit: MoneyUnit::Cny,
        });
        let balance = crate::config::DisplayMode::Balance.project(raw);
        let error = ProviderSnapshot::error(ProviderId::Claude, "expired", "请重新登录");
        let result = rows(&[balance, error], 10);
        assert_eq!(result[0].details.len(), 1);
        assert_eq!(result[0].details[0].value, "¥12.34");
        assert!(result[1].details[0].error);
        assert!(logos::logo_bytes(result[1].provider).is_some());
    }
    #[test]
    fn corners_and_layout_scale_without_clipping() {
        for dpi in [96, 120, 144, 192] {
            let m = Metrics::with_text_pct(dpi, 100);
            let w = m.st(520);
            let h = m.st(400);
            let points = corner_points(w, h, m.sc(24));
            assert!(
                points
                    .iter()
                    .all(|p| p.x >= 0 && p.x <= w && p.y >= 0 && p.y <= h)
            );
            assert!(!points.iter().any(|p| p.x == 0 && p.y == 0));
            let rows = rows(&[ProviderSnapshot::new(ProviderId::Mimo)], 10);
            assert!(body_height(&rows, &m, true) >= body_height(&rows, &m, false));
        }
    }
}
