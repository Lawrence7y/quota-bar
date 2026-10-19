//! 托盘图标 / tooltip 摘要 / 低额度气泡（移植 blue `app.rs` 的 tray 段）。
//!
//! 图标挂在 **helper 窗口**（常驻隐藏顶层窗）上而不是某一条 strip 上：
//! strip 会被 Explorer 重建干掉，而托盘随 helper 存活、只在 `TaskbarCreated`
//! 时重挂一次，生命周期比 blue 的"随条重建"更简单可靠。

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFY_ICON_DATA_FLAGS, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

use crate::strip::App;
use crate::strip::layout::local_hm;
use crate::strip::paint::fill_wchars;

/// 托盘回调消息（lparam 低 16 位 = 鼠标消息码）。
pub const TRAY_MSG: u32 = 0x8000 + 20;

/// 托盘状态（HICON 句柄跨线程以 isize 存）。
pub struct TrayState {
    pub icon: isize,
    pub added: bool,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            icon: 0,
            added: false,
        }
    }
}

impl App {
    fn notify_data(&self, hwnd: HWND, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NOTIFY_ICON_DATA_FLAGS(NIF_MESSAGE.0 | NIF_ICON.0 | NIF_TIP.0);
        nid.uCallbackMessage = TRAY_MSG;
        nid.hIcon = icon;
        fill_wchars(&mut nid.szTip, tip, 128);
        nid
    }

    /// NIM_ADD（启动 / TaskbarCreated 重挂时；重复 ADD 失败无害，随后 MODIFY 生效）。
    pub(crate) fn add_tray_icon(&self) {
        let helper = self.helper_hwnd();
        if helper.0.is_null() {
            return;
        }
        unsafe {
            let icon = match crate::icon::make_icon(None, self.content().low_threshold) {
                Ok(i) => i,
                Err(e) => {
                    crate::dblog::log(&format!("tray: make_icon failed ({e})"));
                    return;
                }
            };
            {
                let mut t = self.tray.lock().unwrap();
                if t.icon != 0 {
                    let _ = DestroyIcon(HICON(t.icon as _));
                }
                t.icon = icon.0 as isize;
            }
            let nid = self.notify_data(helper, icon, "额度条");
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                self.tray.lock().unwrap().added = true;
            }
        }
    }

    /// 刷新图标（最严重已用百分比驱动饼环）与 tooltip 摘要。
    pub(crate) fn update_tray(&self) {
        let helper = self.helper_hwnd();
        if helper.0.is_null() || !self.tray.lock().unwrap().added {
            return;
        }
        let content = self.content();
        // 图标 = 启用 provider 里"已用最高"的那个（最需要被看见的）。
        let worst = content
            .rows
            .iter()
            .filter(|r| r.error.is_none())
            .filter_map(|r| r.worst_used().map(|(_, p)| p))
            .fold(None::<f64>, |acc, p| {
                Some(acc.map_or(p, |a: f64| a.max(p)))
            });
        unsafe {
            let icon = match crate::icon::make_icon(worst, content.low_threshold) {
                Ok(i) => i,
                Err(e) => {
                    crate::dblog::log(&format!("tray: make_icon failed ({e})"));
                    return;
                }
            };
            {
                let mut t = self.tray.lock().unwrap();
                if t.icon != 0 {
                    let _ = DestroyIcon(HICON(t.icon as _));
                }
                t.icon = icon.0 as isize;
            }
            let tip = tray_tip(&content);
            let nid = self.notify_data(helper, icon, &tip);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// 摘掉托盘图标（退出 / 会话结束）。挂在还活着的窗口上调用，
    /// 否则图标会残留在通知区直到下次登录。
    pub(crate) fn remove_tray_icon(&self) {
        let helper = self.helper_hwnd();
        if helper.0.is_null() {
            return;
        }
        let added = self.tray.lock().unwrap().added;
        if !added {
            return;
        }
        unsafe {
            let nid = self.notify_data(helper, HICON::default(), "");
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        let mut t = self.tray.lock().unwrap();
        t.added = false;
        if t.icon != 0 {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(t.icon as _));
            }
            t.icon = 0;
        }
    }

    /// 托盘气泡（低额度提醒：`NIF_INFO` + `NIIF_WARNING`；登录结果用 INFO）。
    pub(crate) fn show_balloon(&self, title: &str, text: &str, warn: bool) {
        let helper = self.helper_hwnd();
        if helper.0.is_null() || !self.tray.lock().unwrap().added {
            return;
        }
        let icon = {
            let t = self.tray.lock().unwrap();
            if t.icon == 0 {
                HICON::default()
            } else {
                HICON(t.icon as _)
            }
        };
        unsafe {
            let mut nid = self.notify_data(helper, icon, "");
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = if warn { NIIF_WARNING } else { NIIF_INFO };
            fill_wchars(&mut nid.szInfoTitle, title, 64);
            fill_wchars(&mut nid.szInfo, text, 256);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

/// tooltip 摘要：`MiMo 63% · GO 2% · AGY --`（超长由 fill_wchars 截到 127 UTF-16）。
fn tray_tip(content: &super::Content) -> String {
    if content.rows.is_empty() {
        return "没有启用的额度来源".to_string();
    }
    let parts: Vec<String> = content
        .rows
        .iter()
        .map(|r| {
            let short = r.provider.short_name();
            if r.error.is_some() {
                format!("{short} !")
            } else {
                match r.worst_used() {
                    Some((_, p)) => format!("{short} {:.0}%", p),
                    None => r.balance.map(|b| format!("{short} {}", b.format())).unwrap_or_else(|| format!("{short} --")),
                }
            }
        })
        .collect();
    let joined = parts.join(" · ");
    let base = if content.any_content {
        joined
    } else if content.in_grace {
        "获取中…".to_string()
    } else {
        joined
    };
    // 尾部追加 ` · 更新 {HH:MM}`：取所有参与行 `fetched_at` 的**最大值**
    //（一轮抓取近似同刻，取最新即可）；无行 / 全 0 → 不加。
    match content.rows.iter().map(|r| r.fetched_at).max() {
        Some(t) if t > 0 => format!("{base} · 更新 {}", local_hm(t)),
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StripStyle;
    use crate::model::{ProviderId, ProviderSnapshot, UsageWindow};
    use crate::strip::Content;

    #[test]
    fn tip_summarizes_worst_pct_and_marks_errors() {
        let mut mimo = ProviderSnapshot::new(ProviderId::Mimo);
        mimo.monthly = Some(UsageWindow::new_percent(63.4));
        mimo.fetched_at = 0; // 本测隔离"更新时间"后缀（全 0 → 不追加）
        let mut err = ProviderSnapshot::error(ProviderId::Antigravity, "no-key", "未配置");
        err.fetched_at = 0;
        let mut go = ProviderSnapshot::new(ProviderId::OpenCodeGo); // 无数据
        go.fetched_at = 0;
        let content = Content {
            rows: vec![mimo, go, err],
            low_threshold: 10,
            text_pct: 100,
            dock_left: false,
            strip_visible: true,
            show_all_monitors: true,
            style: StripStyle::Chips,
            any_enabled: true,
            any_content: true,
            in_grace: false,
        };
        assert_eq!(tray_tip(&content), "MiMo 63% · GO -- · AGY !");
    }

    #[test]
    fn tip_shows_grace_placeholder_before_first_data() {
        let content = Content {
            rows: vec![{
                let mut s = ProviderSnapshot::new(ProviderId::Mimo);
                s.fetched_at = 0; // 隔离更新时间后缀（本测只看占位文案）
                s
            }],
            low_threshold: 10,
            text_pct: 100,
            dock_left: false,
            strip_visible: true,
            show_all_monitors: true,
            style: StripStyle::Chips,
            any_enabled: true,
            any_content: false,
            in_grace: true,
        };
        assert_eq!(tray_tip(&content), "获取中…");
        let empty = Content {
            rows: vec![],
            ..content
        };
        assert_eq!(tray_tip(&empty), "没有启用的额度来源");
    }

    /// tooltip 尾部 ` · 更新 HH:MM` = 所有参与行 fetched_at 的**最大值**；
    /// 全 0 → 不加（无任何行同样不加，见上一测的 empty 分支）。
    #[test]
    fn tray_tip_appends_latest_update_time() {
        let mut older = ProviderSnapshot::new(ProviderId::Mimo);
        older.monthly = Some(UsageWindow::new_percent(10.0));
        older.fetched_at = 1_000_000;
        let mut newer = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        newer.monthly = Some(UsageWindow::new_percent(20.0));
        newer.fetched_at = 1_000_000 + 3600; // 比 older 晚整整 1 小时
        let content = Content {
            rows: vec![older, newer],
            low_threshold: 10,
            text_pct: 100,
            dock_left: false,
            strip_visible: true,
            show_all_monitors: true,
            style: StripStyle::Chips,
            any_enabled: true,
            any_content: true,
            in_grace: false,
        };
        // 两时刻必然差 1 小时（同日不同点 → 后缀可区分取的是谁）
        assert_ne!(
            local_hm(1_000_000),
            local_hm(1_000_000 + 3600),
            "两抓取时刻应相差 1 小时"
        );
        let tip = tray_tip(&content);
        assert!(
            tip.ends_with(&format!(" · 更新 {}", local_hm(1_000_000 + 3600))),
            "应取 fetched_at 最大值：{tip}"
        );

        // 全部归零 → 不追加
        let zeroed = Content {
            rows: content
                .rows
                .iter()
                .map(|r| {
                    let mut c = r.clone();
                    c.fetched_at = 0;
                    c
                })
                .collect(),
            ..content
        };
        assert!(!tray_tip(&zeroed).contains("更新"));
    }
}
