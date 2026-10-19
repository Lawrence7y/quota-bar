//! 几何与任务栏定位：DPI/字号缩放、任务栏与托盘定位、嵌入 placement、多屏枚举。
//!
//! 核心管线整体移植自 `D:\Project\blue\bt-battery-bar\src\app.rs`（逐段对应
//! `find_taskbar` / `find_tray_left` / `client_origin_screen` / `rect_offset_by` /
//! `taskbar_client_geometry` / `strip_placement_in_taskbar`），按本项目需要扩展：
//!
//! 1. **按任务栏参数化**：定位助手全部接收"自己的" `taskbar_hwnd`——多屏下每条
//!    任务栏各自 `FindWindowExW(taskbar, …)` 找托盘子窗口，绝不找成主屏的。
//! 2. **多屏枚举**：`enumerate_taskbars` 主屏 `FindWindowW("Shell_TrayWnd")` +
//!    `FindWindowExW(NULL, prev, "Shell_SecondaryTrayWnd", NULL)` 循环遍历副屏
//!    （`FindWindowW` 只返回第一个匹配，拿不到多个副屏任务栏）。

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MonitorFromWindow, ScreenToClient,
    MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetWindowRect, MONITORINFOF_PRIMARY,
};

use crate::config::{MAX_TEXT_PCT, MIN_TEXT_PCT};

// ---------------------------------------------------------------------------
// 布局常量：全部是 96 DPI（100% 缩放）设计值，经 `Metrics::sc/st` 换算成物理像素。
// （blue 的教训：条高取自真实任务栏，150% 下 72px 高的条里画 16px 小字——所有
//  几何量与字体都必须缩放，见 `Metrics`。）

/// 降级浮动条高度（找不到任务栏时贴工作区上沿用）。
pub const STRIP_HEIGHT: i32 = 40;
/// 退化任务栏（高度被压到 10px 等）的高度下限。
pub const MIN_STRIP_HEIGHT: i32 = 24;
/// 条宽下限（极窄任务栏 / 极端字号时保底）。
pub const MIN_STRIP_WIDTH: i32 = 40;
/// 条内左右留白（绘制起点 cx = MARGIN_X）。
pub const MARGIN_X: i32 = 6;
/// 状态圆点直径。
pub const DOT: i32 = 8;
/// chips 样式圆角图标块边长（96 DPI 设计值，经 `Metrics::sc` 缩放——
/// 只跟 DPI 不跟字号档；16→24 放大底板，真 logo 同步跟随见 `LOGO_PAD`）。
pub const TILE: i32 = 24;
/// logo 与底板的设计单位边距（`sc` 缩放）：logo 边长 =
/// `sc(TILE) − 2·sc(LOGO_PAD)`（DPI144：36 − 6 = **30px**，最初 14 → 现 30）。
pub const LOGO_PAD: i32 = 2;

// ---- chips 盒式血条（QuotaDock 形态）几何常量 ----
/// 无框套餐之间的纯间隙（sc 缩放；替代 text 样式的 ` · ` 文本分隔，
/// 也是 `fold_segments` 在 chips 下的 `sep` 值）。
pub const CHIP_GAP: i32 = 18;
/// 盒内容左右内边距（st 缩放）。
pub const CHIP_PAD_X: i32 = 5;
/// 盒内行块上下内边距（st；参与 `rows_for_height` 判定）。
pub const CHIP_PAD_Y: i32 = 4;
/// 盒内 tile → 行区 的水平间距（st）。
pub const CHIP_TILE_GAP: i32 = 6;
/// 血条行行高（st；需求"行高 ≈ st(11~12px)"取 12）。
pub const CHIP_ROW_H: i32 = 12;
/// 行区内 列 之间的间距（标签|血条|数值，st）。
pub const CHIP_COL_GAP: i32 = 6;
/// 紧凑进度条设计长度，兼顾多供应商显示与可辨识度。
pub const CHIP_BAR_W: i32 = 34;
/// 血条高度（sc 缩放，需求"≈4px（sc）"）。
pub const CHIP_BAR_H: i32 = 5;
/// 圆点与短名的间距。
pub const DOT_GAP: i32 = 6;
/// 段间 / "+n" 胶囊前的分隔文本（宽度按实际测量，这个常量只是文案）。
pub const SEP_TEXT: &str = " · ";
/// 托盘测不到时给时钟/图标区预留的任务栏宽度（blue 同款）。
pub const TASKBAR_RESERVE: i32 = 200;
/// 条与托盘簇之间的间隙（**永不越界遮挡托盘** 的关键余量）。
pub const GAP_TRAY: i32 = 6;
/// 左停靠时条距任务栏左沿的内缩。
pub const LEFT_INSET: i32 = 10;
/// "+n" 折叠胶囊的预留宽（含内边距，不含前导分隔）。
pub const OVERFLOW_PILL_MIN: i32 = 44;
/// 胶囊文字左右内边距。
pub const PILL_PAD: i32 = 5;

pub const BASE_DPI: i32 = 96;
/// 字体设计值（96 DPI 下的 pt 高），经 `Metrics::st` 跟随字号档缩放。
pub const NAME_FONT_PT: i32 = 13;
pub const PILL_FONT_PT: i32 = 12;

/// 条上错误原因片段的最大字符数（面板里才是全文）。
pub const MAX_STRIP_ERROR_CHARS: usize = 16;
/// 无任何数据/错误时显示"获取中…"的宽限期；超过后隐藏条（保留托盘）。
pub const GRACE_SECS: u64 = 60;

/// isize（跨线程存窗口句柄用，HWND 含裸指针不是 Send）→ HWND。
pub fn hw(raw: isize) -> HWND {
    HWND(raw as _)
}

/// HWND → isize（存进实例结构）。
pub fn hi(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

// ---------------------------------------------------------------------------
/// 96 DPI 设计值 → 当前 DPI × 用户字号百分比 的物理像素。
///
/// `sc()` 只按 DPI 缩放：用于与字号无关的几何量（浮动条高度、任务栏预留、
/// 与托盘的间隙等）。
/// `st()` 在 DPI 之上再乘用户字号档（菜单"字号" 80/100/120/140）：用于字体、
/// 圆点、间距、内边距、面板行高——让整套排版随字号一起缩放，而不是只改字。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    pub dpi: i32,
    /// 用户字号百分比（100 = 设计值）。
    pub text_pct: i32,
}

impl Metrics {
    /// 仅测试使用：生产路径一律带用户字号档（`with_text_pct`）。
    #[cfg(test)]
    pub fn new(dpi: i32) -> Self {
        Self::with_text_pct(dpi, 100)
    }

    pub fn with_text_pct(dpi: i32, text_pct: u32) -> Self {
        Self {
            dpi: if dpi <= 0 { BASE_DPI } else { dpi },
            text_pct: text_pct.clamp(MIN_TEXT_PCT, MAX_TEXT_PCT) as i32,
        }
    }

    pub fn sc(&self, v: i32) -> i32 {
        (v * self.dpi + 48) / 96
    }

    pub fn st(&self, v: i32) -> i32 {
        let d = self.sc(v) * self.text_pct;
        (d + 50) / 100
    }
}

// ---------------------------------------------------------------------------
/// 主屏任务栏（`Shell_TrayWnd`）。找不到 = Explorer 没就绪。
pub fn find_primary_taskbar() -> Option<HWND> {
    unsafe {
        let shell = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).unwrap_or_default();
        if shell.0.is_null() {
            None
        } else {
            Some(shell)
        }
    }
}

/// 枚举应当挂 strip 的任务栏：主屏 `Shell_TrayWnd` + （多屏开关打开时）全部
/// 副屏 `Shell_SecondaryTrayWnd`。
///
/// 副屏必须用 `FindWindowExW(NULL, prev, class, NULL)` 循环：`FindWindowW`
/// 只返回第一个匹配窗口，拿不到第二块/第三块屏的任务栏。
/// 返回顺序稳定（主屏在前）；`show_all_monitors=false` 或枚举为空时降级为
/// "只有主屏 / 无条"（调用方按 `plan_instance_sync` 销毁多余实例）。
pub fn enumerate_taskbars(show_all_monitors: bool) -> Vec<HWND> {
    let mut out = Vec::new();
    if let Some(primary) = find_primary_taskbar() {
        out.push(primary);
    }
    if !show_all_monitors {
        return out;
    }
    unsafe {
        let mut prev = HWND::default();
        loop {
            let h = FindWindowExW(
                HWND::default(),
                prev,
                w!("Shell_SecondaryTrayWnd"),
                PCWSTR::null(),
            )
            .unwrap_or_default();
            if h.0.is_null() {
                break;
            }
            out.push(h);
            prev = h;
        }
    }
    out.retain(|taskbar| usable_taskbar(*taskbar));
    out
}

/// 托盘簇（通知区图标行）左沿的**屏幕坐标**。
///
/// 三级回退（移植 blue）：`TrayNotifyWnd` → `SystemTray` →
/// `NotifyIconOverflowWindow`。前两级必须在**这条任务栏下面**找
/// （`FindWindowExW(taskbar, …)`）——多屏下副屏各有一套托盘子窗口，
/// 找成主屏的会把副屏条挤到错误位置。第三级是全局溢出窗，只在主屏任务栏
/// 上兜底（副屏直接返回 None，走 `TASKBAR_RESERVE` 预留）。
pub fn find_tray_left(taskbar: HWND) -> Option<i32> {
    unsafe {
        let mut h = FindWindowExW(taskbar, HWND::default(), w!("TrayNotifyWnd"), PCWSTR::null())
            .unwrap_or_default();
        if h.0.is_null() {
            h = FindWindowExW(taskbar, HWND::default(), w!("SystemTray"), PCWSTR::null())
                .unwrap_or_default();
        }
        if h.0.is_null() {
            // 全局溢出窗：只有当这就是主屏任务栏时坐标才可信。
            if let Some(primary) = find_primary_taskbar()
                && primary == taskbar
            {
                h = FindWindowW(w!("NotifyIconOverflowWindow"), PCWSTR::null()).unwrap_or_default();
            }
        }
        if !h.0.is_null() {
            let mut r = RECT::default();
            let _ = GetWindowRect(h, &mut r);
            if r.right > r.left {
                return Some(r.left);
            }
        }
        None
    }
}

/// `hwnd` 客户区原点的屏幕坐标。
pub unsafe fn client_origin_screen(hwnd: HWND) -> POINT {
    let mut p = POINT { x: 0, y: 0 };
    // ScreenToClient 把屏幕坐标映射到客户区；作用在 (0,0) 上得到的是
    // "客户区原点的屏幕坐标" 取负——取负还原。
    let _ = unsafe { ScreenToClient(hwnd, &mut p) };
    POINT { x: -p.x, y: -p.y }
}

/// 把屏幕坐标矩形平移到以 `origin_screen` 为原点的坐标系（纯函数，可单测）。
pub fn rect_offset_by(r: RECT, origin_screen: POINT) -> RECT {
    RECT {
        left: r.left - origin_screen.x,
        top: r.top - origin_screen.y,
        right: r.right - origin_screen.x,
        bottom: r.bottom - origin_screen.y,
    }
}

/// 指定任务栏窗口矩形（屏幕坐标）。无效/不存在 → None。
pub fn taskbar_screen_rect(taskbar: HWND) -> Option<RECT> {
    unsafe {
        if taskbar.0.is_null() {
            return None;
        }
        let mut tb = RECT::default();
        let _ = GetWindowRect(taskbar, &mut tb);
        if tb.right <= tb.left || tb.bottom <= tb.top {
            return None;
        }
        Some(tb)
    }
}

/// Explorer can retain a tiny, inactive secondary-taskbar placeholder.
/// It is not a docking target and must not get a strip window.
pub fn usable_taskbar(taskbar: HWND) -> bool {
    let Some(rect) = taskbar_screen_rect(taskbar) else { return false; };
    unsafe {
        let mon = MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut info).as_bool() { return false; }
        let screen = info.rcMonitor;
        (rect.right-rect.left) >= (screen.right-screen.left)/2
            || (rect.bottom-rect.top) >= (screen.bottom-screen.top)/2
    }
}

/// 指定任务栏的几何（**该任务栏自己的客户区坐标**）——正是 WS_CHILD 条
/// `SetWindowPos` 需要的坐标系。找不到任务栏 → None。
pub fn taskbar_client_geometry(taskbar: HWND) -> Option<(RECT, Option<i32>)> {
    let tb = taskbar_screen_rect(taskbar)?;
    unsafe {
        let origin = client_origin_screen(taskbar);
        let tb_client = rect_offset_by(tb, origin);
        let tray_left_client = find_tray_left(taskbar).map(|l| l - origin.x);
        Some((tb_client, tray_left_client))
    }
}

/// 窗口所在显示器的工作区（屏幕坐标，负坐标副屏天然正确——`rcWork` 来自
/// 该显示器自己的 `MONITORINFO`）。浮动降级条与面板定位都用它。
pub fn monitor_work_area(hwnd: HWND) -> Option<RECT> {
    unsafe {
        if hwnd.0.is_null() {
            return None;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if mon.0.is_null() {
            return None;
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            Some(mi.rcWork)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
/// 一块物理显示器（菜单勾选 / 门控用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    /// `\\.\DISPLAY1` 这类 `szDevice`（存进 config 白名单的值）。
    pub device: String,
    /// 是否主屏（`MONITORINFOF_PRIMARY`）。
    pub primary: bool,
    pub width: i32,
    pub height: i32,
}

pub fn display_dock_bounds() -> Vec<(String, RECT, RECT)> {
    unsafe extern "system" fn callback(mon: HMONITOR, _dc: HDC, _rect: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let out = &mut *(data.0 as *mut Vec<(String, RECT, RECT)>);
            if let Some(info) = monitor_info_ex(mon) {
                out.push((sz_device_string(&info.szDevice), info.monitorInfo.rcMonitor, info.monitorInfo.rcWork));
            }
            BOOL(1)
        }
    }
    let mut out = Vec::new();
    unsafe { let _ = EnumDisplayMonitors(HDC::default(), None, Some(callback), LPARAM(&mut out as *mut _ as isize)); }
    out
}

/// `MONITORINFOEXW.szDevice` 的 [u16;32] → String（遇 0 截断）。
fn sz_device_string(buf: &[u16; 32]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 读带 `szDevice` 的 `MONITORINFOEXW`（`GetMonitorInfoW` 形参按
/// `*mut MONITORINFO` 传，`cbSize` 用 EX 结构尺寸——两种尺寸实现都接受）。
unsafe fn monitor_info_ex(mon: HMONITOR) -> Option<MONITORINFOEXW> {
    let mut ex = MONITORINFOEXW::default();
    ex.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe {
        GetMonitorInfoW(mon, &mut ex as *mut MONITORINFOEXW as *mut MONITORINFO)
    };
    if ok.as_bool() {
        Some(ex)
    } else {
        None
    }
}

/// 窗口（任务栏）所在显示器的 `(szDevice, 是否主屏)`。
/// strip 的期望实例集合对**每个任务栏单独**调它，再过 `should_show_on` 门控
/// （Some 白名单模式下主屏同样受管）。
pub fn monitor_device_of(hwnd: HWND) -> Option<(String, bool)> {
    unsafe {
        if hwnd.0.is_null() {
            return None;
        }
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if mon.0.is_null() {
            return None;
        }
        let ex = monitor_info_ex(mon)?;
        Some((
            sz_device_string(&ex.szDevice),
            ex.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        ))
    }
}

/// 实时枚举物理显示器（菜单**每次打开**都重新探测，热插拔后仍准）。
/// 顺序 = `EnumDisplayMonitors` 枚举序（菜单里"显示器n"的 n）。
pub fn detect_displays() -> Vec<DisplayInfo> {
    unsafe extern "system" fn detect_cb(
        mon: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        unsafe {
            let out = &mut *(data.0 as *mut Vec<DisplayInfo>);
            if let Some(ex) = monitor_info_ex(mon) {
                out.push(DisplayInfo {
                    device: sz_device_string(&ex.szDevice),
                    primary: ex.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                    width: ex.monitorInfo.rcMonitor.right - ex.monitorInfo.rcMonitor.left,
                    height: ex.monitorInfo.rcMonitor.bottom - ex.monitorInfo.rcMonitor.top,
                });
            }
            BOOL(1) // 继续枚举
        }
    }
    let mut out: Vec<DisplayInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(detect_cb),
            LPARAM(&mut out as *mut Vec<DisplayInfo> as isize),
        );
    }
    out
}

/// 内容宽度预算：`LEFT_INSET` → `托盘左沿 - GAP_TRAY` 之间的全部可用宽。
/// 托盘测不到时退回 `TASKBAR_RESERVE` 预留（时钟/图标区）。条永远不越界
/// 遮挡托盘——`strip_placement_in_taskbar` 也用同一个右沿做钳制。
pub fn width_budget(tb_client: RECT, tray_left: Option<i32>, m: &Metrics) -> i32 {
    let left = tb_client.left + m.sc(LEFT_INSET);
    let right = match tray_left {
        Some(l) if l > tb_client.left => l - m.sc(GAP_TRAY),
        _ => tb_client.right - m.sc(TASKBAR_RESERVE),
    };
    (right - left).max(m.st(MIN_STRIP_WIDTH))
}

/// 嵌入条 placement 的纯函数（移植 blue，逐位保留其钳制语义）：
/// 给定任务栏自身客户区矩形、通知区左沿（同坐标系）、停靠侧与期望宽度，
/// 返回 WS_CHILD 条 `SetWindowPos` 的 `(x, y, w, h)`。条贴着托盘簇停靠，
/// 无论任务栏多窄都钳在任务栏内部（竖排任务栏等退化情况同样成立）。
///
/// 所有几何常量经 `m` 按 DPI 缩放（96 DPI 时 sc(v) == v，纯函数测试照旧）。
pub fn strip_placement_in_taskbar(
    tb_client: RECT,
    tray_left_client: Option<i32>,
    dock_left: bool,
    width: i32,
    m: &Metrics,
) -> (i32, i32, i32, i32) {
    let tb_w = (tb_client.right - tb_client.left).max(0);
    let tb_h = (tb_client.bottom - tb_client.top).max(m.sc(MIN_STRIP_HEIGHT));
    let avail_w = (tb_w - m.sc(4)).max(m.st(MIN_STRIP_WIDTH));
    let w = width.clamp(m.st(MIN_STRIP_WIDTH), avail_w);
    let right_edge = match tray_left_client {
        Some(l) if l > tb_client.left => l - m.sc(GAP_TRAY),
        _ => tb_client.right - m.sc(TASKBAR_RESERVE),
    };
    let max_x = (right_edge - w).max(tb_client.left + m.sc(2));
    let x = if dock_left {
        (tb_client.left + m.sc(LEFT_INSET)).min(max_x)
    } else {
        max_x
    };
    (x, tb_client.top, w, tb_h)
}

// ---------------------------------------------------------------------------
/// 实例集合同步计划（多屏核心纯函数）：
/// `existing` = 当前已挂 strip 的任务栏句柄集合，`desired` = 多屏枚举出的
/// "现在应当挂 strip" 的任务栏集合。返回需要新建 / 销毁的句柄。
///
/// - 多屏枚举为空（Explorer 未就绪、全部拔屏）→ 全部销毁、一个不建，
///   进程降级为"只有托盘"（下个 1s tick 或 TaskbarCreated 再补回来）。
/// - 新增屏出现任务栏 → 只建不删；拔屏 → 只删不建。
#[cfg(test)]
pub fn plan_instance_sync(existing: &[isize], desired: &[isize]) -> SyncPlan {
    let create: Vec<isize> = desired
        .iter()
        .copied()
        .filter(|h| !existing.contains(h))
        .collect();
    let destroy: Vec<isize> = existing
        .iter()
        .copied()
        .filter(|h| !desired.contains(h))
        .collect();
    SyncPlan { create, destroy }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct SyncPlan {
    /// 需要新建 strip 的任务栏句柄。
    pub create: Vec<isize>,
    /// 需要销毁 strip 的任务栏句柄。
    pub destroy: Vec<isize>,
}

// ---------------------------------------------------------------------------
// 纯函数单测：移植 blue 的 layout_tests（嵌入 placement / 坐标映射 / Metrics），
// 并新增多屏相关（负坐标副屏 placement、枚举为空降级、宽度预算）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_TEXT_PCT;

    /// 96 DPI（100% 缩放）：纯函数行为与 blue 改动前逐位一致。
    fn m96() -> Metrics {
        Metrics::new(96)
    }

    fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    /// 3840x2160 @150% 的底部任务栏：屏幕矩形 → 任务栏客户区矩形。
    #[test]
    fn screen_rect_maps_into_taskbar_client_coords() {
        // 任务栏横跨全宽；客户区原点在 (12, 2088)。
        let origin = POINT { x: 12, y: 2088 };
        let screen = rect(-12, 2088, 3852, 2160);
        let c = rect_offset_by(screen, origin);
        assert_eq!(c, rect(-24, 0, 3840, 72));
        // 托盘左沿同样换算。
        assert_eq!(3600 - origin.x, 3588);
    }

    #[test]
    fn negative_origin_secondary_monitor_left_of_primary() {
        let origin = POINT { x: -1920, y: 0 };
        let screen = rect(-1920, -60, 0, 0);
        let c = rect_offset_by(screen, origin);
        assert_eq!(c, rect(0, -60, 1920, 0));
    }

    /// 多屏核心：副屏在主屏左侧（负坐标）。屏幕坐标托盘左沿 -320 换算到该
    /// 副屏任务栏客户区后必须落在 [0, 1920]，placement 右沿贴住它且不出界。
    #[test]
    fn secondary_monitor_negative_coords_placement() {
        // 副屏任务栏屏幕矩形 x∈[-1920, 0)，托盘左沿屏幕坐标 -320。
        let tb_screen = rect(-1920, 1020, 0, 1080);
        let origin = POINT {
            x: -1920,
            y: 1040, // 客户区原点（任务栏上边框屏幕 y）
        };
        let tb_client = rect_offset_by(tb_screen, origin);
        let tray_left_client = -320 - origin.x; // 1600
        assert_eq!(tray_left_client, 1600);
        assert_eq!(tb_client, rect(0, -20, 1920, 40));
        let (x, _y, w, h) =
            strip_placement_in_taskbar(tb_client, Some(tray_left_client), false, 400, &m96());
        // 右沿恰好顶在托盘左边 GAP_TRAY 处，且整体在客户区内。
        assert_eq!(x + w + GAP_TRAY, 1600);
        assert!(x >= tb_client.left);
        assert_eq!(h, 60);
    }

    #[test]
    fn embedded_docks_right_next_to_tray() {
        let tb = rect(0, 0, 3840, 72);
        let (x, y, w, h) = strip_placement_in_taskbar(tb, Some(3600), false, 400, &m96());
        assert_eq!(y, 0);
        assert_eq!(h, 72);
        assert_eq!(w, 400);
        // 右沿必须恰好停在托盘左沿 GAP_TRAY 之前。
        assert_eq!(x + w + GAP_TRAY, 3600);
    }

    #[test]
    fn embedded_docks_left_with_inset() {
        let tb = rect(0, 0, 3840, 72);
        let (x, y, w, h) = strip_placement_in_taskbar(tb, Some(3600), true, 400, &m96());
        assert_eq!(x, LEFT_INSET);
        assert_eq!(y, 0);
        assert_eq!(w, 400);
        assert_eq!(h, 72);
    }

    #[test]
    fn embedded_falls_back_to_reserve_when_tray_not_found() {
        let tb = rect(0, 0, 3840, 72);
        let (_, _, w, _) = strip_placement_in_taskbar(tb, None, false, 400, &m96());
        let (xr, _, _, _) = strip_placement_in_taskbar(tb, None, false, w, &m96());
        assert_eq!(xr + w, tb.right - TASKBAR_RESERVE);
    }

    #[test]
    fn embedded_overwide_strip_stays_inside_taskbar() {
        let tb = rect(0, 0, 2000, 48);
        let tray_left = 1800;
        // 离谱的条宽放不进托盘前的空间，数学必须把它钳在任务栏内。
        let (x, _, w, h) = strip_placement_in_taskbar(tb, Some(tray_left), false, 5000, &m96());
        assert_eq!(h, 48);
        assert_eq!(w, tb.right - tb.left - 4);
        assert!(x >= tb.left + 2);
        assert!(x + w <= tb.right);
    }

    #[test]
    fn embedded_vertical_taskbar_clamps_width_and_height() {
        // 左侧竖排任务栏：窄而高。
        let tb = rect(0, 0, 80, 1080);
        let (x, y, w, h) = strip_placement_in_taskbar(tb, Some(80), true, 600, &m96());
        assert_eq!(y, 0);
        assert_eq!(h, 1080);
        assert_eq!(w, 76); // 80 - 4
        assert!(x >= tb.left + 2);
        assert!(x + w <= tb.right);
    }

    #[test]
    fn embedded_degenerate_height_gets_floor() {
        let tb = rect(0, 0, 1000, 10);
        let (_, _, _, h) = strip_placement_in_taskbar(tb, Some(900), false, 300, &m96());
        assert_eq!(h, MIN_STRIP_HEIGHT);
    }

    #[test]
    fn min_width_respected_on_tiny_taskbars() {
        let tb = rect(0, 0, 20, 1080);
        let (_, _, w, _) = strip_placement_in_taskbar(tb, None, false, 600, &m96());
        assert_eq!(w, MIN_STRIP_WIDTH);
    }

    /// 宽度预算：托盘找得到 → 量到托盘前；找不到 → TASKBAR_RESERVE 预留。
    /// 下限永远 ≥ MIN_STRIP_WIDTH（读失败不会算出 0 宽窗口）。
    #[test]
    fn width_budget_prefers_tray_gap_then_reserve() {
        let m = m96();
        let tb = rect(0, 0, 3840, 72);
        // LEFT_INSET(10) → 3600 - GAP_TRAY(6)
        assert_eq!(width_budget(tb, Some(3600), &m), 3600 - 6 - 10);
        // 托盘在任务栏左沿左边（异常值）→ 走预留
        assert_eq!(
            width_budget(tb, Some(0), &m),
            tb.right - TASKBAR_RESERVE - LEFT_INSET
        );
        assert_eq!(width_budget(tb, None, &m), tb.right - TASKBAR_RESERVE - 10);
        // 极窄任务栏 → 下限保底
        let tiny = rect(0, 0, 30, 40);
        assert_eq!(width_budget(tiny, None, &m), MIN_STRIP_WIDTH);
    }

    /// 多屏枚举为空（Explorer 未就绪 / 全部拔屏）→ 降级：销毁全部、一个不建。
    #[test]
    fn plan_empty_desired_destroys_all_and_creates_none() {
        let plan = plan_instance_sync(&[0x111, 0x222], &[]);
        assert_eq!(plan.destroy, vec![0x111, 0x222]);
        assert!(plan.create.is_empty());
        // 反向：从空集合起步（首启动）→ 只建不删。
        let plan = plan_instance_sync(&[], &[0x111, 0x222]);
        assert!(plan.destroy.is_empty());
        assert_eq!(plan.create, vec![0x111, 0x222]);
    }

    /// 插拔屏只动增量：新增副屏 → 补 strip；拔掉的 → 销毁；不变的不动。
    #[test]
    fn plan_only_touches_added_and_removed_taskbars() {
        let plan = plan_instance_sync(&[0xAAA, 0xBBB], &[0xAAA, 0xCCC]);
        assert_eq!(plan.create, vec![0xCCC]);
        assert_eq!(plan.destroy, vec![0xBBB]);
        // 完全一致 → 什么都不做
        let plan = plan_instance_sync(&[0xAAA], &[0xAAA]);
        assert_eq!(plan, plan_instance_sync(&[0xAAA], &[0xAAA]));
        assert!(plan.create.is_empty() && plan.destroy.is_empty());
    }

    /// DPI 缩放：150% 下所有几何量按 1.5 放大（真机 2560x1440@150%）。
    #[test]
    fn metrics_scale_all_geometry_at_150_percent() {
        let m = Metrics::new(144);
        assert_eq!(m.sc(6), 9); // GAP_TRAY / MARGIN_X
        assert_eq!(m.sc(8), 12); // DOT
        assert_eq!(m.sc(40), 60); // STRIP_HEIGHT
        assert_eq!(m.sc(MIN_STRIP_HEIGHT), 36);
        // 退化任务栏高度下限也按 DPI 放大
        let tb = rect(0, 0, 1000, 10);
        let (_, _, _, h) = strip_placement_in_taskbar(tb, Some(900), false, 300, &m);
        assert_eq!(h, 36);
        // 与托盘的间隙同样放大：1200 - 9
        let tb2 = rect(0, 0, 2000, 72);
        let (x, _, w, _) = strip_placement_in_taskbar(tb2, Some(1200), false, 300, &m);
        assert_eq!(x + w, 1191);
    }

    /// 字号档：DPI × 用户百分比（150% 下字体设计值 13 → 20px）。
    #[test]
    fn metrics_text_pct_scales_text_metrics() {
        let base = Metrics::with_text_pct(144, 100);
        assert_eq!(base.st(13), 20);
        assert_eq!(base.sc(13), 20);
        let small = Metrics::with_text_pct(144, 80);
        assert_eq!(small.st(13), 16);
        assert_eq!(small.st(8), 10); // 圆点跟着缩
        let big = Metrics::with_text_pct(144, 140);
        assert_eq!(big.st(13), 28);
        // 与字号无关的几何量不受档位影响
        assert_eq!(base.sc(40), big.sc(40));
        assert_eq!(base.sc(40), 60);
        // 越界档位被钳制
        assert_eq!(
            Metrics::with_text_pct(96, 10).st(10),
            Metrics::with_text_pct(96, MIN_TEXT_PCT).st(10)
        );
        assert_eq!(
            Metrics::with_text_pct(96, 9999).st(10),
            Metrics::with_text_pct(96, MAX_TEXT_PCT).st(10)
        );
    }

    #[test]
    fn metrics_treat_invalid_dpi_as_96() {
        assert_eq!(Metrics::new(0).sc(10), 10);
        assert_eq!(Metrics::new(-5).sc(10), 10);
        assert_eq!(Metrics::new(192).sc(10), 20); // 200%
    }

    /// 默认字号档必须是合法档位（菜单会把它当基准显示勾选）。
    #[test]
    fn default_text_pct_is_in_range() {
        assert!((MIN_TEXT_PCT..=MAX_TEXT_PCT).contains(&DEFAULT_TEXT_PCT));
        assert_eq!(Metrics::with_text_pct(96, DEFAULT_TEXT_PCT).st(13), 13);
    }

    /// detect_displays 输出的自洽性（有屏环境才校验，避免无头环境脆断）：
    /// 设备名 `\\.\DISPLAYn` 格式、主屏至多一块、分辨率 >0。
    #[test]
    fn detect_displays_output_is_consistent() {
        let ds = detect_displays();
        if !ds.is_empty() {
            assert!(
                ds.iter().all(|d| d.device.starts_with("\\\\.\\DISPLAY")),
                "设备名应为 \\\\.\\DISPLAYn 形式：{:?}",
                ds
            );
            assert!(
                ds.iter().filter(|d| d.primary).count() <= 1,
                "主屏至多一块"
            );
            assert!(ds.iter().all(|d| d.width > 0 && d.height > 0));
        }
    }
}
