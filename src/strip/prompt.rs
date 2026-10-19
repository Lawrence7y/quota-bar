//! 模态凭据输入框（「添加供应商」用）：纯 Win32 手搓对话框 —— WS_POPUP 顶层窗 +
//! STATIC 标签 + EDIT 输入 + 确定/取消按钮，**嵌套消息循环**做到模态（与
//! `TrackPopupMenu` 同款手法：菜单命令栈内直接开，返回 `Option<String>`）。
//!
//! 交互：Enter = 确定、Esc = 取消、Tab / Shift+Tab 在三个控件间循环（键拦截在
//! `DispatchMessage` **之前**，编辑框收不到这些键；其余按键正常派发给 EDIT）。
//! 确定且非空 → `Some(原文)`；取消 / 关闭 / 已有框打开 → `None`（本层不回显
//! 已存凭据——防肩窥；空串由调用方按"取消"处置）。
//!
//! 模态期间主消息循环的定时器/广播照常被本循环派发（条继续自愈重绘）；进程内
//! 同时只允许一个输入框（`BUSY` 原子闩 + RAII 清闩），条/托盘两个入口并发触发
//! 时后者直接放弃。窗口创建在**调用线程**上、`GetMessage` 阻塞到销毁——只允许在
//! UI 线程（菜单命令栈）内调用；测试从独立线程调同一入口（该线程自带消息循环），
//! 用 `FindWindowW` + 投递 `WM_COMMAND`/`WM_KEYDOWN` 驱动确定/取消/Esc 三条路径。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DEFAULT_GUI_FONT, GetMonitorInfoW, GetStockObject, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetAsyncKeyState, GetFocus, SetFocus, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetWindowTextLengthW, GetWindowTextW, GetWindowLongPtrW, HMENU,
    IsChild, IsWindow, LoadCursorW, MSG, PostQuitMessage, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, ShowWindow, SPI_GETWORKAREA, SystemParametersInfoW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TranslateMessage, BS_DEFPUSHBUTTON, CW_USEDEFAULT,
    ES_AUTOHSCROLL, ES_LEFT, GWLP_USERDATA, IDC_ARROW, IDCANCEL, IDOK, SW_SHOW, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_KEYDOWN, WM_SETFONT, WM_SYSKEYDOWN, WNDCLASS_STYLES, WNDCLASSEXW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::strip::geometry::monitor_work_area;

/// 窗口类名（测试用它 `FindWindowW` 定位输入框）。
const CLASS_NAME: &str = "QuotaBarPrompt";

/// 96 DPI 设计尺寸（client 宽高；`scale` 按 owner 的 Per-Monitor DPI 换算）。
const DESIGN_W: i32 = 440;
const DESIGN_H: i32 = 136;

/// 进程内模态闩：已有输入框打开时后来者直接放弃（条/托盘双入口防重入）。
static BUSY: AtomicBool = AtomicBool::new(false);

/// &str → UTF16（含结尾0）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 输入框的跨消息状态（`GWLP_USERDATA` 指回堆上的本结构）。
struct State {
    /// 对话框自身（确定路径据此销毁，免去 IsChild 反查）。
    dlg: HWND,
    edit: HWND,
    ok: HWND,
    cancel: HWND,
    /// 确定 = Some(原文，未 trim)；取消 / 关闭 = None。
    result: Option<String>,
}

/// 开一个模态输入框。`owner` 用于 DPI 与居中（可为 null —— 测试路径）。
///
/// 返回 `Some(输入原文)`（确定，可能为空串——空由调用方按取消处理）、
/// `None`（取消 / 关闭 / 进程内已有框在开 / 窗口创建失败）。
pub fn run(owner: HWND, title: &str, label: &str, hint: &str) -> Option<String> {
    if BUSY.swap(true, Ordering::AcqRel) {
        return None; // 已有输入框：并发入口静默放弃（模态语义）
    }
    let _busy = BusyGuard;
    unsafe { run_inner(owner, title, label, hint) }
}

/// RAII 清闩：任何退出路径（含 panic）都不把 `BUSY` 卡死。
struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

unsafe fn run_inner(owner: HWND, title: &str, label: &str, hint: &str) -> Option<String> {
    // edition 2024：unsafe fn 体内仍需显式 unsafe 块（项目其余 unsafe fn 同风格）。
    unsafe {
    let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).ok()?.0);

    // Per-Monitor DPI：owner 在哪块屏，框就按那块屏缩放（null/失败 → 96）。
    let dpi = if owner.0.is_null() {
        96i32
    } else {
        let d = GetDpiForWindow(owner) as i32;
        if d > 0 { d } else { 96 }
    };
    let scale = |v: i32| v * dpi / 96;
    let (cw, ch) = (scale(DESIGN_W), scale(DESIGN_H));

    // 对话框框架：caption + sysmenu（无粗边框不可缩放）；bmenu = FALSE。
    let style = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0);
    let ex = WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0); // 不占任务栏 / Alt-Tab
    let mut outer = RECT { left: 0, top: 0, right: cw, bottom: ch };
    let _ = AdjustWindowRectEx(&mut outer, style, BOOL(0), ex);
    let (ow, oh) = (outer.right - outer.left, outer.bottom - outer.top);

    ensure_class(hinst);

    let class16 = wide(CLASS_NAME);
    let title16 = wide(title);
    let (x, y) = center_xy(owner, ow, oh);
    // 对 WS_POPUP，hwndParent 是 *owner* —— 条是 WS_CHILD 当不了 owner（panel 同款
    // 理由），传 null 独立顶层，靠 SetForegroundWindow 置前。
    let dlg = CreateWindowExW(
        ex,
        PCWSTR(class16.as_ptr()),
        PCWSTR(title16.as_ptr()),
        style,
        x,
        y,
        ow,
        oh,
        HWND::default(),
        HMENU::default(),
        hinst,
        None,
    )
    .ok()?;

    // 子控件创建失败（系统类缺失，理论不发生）→ 销毁父窗收尾，不泄漏。
    let (edit, ok, cancel) = match create_children(dlg, hinst, cw, ch, scale, label, hint) {
        Some(v) => v,
        None => {
            let _ = DestroyWindow(dlg);
            return None;
        }
    };

    // 状态挂窗口上；窗口销毁前 Box 一直由本栈帧持有。
    let mut state = Box::new(State {
        dlg,
        edit,
        ok,
        cancel,
        result: None,
    });
    let ptr: *mut State = &mut *state;
    SetWindowLongPtrW(dlg, GWLP_USERDATA, ptr as isize);

    // 真模态：禁用 owner（条 / helper），对话框期间点它不会再弹一层菜单
    //（`BUSY` 闩兜住托盘路径——helper 禁用不影响已投递的托盘消息）。
    // EnableWindow 返回"之前是否启用" → 恢复时只恢复本来启用的，且防 owner
    // 在对话框期间被 Explorer 重建销毁（IsWindow 兜底）。
    let owner_was_enabled =
        !owner.0.is_null() && IsWindow(owner).as_bool() && EnableWindow(owner, BOOL(0)).as_bool();

    let _ = ShowWindow(dlg, SW_SHOW);
    let _ = SetForegroundWindow(dlg);
    let _ = SetFocus(edit);

    // ---- 模态循环：Enter/Esc/Tab 在 Dispatch 前拦截；其余消息照常派发（条的
    //      1s 心跳等在模态期间继续工作）。WM_QUIT 透传外层，不能吞掉退出。----
    let mut msg = MSG::default();
    while IsWindow(dlg).as_bool() {
        let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
        if r.0 == 0 {
            PostQuitMessage(msg.wParam.0 as i32);
            break;
        }
        if r.0 == -1 {
            break;
        }
        let in_dlg = msg.hwnd == dlg || IsChild(dlg, msg.hwnd).as_bool();
        if in_dlg && (msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN) {
            let vk = msg.wParam.0 as u16;
            if vk == VK_RETURN.0 {
                accept(ptr);
                continue;
            }
            if vk == VK_ESCAPE.0 {
                // 取消：result 保持 None
                let _ = DestroyWindow(dlg);
                continue;
            }
            if vk == VK_TAB.0 {
                cycle_focus(&*ptr, GetAsyncKeyState(VK_SHIFT.0 as i32) < 0);
                continue;
            }
        }
        let _ = TranslateMessage(&msg);
        let _ = DispatchMessageW(&msg);
    }
    if IsWindow(dlg).as_bool() {
        let _ = DestroyWindow(dlg); // WM_QUIT 中断兜底
    }
    if owner_was_enabled && IsWindow(owner).as_bool() {
        let _ = EnableWindow(owner, BOOL(1));
    }
    state.result
    }
}

/// 创建三个子控件（标签 / 提示 / 编辑框 / 确定 / 取消）并统一设 GUI 字体。
/// 返回 (edit, ok, cancel)；任一失败 → None（调用方销毁父窗）。
unsafe fn create_children(
    dlg: HWND,
    hinst: HINSTANCE,
    cw: i32,
    ch: i32,
    scale: impl Fn(i32) -> i32,
    label: &str,
    hint: &str,
) -> Option<(HWND, HWND, HWND)> {
    unsafe {
    let font = GetStockObject(DEFAULT_GUI_FONT);
    let plain = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);
    let label16 = wide(label);
    let hint16 = wide(hint);

    let lab = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(label16.as_ptr()),
        plain,
        scale(12),
        scale(10),
        cw - scale(24),
        scale(18),
        dlg,
        HMENU::default(),
        hinst,
        None,
    )
    .ok()?;
    let hin = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(hint16.as_ptr()),
        plain,
        scale(12),
        scale(32),
        cw - scale(24),
        scale(28),
        dlg,
        HMENU::default(),
        hinst,
        None,
    )
    .ok()?;
    let edit = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        w!("EDIT"),
        PCWSTR::null(),
        WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | (ES_LEFT | ES_AUTOHSCROLL) as u32,
        ),
        scale(12),
        scale(66),
        cw - scale(24),
        scale(24),
        dlg,
        HMENU::default(),
        hinst,
        None,
    )
    .ok()?;
    let btn_w = scale(80);
    let btn_h = scale(26);
    let btn_y = ch - scale(12) - btn_h;
    // 控件 ID 走 HMENU（WM_COMMAND 的 LOWORD 分流 IDOK/IDCANCEL）。
    let ok = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        w!("确定"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32),
        cw - scale(12) - btn_w - scale(8) - btn_w,
        btn_y,
        btn_w,
        btn_h,
        dlg,
        HMENU(IDOK.0 as isize as *mut core::ffi::c_void),
        hinst,
        None,
    )
    .ok()?;
    let cancel = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        w!("取消"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0),
        cw - scale(12) - btn_w,
        btn_y,
        btn_w,
        btn_h,
        dlg,
        HMENU(IDCANCEL.0 as isize as *mut core::ffi::c_void),
        hinst,
        None,
    )
    .ok()?;
    for c in [lab, hin, edit, ok, cancel] {
        SendMessageW(c, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
    Some((edit, ok, cancel))
    }
}

/// 确定：读编辑框文本写入状态，销毁窗口（空串也返回 Some——空由调用方处置）。
unsafe fn accept(ptr: *mut State) {
    unsafe {
    if ptr.is_null() {
        return;
    }
    let st = &mut *ptr;
    let n = GetWindowTextLengthW(st.edit);
    let mut buf = vec![0u16; n.max(0) as usize + 1];
    let got = GetWindowTextW(st.edit, &mut buf);
    st.result = Some(String::from_utf16_lossy(&buf[..got.max(0) as usize]));
    let _ = DestroyWindow(st.dlg);
    }
}

/// Tab / Shift+Tab：在 编辑框 → 确定 → 取消 之间循环焦点。
unsafe fn cycle_focus(st: &State, back: bool) {
    unsafe {
    let order = [st.edit, st.ok, st.cancel];
    let cur = GetFocus();
    let idx = order.iter().position(|h| *h == cur).unwrap_or(0);
    let next = if back {
        (idx + order.len() - 1) % order.len()
    } else {
        (idx + 1) % order.len()
    };
    let _ = SetFocus(order[next]);
    }
}

/// 注册窗口类（进程一次）；白画刷做客户区背景（类不注销，1 个画刷随进程——
/// 类画刷的标准生命周期）。注册失败由后续 `CreateWindowExW` 的 Err 暴露。
fn ensure_class(hinst: HINSTANCE) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        let class16 = wide(CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: Default::default(),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: CreateSolidBrush(COLORREF(0x00_FF_FF_FF)), // 白底
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class16.as_ptr()),
            hIconSm: Default::default(),
        };
        let _ = RegisterClassExW(&wc); // 重复注册失败无害（OnceLock 只跑一次）
    });
}

/// 居中坐标（屏幕系）：优先 owner 所在屏工作区 → 光标所在屏 → 主工作区。
fn center_xy(owner: HWND, w: i32, h: i32) -> (i32, i32) {
    let wa = monitor_work_area(owner).or_else(|| unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p); // 失败保持 (0,0) → 主屏，仍可用
        let mon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            return Some(mi.rcWork);
        }
        let mut r = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut r as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if r.right > r.left {
            Some(r)
        } else {
            None
        }
    });
    match wa {
        Some(r) => (
            r.left + (r.right - r.left - w) / 2,
            r.top + (r.bottom - r.top - h) / 2,
        ),
        None => (CW_USEDEFAULT, CW_USEDEFAULT), // 极端兜底（理论到不了）
    }
}

/// 对话框窗口过程：`WM_COMMAND`（按钮 → LOWORD 分流 IDOK/IDCANCEL）、
/// `WM_CLOSE`（× 按钮 = 取消），其余交 `DefWindowProcW`。
/// 状态经 `GWLP_USERDATA` 取回（创建早期的 null userdata 容错）。
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let _ = lparam;
        let st = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        match msg {
            WM_COMMAND => {
                if !st.is_null() {
                    let id = wparam.0 & 0xFFFF;
                    if id == IDOK.0 as usize {
                        accept(st);
                        return LRESULT(0);
                    }
                    if id == IDCANCEL.0 as usize {
                        (*st).result = None;
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if !st.is_null() {
                    (*st).result = None;
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, FindWindowW, PostMessageW, SetWindowTextW,
    };

    /// GUI 测试串行锁：cargo test 多线程并行时，`BUSY` 闩会让第二个 `run`
    /// 直接返回 None —— 所有开框测试必须持锁串行。
    static GUI_LOCK: Mutex<()> = Mutex::new(());
    fn gui_lock() -> std::sync::MutexGuard<'static, ()> {
        GUI_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 找指定标题的输入框（窗口在被测线程上创建 → 轮询）。
    fn wait_window(title: &str) -> HWND {
        let class = wide(CLASS_NAME);
        let t = wide(title);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(h) = unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(t.as_ptr())) } {
                if !h.0.is_null() {
                    return h;
                }
            }
            assert!(Instant::now() < deadline, "输入框 `{title}` 5s 内未出现");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 对话框下的 EDIT 子窗口（子控件晚于父窗创建 → 轮询）。
    fn wait_edit(dlg: HWND) -> HWND {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(h) =
                unsafe { FindWindowExW(dlg, HWND::default(), w!("EDIT"), PCWSTR::null()) }
            {
                if !h.0.is_null() {
                    return h;
                }
            }
            assert!(Instant::now() < deadline, "EDIT 子窗口 5s 内未出现");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 独立线程开框、本线程驱动：填文本 → 投 IDOK（= 点"确定"）→ 断言带回原文。
    #[test]
    fn prompt_ok_returns_typed_text() {
        let _g = gui_lock();
        let (tx, rx) = mpsc::channel();
        let t = "prompt-test-ok";
        let h = std::thread::spawn(move || {
            let r = run(HWND::default(), t, "测试标签", "测试提示：粘贴 Cookie 或 API Key");
            let _ = tx.send(r);
        });
        let dlg = wait_window(t);
        let edit = wait_edit(dlg);
        let text = "sk-test-123";
        let w = wide(text);
        unsafe {
            let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));
            let _ = PostMessageW(dlg, WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0));
        }
        let r = rx.recv_timeout(Duration::from_secs(10)).expect("确定路径未返回");
        assert_eq!(r.as_deref(), Some(text), "确定应带回编辑框原文");
        h.join().expect("确定路径线程 panic");
    }

    /// 投 IDCANCEL（= 点"取消"）→ None。
    #[test]
    fn prompt_cancel_button_returns_none() {
        let _g = gui_lock();
        let (tx, rx) = mpsc::channel();
        let t = "prompt-test-cancel";
        let h = std::thread::spawn(move || {
            let r = run(HWND::default(), t, "标签", "提示");
            let _ = tx.send(r);
        });
        let dlg = wait_window(t);
        let _ = wait_edit(dlg); // 子控件齐了再投，避免与创建竞态
        unsafe {
            let _ = PostMessageW(dlg, WM_COMMAND, WPARAM(IDCANCEL.0 as usize), LPARAM(0));
        }
        let r = rx.recv_timeout(Duration::from_secs(10)).expect("取消未返回");
        assert_eq!(r, None);
        h.join().expect("取消线程 panic");
    }

    /// Esc 键路径（拦截层，不走 WM_COMMAND）→ None。
    #[test]
    fn prompt_escape_key_returns_none() {
        let _g = gui_lock();
        let (tx, rx) = mpsc::channel();
        let t = "prompt-test-esc";
        let h = std::thread::spawn(move || {
            let r = run(HWND::default(), t, "标签", "提示");
            let _ = tx.send(r);
        });
        let dlg = wait_window(t);
        let _ = wait_edit(dlg);
        // 直接向对话框投 VK_ESCAPE（拦截层认 hwnd ∈ 对话框树）
        unsafe {
            let _ = PostMessageW(dlg, WM_KEYDOWN, WPARAM(VK_ESCAPE.0 as usize), LPARAM(1));
        }
        let r = rx.recv_timeout(Duration::from_secs(10)).expect("Esc 未返回");
        assert_eq!(r, None, "Esc 应返回 None（取消）");
        h.join().expect("Esc 线程 panic");
    }
}
