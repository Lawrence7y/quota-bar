//! 弹出额度面板（点击 strip / 托盘左键打开）。
//!
//! 独立、不透明的 `WS_POPUP + WS_EX_TOPMOST` 窗口（blue
//! `BtBatteryBarPanel` 模式：owner 不是 child——嵌入后的条是 WS_CHILD 当不了
//! owner，面板必须是独立顶层弹窗），GDI 双缓冲绘制石墨清单。
//!
//! 定位：贴在**打开它的 strip** 所在 monitor 内（用该 strip 所在任务栏窗口的
//! `GetMonitorInfoW rcWork`，负坐标副屏天然正确），紧贴 strip 上沿（放不下改
//! 下沿），水平夹在工作区内。关闭：点外部（失焦）/ Esc。超长清单支持滚轮。

use crate::strip::App;
use crate::strip::geometry::{GAP_TRAY, Metrics, hi, hw, monitor_work_area};
use crate::strip::graphite::{self, FOOTER, HEADER};
use crate::strip::paint::{create_panel_font, delete_font};
use crate::theme;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FrameRgn, HGDIOBJ, IntersectClipRect, InvalidateRect, PAINTSTRUCT,
    ROP_CODE, RestoreDC, SaveDC, SelectObject, SetWindowRgn,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowRect,
    HMENU, IsWindow, IsWindowVisible, RegisterClassExW, SW_HIDE, SW_SHOWNA, SWP_NOACTIVATE,
    SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow, WINDOW_EX_STYLE, WM_ACTIVATE,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_PAINT,
    WM_RBUTTONUP, WNDCLASS_STYLES, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// 面板窗口状态（跨消息共享；字段存 isize 因为 HWND 不是 Send）。
pub struct PanelState {
    /// 0 = 尚未创建。
    pub hwnd: isize,
    /// 打开它的 strip（定位/取 DPI；0 = 裸任务栏兜底）。
    pub anchor_strip: isize,
    /// 它所在任务栏（monitor 工作区来源）。
    pub anchor_taskbar: isize,
    /// 当前字体的 (DPI, 字号档)，变化才重建。
    pub dpi: i32,
    pub text_pct: i32,
    pub name_font: i64,
    pub pill_font: i64,
    pub dim_font: i64,
    pub scroll_y: i32,
    pub menu_open: bool,
}

impl Default for PanelState {
    fn default() -> Self {
        Self {
            hwnd: 0,
            anchor_strip: 0,
            anchor_taskbar: 0,
            dpi: 0,
            text_pct: 0,
            name_font: 0,
            pill_font: 0,
            dim_font: 0,
            scroll_y: 0,
            menu_open: false,
        }
    }
}

impl App {
    pub(crate) fn panel_visible(&self) -> bool {
        let raw = self.panel.lock().unwrap().hwnd;
        raw != 0 && unsafe { IsWindowVisible(hw(raw)) }.as_bool()
    }

    pub(crate) fn hide_panel(&self) {
        let raw = self.panel.lock().unwrap().hwnd;
        if raw != 0 {
            unsafe {
                let _ = ShowWindow(hw(raw), SW_HIDE);
            }
        }
    }

    /// 在指定 strip（找不到就用裸任务栏）处显示面板。
    pub(crate) fn show_panel_at(&self, strip: HWND, taskbar: HWND) {
        let hwnd = self.ensure_panel();
        if hwnd.0.is_null() {
            return;
        }
        {
            let mut p = self.panel.lock().unwrap();
            p.anchor_strip = hi(strip);
            p.anchor_taskbar = hi(taskbar);
        }
        self.size_and_paint_panel();
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            let _ = SetForegroundWindow(hwnd);
        }
    }

    /// 面板窗口不存在则创建（类重复注册的错误忽略）。
    fn ensure_panel(&self) -> HWND {
        let existing = self.panel.lock().unwrap().hwnd;
        if existing != 0 && unsafe { IsWindow(hw(existing)) }.as_bool() {
            return hw(existing);
        }
        unsafe {
            let class_name = w!("QuotaBarPanel");
            let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: WNDCLASS_STYLES(0),
                lpfnWndProc: Some(panel_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: Default::default(),
                hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
                    HINSTANCE::default(),
                    windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
                )
                .unwrap_or_default(),
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name,
                hIconSm: Default::default(),
            };
            // ERROR_CLASS_ALREADY_EXISTS on second call is fine.
            let _ = RegisterClassExW(&wc);
            let ex = WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0);
            // 对 WS_POPUP 这个参数是 *owner* 而不是 parent——面板必须保持独立
            // 顶层弹窗，绝不能是任务栏/条的 child。条嵌入后是 WS_CHILD 当不了
            // owner，所以传 none（blue 原注释照搬）。
            let hwnd = CreateWindowExW(
                ex,
                class_name,
                w!("QuotaBarPanel"),
                WS_POPUP,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1,
                1,
                HWND::default(),
                HMENU::default(),
                hinst,
                None,
            );
            match hwnd {
                Ok(h) => {
                    self.panel.lock().unwrap().hwnd = hi(h);
                    h
                }
                Err(e) => {
                    crate::dblog::log(&format!("panel: create failed ({e})"));
                    HWND::default()
                }
            }
        }
    }

    /// 按 (DPI, 字号档) 重建面板字体；重建前先删旧字体（防 GDI 泄漏）。
    fn panel_ensure_fonts(&self, dpi: i32, text_pct: u32) -> (i64, i64, i64) {
        let mut p = self.panel.lock().unwrap();
        if p.dpi == dpi && p.text_pct == text_pct as i32 && p.name_font != 0 {
            return (p.name_font, p.pill_font, p.dim_font);
        }
        delete_font(&mut p.name_font);
        delete_font(&mut p.pill_font);
        delete_font(&mut p.dim_font);
        let m = Metrics::with_text_pct(dpi, text_pct);
        p.name_font = create_panel_font(-m.st(14), 500).0 as i64;
        p.pill_font = create_panel_font(-m.st(14), 600).0 as i64;
        p.dim_font = create_panel_font(-m.st(12), 400).0 as i64;
        p.dpi = dpi;
        p.text_pct = text_pct as i32;
        (p.name_font, p.pill_font, p.dim_font)
    }

    /// 重新度量 → 定位（strip 所在 monitor 的 rcWork 内夹紧）→ 失效重绘。
    pub(crate) fn size_and_paint_panel(&self) {
        // 1) 快照锚点（锁外做一切 Win32，避免 SetWindowPos 重入 wndproc 死锁）。
        let (raw, strip_raw, tb_raw) = {
            let p = self.panel.lock().unwrap();
            (p.hwnd, p.anchor_strip, p.anchor_taskbar)
        };
        if raw == 0 {
            return;
        }
        let hwnd = hw(raw);
        let content = self.content();
        let rows = graphite::rows(&content.rows, content.low_threshold);

        // 2) 面板 DPI = 锚点 strip（所在任务栏）的窗口 DPI；字体随 DPI×字号档重建。
        let anchor = if strip_raw != 0 && unsafe { IsWindow(hw(strip_raw)) }.as_bool() {
            hw(strip_raw)
        } else {
            hw(tb_raw)
        };
        let dpi = if anchor.0.is_null() {
            crate::strip::geometry::BASE_DPI
        } else {
            let d = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(anchor) } as i32;
            if d <= 0 {
                crate::strip::geometry::BASE_DPI
            } else {
                d
            }
        };
        self.panel_ensure_fonts(dpi, content.text_pct);
        let m = Metrics::with_text_pct(dpi, content.text_pct);

        // 3) The list uses fixed readable columns and grows vertically.
        let mut w = m.st(graphite::WIDTH);
        let mut rect = RECT::default();
        let got_rect = !anchor.0.is_null()
            && (if strip_raw != 0 {
                unsafe { GetWindowRect(hw(strip_raw), &mut rect) }.is_ok()
            } else {
                unsafe { GetWindowRect(hw(tb_raw), &mut rect) }.is_ok()
            });
        let wa = monitor_work_area(anchor)
            .or_else(|| monitor_work_area(hw(tb_raw)))
            .unwrap_or_else(|| {
                // 兜底：主屏工作区。
                let mut r = RECT::default();
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
                        windows::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
                        0,
                        Some(&mut r as *mut _ as *mut _),
                        windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                    );
                }
                r
            });
        // 面板不能高过/宽过工作区（否则顶部几行永远看不到 / 横向出屏）。
        let avail_w = (wa.right - wa.left - 8).max(64);
        if w > avail_w {
            w = avail_w;
        }
        let mut h =
            graphite::body_height(&rows, &m, graphite::narrow(w, &m)) + m.st(HEADER + FOOTER);
        let avail_h = (wa.bottom - wa.top - 16)
            .max(64)
            .min(m.st(graphite::MAX_HEIGHT));
        if h > avail_h {
            h = avail_h;
        }
        let gap = m.sc(GAP_TRAY);
        // A fallback strip is inside rcWork, unlike an Explorer taskbar. A long
        // list must fit on one side of its anchor instead of covering that strip.
        if got_rect {
            let above = (rect.top - gap - wa.top).max(0);
            let below = (wa.bottom - rect.bottom - gap).max(0);
            let room = above.max(below);
            if room >= m.st(HEADER + FOOTER + 40) {
                h = h.min(room);
            }
        }
        let mut y;
        let mut x;
        if got_rect {
            y = rect.top - h - gap;
            if y < wa.top {
                y = (rect.bottom + gap).min(wa.bottom - h - 4);
            }
            y = y.clamp(wa.top, (wa.bottom - h).max(wa.top));
            // 右对齐 strip 右沿（右停靠好看），再夹进工作区（左停靠/窄屏时兜底）。
            x = rect.right - w - 2;
            x = x.clamp(wa.left + 4, (wa.right - w - 4).max(wa.left + 4));
        } else {
            // 没有锚点：居中于工作区顶部。
            x = (wa.left + (wa.right - wa.left - w) / 2).max(wa.left + 4);
            y = wa.top + 8;
        }

        unsafe {
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                x,
                y,
                w,
                h,
                windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(
                    SWP_NOZORDER.0 | SWP_NOACTIVATE.0,
                ),
            );
            let region = graphite::region(w, h, m.sc(24));
            // SetWindowRgn owns the region on success.
            if SetWindowRgn(hwnd, region, true) == 0 {
                let _ = DeleteObject(HGDIOBJ(region.0));
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
    }

    /// 销毁面板（实例重建 / 退出时调用；独立顶层弹窗不销毁会永远悬在屏幕上）。
    pub(crate) fn destroy_panel(&self) {
        let raw = self.panel.lock().unwrap().hwnd;
        if raw != 0 {
            unsafe {
                let _ = DestroyWindow(hw(raw));
            }
        }
        let mut p = self.panel.lock().unwrap();
        p.hwnd = 0;
        p.anchor_strip = 0;
        p.anchor_taskbar = 0;
        delete_font(&mut p.name_font);
        delete_font(&mut p.pill_font);
        delete_font(&mut p.dim_font);
        p.dpi = 0;
        p.scroll_y = 0;
        p.menu_open = false;
    }

    // ------------------------------------------------------------------ paint
    fn paint_panel(&self, hwnd: HWND) {
        let (fonts, dpi, scroll) = {
            let p = self.panel.lock().unwrap();
            ((p.name_font, p.pill_font, p.dim_font), p.dpi, p.scroll_y)
        };
        if fonts.0 == 0 {
            return;
        }
        let content = self.content();
        let m = Metrics::with_text_pct(dpi, content.text_pct);
        let rows = graphite::rows(&content.rows, content.low_threshold);
        let theme = theme::detect();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if rc.right <= 0 || rc.bottom <= 0 {
                let _ = EndPaint(hwnd, &ps);
                return;
            }
            let md = CreateCompatibleDC(dc);
            let bmp = CreateCompatibleBitmap(dc, rc.right, rc.bottom);
            let old = SelectObject(md, HGDIOBJ(bmp.0));
            graphite::fill(md, rc, theme.panel_bg);
            let border = CreateSolidBrush(COLORREF(theme.panel_border));
            let region = graphite::region(rc.right, rc.bottom, m.sc(24));
            let _ = FrameRgn(md, region, border, 1, 1);
            let _ = DeleteObject(HGDIOBJ(region.0));
            let _ = DeleteObject(HGDIOBJ(border.0));
            let pad = m.st(graphite::PAD);
            let top = m.st(HEADER);
            let bottom = rc.bottom - m.st(FOOTER);
            graphite::text(
                md,
                fonts.0,
                graphite::rect(pad, m.st(12), m.st(160), m.st(24)),
                "额度",
                theme.fg,
                false,
            );
            graphite::text(
                md,
                fonts.0,
                graphite::rect(rc.right - m.st(66), m.st(12), m.st(42), m.st(24)),
                "刷新",
                theme.fg,
                true,
            );
            graphite::fill(
                md,
                graphite::rect(pad, m.st(46), rc.right - pad * 2, 1),
                theme.panel_border,
            );
            graphite::text(
                md,
                fonts.2,
                graphite::rect(pad, m.st(50), m.st(150), m.st(20)),
                "供应商 / 账号",
                graphite::DIM,
                false,
            );
            graphite::text(
                md,
                fonts.2,
                graphite::rect(pad, m.st(50), rc.right - pad * 2, m.st(20)),
                "已用额度",
                graphite::DIM,
                true,
            );
            let is_narrow = graphite::narrow(rc.right, &m);
            let body = graphite::body_height(&rows, &m, is_narrow);
            let scroll = scroll.clamp(0, (body - (bottom - top)).max(0));
            self.panel.lock().unwrap().scroll_y = scroll;
            let saved = SaveDC(md);
            IntersectClipRect(md, 1, top, rc.right - 1, bottom);
            let mut y = top - scroll;
            for row in &rows {
                graphite::paint_row(md, row, &m, rc.right, y, fonts, &theme);
                y += graphite::row_height(row, &m, is_narrow);
            }
            if rows.is_empty() {
                graphite::text(
                    md,
                    fonts.2,
                    graphite::rect(pad, top, rc.right - pad * 2, m.st(72)),
                    "没有启用的额度来源",
                    graphite::DIM,
                    false,
                );
            }
            if body > bottom - top {
                let track_h = (bottom - top).max(1);
                let thumb_h = (track_h * track_h / body).max(m.sc(20)).min(track_h);
                let thumb_y = top + (track_h - thumb_h) * scroll / (body - track_h).max(1);
                graphite::fill(
                    md,
                    graphite::rect(rc.right - m.sc(6), thumb_y, m.sc(2), thumb_h),
                    graphite::DIM,
                );
            }
            let _ = RestoreDC(md, saved);
            let footer_y = rc.bottom - m.st(FOOTER) + m.st(8);
            super::paint::round_rect_filled(
                md,
                m.st(14),
                footer_y,
                rc.right - m.st(28),
                m.st(36),
                theme.panel_row_alt,
                theme.panel_border,
                m.sc(28),
            );
            let cfg = self.settings.read().unwrap().clone();
            let labels = [
                format!(
                    "字号 {} ▾",
                    match cfg.text_pct() {
                        80 => "小",
                        100 => "标准",
                        120 => "大",
                        _ => "特大",
                    }
                ),
                format!("刷新 {}秒 ▾", cfg.poll_interval().as_secs()),
                "供应商 ›".into(),
            ];
            let cell = (rc.right - m.st(40)) / 3;
            for (i, label) in labels.iter().enumerate() {
                graphite::text(
                    md,
                    fonts.2,
                    graphite::rect(m.st(20) + i as i32 * cell, footer_y, cell, m.st(36)),
                    label,
                    theme.fg,
                    false,
                );
            }
            let _ = BitBlt(
                dc,
                0,
                0,
                rc.right,
                rc.bottom,
                md,
                0,
                0,
                ROP_CODE(0x00CC0020),
            );
            SelectObject(md, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(md);
            let _ = EndPaint(hwnd, &ps);
        }
    }

    // ---------------------------------------------------------------- messages
    /// 面板窗口消息：点面板 / 点外部（失焦）/ Esc → 关闭。
    pub(crate) fn handle_panel_message(
        &self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_PAINT => {
                self.paint_panel(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) as u16) as i16 as i32;
                let mut p = self.panel.lock().unwrap();
                let m = Metrics::with_text_pct(p.dpi, p.text_pct as u32);
                p.scroll_y = (p.scroll_y - delta * m.st(60) / 120).max(0);
                drop(p);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 as u16) as i16 as i32;
                let y = ((lparam.0 >> 16) as u16) as i16 as i32;
                let p = self.panel.lock().unwrap();
                let m = Metrics::with_text_pct(p.dpi, p.text_pct as u32);
                drop(p);
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut rc);
                }
                if y < m.st(46) && x > rc.right - m.st(80) {
                    self.poll.kick();
                } else if y >= rc.bottom - m.st(FOOTER) {
                    let index = ((x - m.st(20)).max(0) / ((rc.right - m.st(40)) / 3).max(1)).min(2);
                    if index == 2 {
                        self.hide_panel();
                        self.show_menu(self.menu_owner());
                    } else {
                        self.panel.lock().unwrap().menu_open = true;
                        let cfg = self.settings.read().unwrap().clone();
                        let selected = super::menu::show_preference(hwnd, &cfg, index == 0);
                        self.panel.lock().unwrap().menu_open = false;
                        if let Some(cmd) = selected {
                            self.on_menu_cmd(cmd, hwnd);
                        }
                        unsafe {
                            let _ = SetForegroundWindow(hwnd);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                self.hide_panel();
                self.show_menu(self.menu_owner());
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                if self.panel.lock().unwrap().menu_open {
                    return LRESULT(0);
                }
                self.hide_panel();
                LRESULT(0)
            }
            WM_ACTIVATE => {
                // 点面板以外的地方会把本窗口 deactivate（WA_INACTIVE=0）→ 关。
                let state = (wparam.0 & 0xFFFF) as u32;
                if state == 0 && !self.panel.lock().unwrap().menu_open {
                    self.hide_panel();
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wparam.0 as u16 == windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 {
                    self.hide_panel();
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let mut p = self.panel.lock().unwrap();
                if p.hwnd == hi(hwnd) {
                    p.hwnd = 0;
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }
}

/// 面板窗口过程：点面板 / 失焦 / Esc 关闭（逻辑在 `handle_panel_message`）。
pub(crate) unsafe extern "system" fn panel_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(app) = super::APP.get()
        && app.panel.lock().unwrap().hwnd == hi(hwnd)
    {
        return app.handle_panel_message(hwnd, msg, wparam, lparam);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
