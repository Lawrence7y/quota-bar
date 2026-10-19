//! 任务栏额度条（strip + 面板 + 托盘 + 菜单 + 多屏）——完整实现。
//!
//! Win32 管线整体移植自 `D:\Project\blue\bt-battery-bar\src\app.rs`（嵌入 /
//! heal / 重建 / helper 广播窗 / 托盘 / 菜单 / GDI 双缓冲 / 定时器），并在其上
//! 做了三件本项目特有的事：
//! 1. **内容层**换成额度语义：`ProviderSnapshot` → 紧凑段（色点 + 短名 + 三窗口
//!    + 余额 / 错误原因全部平铺），放不下尾部折叠 `+n`，永不越界遮挡托盘；
//! 2. **弹出面板**（独立 WS_POPUP+TOPMOST，GDI 表格，见 `panel.rs`）；
//! 3. **多屏**：每个任务栏窗口一个 strip 实例（`Shell_TrayWnd` +
//!    `Shell_SecondaryTrayWnd`），heal/placement/重绘按实例各跑一份，
//!    一个 1s 定时器遍历全部实例。
//!
//! ## 对外契约（main / poll / oauth 依赖，必须保留）
//!
//! ```ignore
//! pub fn run(
//!     settings: Arc<RwLock<Settings>>,   // 配置；菜单改动必须 save() 后 kick
//!     store: Arc<SnapshotStore>,         // 额度数据；take_dirty() 决定重绘，
//!                                         // take_alert() 取待弹气泡
//!     poll: PollHandle,                  // kick() 立即刷新
//! ) -> i32                               // 消息循环退出码（正常不返回）
//! ```
//!
//! 附加约定（本实现的落点）：
//! - 1s 定时器（挂在 helper 上）里检查 `store.take_alert()` → 托盘气泡（NIF_INFO/NIIF_WARNING）。
//! - 菜单"登录/登出"放后台线程，结果 `PostMessageW` 回主线程 → save/kick/dirty/气泡。
//! - 设置变更统一走 `App::commit`：`settings.write()` → `save()`（写盘）→
//!   `poll.kick()` → `store.mark_dirty()`（poll 每轮从磁盘重载，只改内存不写盘会被覆盖）。

mod geometry;
mod graphite;
mod docking;
mod dragging;
mod layout;
mod logos;
mod menu;
mod panel;
mod paint;
mod prompt;
mod tray;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint,
    FillRect, GetDC, HDC, HGDIOBJ, InvalidateRect, MonitorFromPoint, MonitorFromWindow,
    PAINTSTRUCT, ROP_CODE, ReleaseDC, SelectObject, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW, GetParent, GetWindowLongPtrW,
    GWL_EXSTYLE, GWL_STYLE, HICON, HMENU, IDC_ARROW, IsWindow, IsWindowVisible, KillTimer,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_COLORKEY, LoadCursorW, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SET_WINDOW_POS_FLAGS,
    SetLayeredWindowAttributes, SetParent, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SW_HIDE, SW_SHOWNA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    TranslateMessage, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION,
    WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_CAPTURECHANGED, WM_CANCELMODE, WM_PAINT, WM_QUERYENDSESSION, WM_RBUTTONUP,
    WM_SETTINGCHANGE, WM_TIMER, WINDOW_EX_STYLE, WNDCLASS_STYLES, WNDCLASSEXW, WS_CHILD,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::config::{Settings, StripStyle, should_show_on};
use crate::model::{CredentialKind, ProviderId, ProviderSnapshot};
use crate::poll::{PollHandle, SnapshotStore};
use crate::theme;
use geometry::{
    BASE_DPI, CHIP_BAR_H, CHIP_BAR_W, CHIP_COL_GAP, CHIP_GAP, CHIP_PAD_X, CHIP_ROW_H,
    CHIP_TILE_GAP, DOT, DOT_GAP, GAP_TRAY, GRACE_SECS, LEFT_INSET, LOGO_PAD, MARGIN_X,
    MIN_STRIP_HEIGHT, MIN_STRIP_WIDTH, NAME_FONT_PT, OVERFLOW_PILL_MIN, PILL_FONT_PT, PILL_PAD,
    SEP_TEXT, STRIP_HEIGHT, TILE, Metrics, enumerate_taskbars, find_primary_taskbar, hi, hw,
    monitor_work_area, strip_placement_in_taskbar, taskbar_client_geometry, width_budget,
};
use layout::{SegmentText, StripLayout};
use menu::MenuCmd;
use paint::{draw_status_dot, text_size};

// ---------------------------------------------------------------------------
/// 自定义消息（全部 posted 而非 sent——GetMessage 才能可靠取到，blue 同款理由）。
/// 重建 strip：WM_DESTROY（Explorer 带走子窗口）时向仍活着的 helper 投递。
const MSG_RECREATE_STRIP: u32 = 0x8000 + 22;
/// 登录/登出后台线程完成 → 回主线程执行 save/kick/dirty/气泡。
const MSG_AUTH_DONE: u32 = 0x8000 + 23;
/// helper 上的 1s 心跳：heal 全部实例 + 数据重绘 + 托盘气泡。
const TICK_TIMER: usize = 1;
/// 重建失败后的 500ms 重试定时器（挂 helper，绝不 sleep UI 线程）。
const RECREATE_TIMER: usize = 2;
const RECREATE_RETRY_MS: u32 = 500;

/// 全局应用句柄（wndproc 按 hwnd 分流到各处理器；blue `APP` 同款 OnceLock）。
pub(crate) static APP: OnceLock<Arc<App>> = OnceLock::new();

/// 登录/登出后台线程的结果（`MSG_AUTH_DONE` 消费；带 provider 以便把新登录的
/// 账号并入该 provider 的显示白名单）。
struct AuthOutcome {
    ok: bool,
    login: bool,
    provider: ProviderId,
    title: String,
    text: String,
}

// ---------------------------------------------------------------------------
/// 单个任务栏窗口对应的 strip 实例（多屏核心：一块屏一份）。
///
/// HWND 含裸指针不是 `Send`，所以句柄一律存 `isize`，用时 `hw()` 转回。
struct StripInstance {
    provider: Option<ProviderId>,
    position: Option<crate::config::ProviderPosition>,
    lane: Option<(i32, i32)>,
    drag_preview: bool,
    drag_place: Option<(i32, i32, i32, i32)>,
    /// 它归属的任务栏（Shell_TrayWnd / Shell_SecondaryTrayWnd）。
    taskbar: isize,
    /// strip 窗口；0 = 窗口还没建成（创建失败，等 500ms 重试）。
    hwnd: isize,
    /// 该 strip 所在屏的 DPI（PMv2：每块屏一个）。
    dpi: i32,
    /// 是否已完成 WS_CHILD + SetParent。
    embedded: bool,
    /// 上次 SetWindowPos 的结果（空闲重锚判断）。
    last_place: (i32, i32, i32, i32),
    /// 内容布局（段 + 折叠数 + 宽度）。
    layout: StripLayout,
    layout_valid: bool,
    /// 上次布局时的条内容高（chips 盒内 2/3 行自适应的依据；变了要重建布局）。
    layout_height: i32,
    /// 按 (DPI × 字号档) 创建的字体；实例销毁/字号变化时 DeleteObject 重建。
    name_font: i64,
    pill_font: i64,
    /// 上次 SetParent 失败错误码（0 = 成功）；抑制每秒重复写日志。
    embed_fail_logged: u32,
}

impl StripInstance {
    fn new(taskbar: isize) -> Self {
        Self {
            taskbar,
            provider: None, position: None, lane: None, drag_preview: false, drag_place: None,
            hwnd: 0,
            dpi: BASE_DPI,
            embedded: false,
            last_place: (0, 0, 0, 0),
            layout: StripLayout::default(),
            layout_valid: false,
            layout_height: 0,
            name_font: 0,
            pill_font: 0,
            embed_fail_logged: 0,
        }
    }
}

/// 一轮布局/可见性判断需要的全部设置与数据快照。
/// 锁序纪律：先 settings 再 store；**绝不在持有 instances 锁时调用 `content()`**。
#[derive(Clone)]
pub(crate) struct Content {
    pub rows: Vec<ProviderSnapshot>,
    pub low_threshold: u8,
    pub text_pct: u32,
    pub dock_left: bool,
    pub strip_visible: bool,
    #[allow(dead_code)]
    pub show_all_monitors: bool,
    /// 条视觉样式（chips / text），决定 `build_segments` 走哪个分支。
    pub style: StripStyle,
    pub any_enabled: bool,
    pub any_content: bool,
    pub in_grace: bool,
}

fn screen_content(content: &Content, settings: &Settings, device: &str, available: &[String]) -> Content {
    let mut local = content.clone();
    local.rows.retain(|row| settings.provider_positions.get(row.provider.as_str())
        .filter(|pos| available.contains(&pos.device)).is_none_or(|pos| pos.device == device));
    let order = settings.ordered_providers();
    local.rows.sort_by_key(|row| order.iter().position(|p| *p == row.provider).unwrap_or(usize::MAX));
    local.any_enabled = !local.rows.is_empty();
    local.any_content = local.rows.iter().any(|r| r.has_data() || r.error.is_some());
    local
}

fn local_content(content: &Content, provider: Option<ProviderId>, moved: &[ProviderId]) -> Content {
    let mut local = content.clone();
    local.rows.retain(|r| match provider {
        Some(provider) => r.provider == provider,
        None => !moved.contains(&r.provider),
    });
    local.any_enabled = !local.rows.is_empty();
    local.any_content = local.rows.iter().any(|r| r.has_data() || r.error.is_some());
    local
}

/// 条当前该不该显示。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StripVisibility {
    Shown,
    Hidden,
}

/// 可见性纯策略（blue `strip_visibility` 的额度多屏版）：
/// - 用户隐藏 → 永不显示（嵌入/heal/重建都不得把它翻出来，blue 同款纪律）；
/// - 没有任何**可显示** provider（启用 ∩ 已添加）→ 隐藏（托盘保留）；
/// - 有内容（窗口数据或错误行）→ 显示；
/// - 无内容：仅启动 `GRACE_SECS` 宽限内显示"获取中…"，过期隐藏（保留托盘）。
fn strip_visibility(
    pref_visible: bool,
    any_enabled: bool,
    any_content: bool,
    in_grace: bool,
) -> StripVisibility {
    if pref_visible && any_enabled && (any_content || in_grace) {
        StripVisibility::Shown
    } else {
        StripVisibility::Hidden
    }
}

/// WM_DESTROY → 重建决策：不是主动退出就要重建。
///
/// 与 blue 的差异（多屏导致）：blue 还要求 `was_embedded`——它只有全局一根条，
/// 浮动条被销毁就等于退出。本项目每秒的 `sync_instances` 按"任务栏期望集合"
/// 幂等判定该不该补条，所以任何非主动销毁都调度一次重建即可；决策**依然
/// 不探测 Shell_TrayWnd 是否存在**——重启后的 Explorer 可能已建好新任务栏，
/// 在这里探测会把"该重建"误判成"静默退出"（blue 原注释照搬）。
fn should_recreate_after_destroy(shutting_down: bool) -> bool {
    !shutting_down
}

// ---------------------------------------------------------------------------
pub struct App {
    settings: Arc<RwLock<Settings>>,
    store: Arc<SnapshotStore>,
    poll: PollHandle,

    /// 隐藏顶层 helper 窗：广播接收 + 1s 心跳 + 重建调度 + 托盘宿主。
    helper: AtomicI32,
    /// 每块任务栏一个 strip 实例（多屏）。
    instances: Mutex<Vec<StripInstance>>,
    monitor_anchors: Mutex<Vec<isize>>,
    panel: Mutex<panel::PanelState>,
    tray: Mutex<tray::TrayState>,

    wm_taskbar_created: AtomicU32,
    shutting_down: AtomicBool,
    pending_recreate: AtomicBool,
    /// 防止消息 + 定时器两路重建重入叠加。
    recreating: AtomicBool,
    recreate_attempts: AtomicU32,
    /// 缓存 settings.text_pct：字体度量路径可能已在持锁状态，不能再锁 settings
    /// （std RwLock 不可重入，会死锁——blue 同款教训）。
    text_pct: AtomicI32,
    /// "获取中…"宽限期起点。
    started: Instant,
    /// 登录/登出后台线程防重入。
    auth_running: AtomicBool,
    /// 登录线程的结果（`MSG_AUTH_DONE` 消费）。
    auth_msg: Mutex<Option<AuthOutcome>>,
    /// strip 左键按下时面板可见 → 本次抬起不再打开（点条关面板的抑制标记）。
    press_closed: AtomicBool,
    drag: Mutex<Option<dragging::DragState>>,
    docking_revision: std::sync::atomic::AtomicU64,
}

impl App {
    fn available_taskbars(&self) -> Vec<HWND> {
        let mut taskbars = enumerate_taskbars(true);
        taskbars.extend(self.monitor_anchors.lock().unwrap().iter().copied().map(hw));
        taskbars
    }

    fn sync_monitor_anchors(&self) {
        let native = enumerate_taskbars(true).into_iter().filter_map(|tb|
            geometry::monitor_device_of(tb).map(|(device,_)| device)).collect::<Vec<_>>();
        let missing = geometry::display_dock_bounds().into_iter()
            .filter(|(device,_,_)| !native.contains(device)).collect::<Vec<_>>();
        let removed = {
            let mut anchors = self.monitor_anchors.lock().unwrap();
            let mut removed = Vec::new();
            anchors.retain(|anchor| {
                let keep = geometry::monitor_device_of(hw(*anchor))
                    .is_some_and(|(device,_)| missing.iter().any(|(name,_,_)| name == &device));
                if !keep { removed.push(*anchor); }
                keep
            });
            removed
        };
        for anchor in removed { unsafe { let _ = DestroyWindow(hw(anchor)); } }
        for (device, rect, _) in missing {
            let existing = self.monitor_anchors.lock().unwrap().iter().copied()
                .find(|anchor| geometry::monitor_device_of(hw(*anchor)).is_some_and(|(name,_)| name == device));
            if let Some(anchor) = existing {
                unsafe { let _ = SetWindowPos(hw(anchor), HWND::default(), rect.left, rect.bottom-72,
                    rect.right-rect.left,72,SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0|SWP_NOZORDER.0)); }
            } else {
                let anchor = unsafe { CreateWindowExW(WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0|WS_EX_NOACTIVATE.0),
                    w!("STATIC"),w!("QuotaBarMonitorAnchor"),WS_POPUP,
                    rect.left,rect.bottom-72,rect.right-rect.left,72,HWND::default(),HMENU::default(),
                    HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0),None) };
                if let Ok(anchor) = anchor {
                    self.monitor_anchors.lock().unwrap().push(hi(anchor));
                    crate::dblog::log(&format!("strip: fallback notification area for {device}"));
                }
            }
        }
    }

    fn new(settings: Arc<RwLock<Settings>>, store: Arc<SnapshotStore>, poll: PollHandle) -> Self {
        let text_pct = settings.read().unwrap().text_pct() as i32;
        Self {
            settings,
            store,
            poll,
            helper: AtomicI32::new(0),
            instances: Mutex::new(Vec::new()),
            monitor_anchors: Mutex::new(Vec::new()),
            panel: Mutex::new(panel::PanelState::default()),
            tray: Mutex::new(tray::TrayState::default()),
            wm_taskbar_created: AtomicU32::new(0),
            shutting_down: AtomicBool::new(false),
            pending_recreate: AtomicBool::new(false),
            recreating: AtomicBool::new(false),
            recreate_attempts: AtomicU32::new(0),
            text_pct: AtomicI32::new(text_pct),
            started: Instant::now(),
            auth_running: AtomicBool::new(false),
            auth_msg: Mutex::new(None),
            press_closed: AtomicBool::new(false),
            drag: Mutex::new(None),
            docking_revision: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn helper_hwnd(&self) -> HWND {
        hw(self.helper.load(Ordering::Relaxed) as isize)
    }

    /// 当前内容快照（settings + store + 宽限期）。调用前不得持有 instances 锁。
    ///
    /// 显示集合 = **启用 ∩ 已添加**（`providers::display_list`）：没添加过凭据的
    /// provider 不上条、不进 `any_enabled`——全新安装时条静默（托盘在），添加
    /// 第一家后自动出现；与菜单「显示勾选」**同一判定**，永不矛盾。
    /// 判定含 keyring 读 → 先克隆 settings 出锁再算（poll 线程的写锁不被 CredRead 拖住；
    /// 克隆 = 13 条小 map，µs 级）。
    fn content(&self) -> Content {
        let (low_threshold, text_pct, dock_left, strip_visible, show_all_monitors, style, snap) = {
            let s = self.settings.read().unwrap();
            (
                s.low_threshold_percent,
                s.text_pct(),
                s.dock_left(),
                s.strip_visible,
                s.show_all_monitors,
                s.style(),
                s.clone(),
            )
        };
        let enabled = crate::providers::display_list(&snap);
        let rows = self.store.rows(&enabled).into_iter()
            .map(|r| snap.display_mode(r.provider).project(r)).collect::<Vec<_>>();
        let any_enabled = !enabled.is_empty();
        let any_content = rows.iter().any(|r| r.has_data() || r.error.is_some());
        let in_grace = self.started.elapsed() < Duration::from_secs(GRACE_SECS);
        Content {
            rows,
            low_threshold,
            text_pct,
            dock_left,
            strip_visible,
            show_all_monitors,
            style,
            any_enabled,
            any_content,
            in_grace,
        }
    }

    fn visibility_of(content: &Content) -> bool {
        strip_visibility(
            content.strip_visible,
            content.any_enabled,
            content.any_content,
            content.in_grace,
        ) == StripVisibility::Shown
    }

    fn metrics(&self, dpi: i32) -> Metrics {
        Metrics::with_text_pct(dpi, self.text_pct.load(Ordering::Relaxed).max(0) as u32)
    }

    fn instance_index(&self, hwnd: HWND) -> Option<usize> {
        let raw = hi(hwnd);
        self.instances
            .lock()
            .unwrap()
            .iter()
            .position(|i| i.hwnd != 0 && i.hwnd == raw)
    }

    // ================================================================ 主循环
    /// 真正的消息循环（契约入口 `run` 调它）。
    fn run_loop(
        settings: Arc<RwLock<Settings>>,
        store: Arc<SnapshotStore>,
        poll: PollHandle,
    ) -> i32 {
        crate::dblog::log("strip: starting（完整任务栏实现：嵌入/多屏/面板/托盘）");
        let app = Arc::new(App::new(settings, store, poll));
        let _ = APP.set(app.clone());
        unsafe {
            let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: WNDCLASS_STYLES(CS_HREDRAW.0 | CS_VREDRAW.0),
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: HICON::default(),
                hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: w!("QuotaBarStrip"),
                hIconSm: HICON::default(),
            };
            if RegisterClassExW(&wc) == 0 {
                crate::dblog::log("strip: register class failed");
                return 1;
            }

            // 隐藏顶层 helper：WS_CHILD 条与消息-only 窗都收不到
            // TaskbarCreated / WM_DISPLAYCHANGE / WM_SETTINGCHANGE 广播，
            // 必须有一个真正的顶层窗替我们接（blue 原注释照搬）。
            let helper = create_helper_window(hinst);
            if helper.0.is_null() {
                crate::dblog::log("strip: helper window creation failed");
                return 1;
            }
            app.helper.store(hi(helper) as i32, Ordering::Relaxed);

            // Explorer 每次（重）建任务栏都会广播这条注册消息。
            app.wm_taskbar_created
                .store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

            app.add_tray_icon();
            let _ = SetTimer(helper, TICK_TIMER, 1000, None);

            // 首轮：建各屏 strip → 嵌入 → 布局/显示 → 托盘。
            app.sync_instances();
            app.heal_all();
            let content = app.content();
            app.refresh_layouts(true, &content);
            app.update_tray();

            // 触发首轮抓取（契约：启动即 kick）。
            app.poll.kick();

            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
                if r.0 == 0 || r.0 == -1 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
                // 重建主路径是 MSG_RECREATE_STRIP；本检查只是 helper 不在时的兜底
                // （blue 同款）。
                if app.pending_recreate.swap(false, Ordering::AcqRel) {
                    app.try_recreate();
                }
            }

            // ---- 正常退出清理 ----
            app.shutting_down.store(true, Ordering::Release);
            app.teardown();
            msg.wParam.0 as i32
        }
    }

    // ============================================================ 实例集合
    /// 多屏同步：按"期望任务栏集合"补建 / 销毁 strip 实例。
    ///
    /// 每个 1s tick 都跑：新增屏的任务栏出现稍晚 → 下个 tick 自动补上；
    /// 拔屏 → 销毁对应 hwnd；窗口创建失败 → 记日志并 arm 500ms 重试定时器。
    fn sync_instances(&self) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        if self.drag.lock().unwrap().as_ref().is_some_and(|d| d.active) { return; }
        let settings = self.settings.read().unwrap().clone();
        let primary = find_primary_taskbar().map(hi);
        self.sync_monitor_anchors();
        let all = self.available_taskbars();
        let mut desired: Vec<(isize, Option<ProviderId>)> = all.iter().filter(|tb| {
            let (device, is_primary) = geometry::monitor_device_of(**tb)
                .unwrap_or_else(|| (String::new(), primary == Some(hi(**tb))));
            should_show_on(&settings.display_devices, &device, is_primary, settings.show_all_monitors)
        }).map(|tb| (hi(*tb), None)).collect();
        for (key, position) in &settings.provider_positions {
            let Some(provider) = ProviderId::parse(key) else { continue };
            if !settings.is_enabled(provider) { continue; }
            if let Some(taskbar) = all.iter().find(|tb|
                geometry::monitor_device_of(**tb).is_some_and(|(device, _)| device == position.device)) {
                if !desired.contains(&(hi(*taskbar), None)) { desired.push((hi(*taskbar), None)); }
            }
        }
        let removed = {
            let mut insts = self.instances.lock().unwrap();
            let mut removed = Vec::new();
            let mut kept = Vec::new();
            for inst in insts.drain(..) {
                let dead = inst.hwnd != 0 && !unsafe { IsWindow(hw(inst.hwnd)) }.as_bool();
                if dead || !desired.contains(&(inst.taskbar, inst.provider)) { removed.push(inst); }
                else { kept.push(inst); }
            }
            *insts = kept;
            for &(taskbar, provider) in &desired {
                if !insts.iter().any(|i| (i.taskbar, i.provider) == (taskbar, provider)) {
                    let mut inst = StripInstance::new(taskbar);
                    inst.provider = provider;
                    insts.push(inst);
                }
            }
            removed
        };
        for mut inst in removed { destroy_instance_window(&mut inst); }
        let need = {
            let insts = self.instances.lock().unwrap();
            insts.iter().filter(|i| i.hwnd == 0).map(|i| (i.taskbar, i.provider)).collect::<Vec<_>>()
        };
        let mut failures = 0u32;
        for (taskbar, provider) in need {
            match self.create_strip_instance(taskbar) {
                Some(mut inst) => {
                    inst.provider = provider;
                    let mut insts = self.instances.lock().unwrap();
                    if let Some(slot) = insts.iter_mut().find(|i|
                        i.taskbar == taskbar && i.provider == provider && i.hwnd == 0) { *slot = inst; }
                    else { insts.push(inst); }
                }
                None => failures += 1,
            }
        }
        if failures > 0 {
            let n = self.recreate_attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 10 == 0 {
                crate::dblog::log(&format!(
                    "strip: {failures} instance create attempt(s) failed (#{n}); retry via helper timer"
                ));
            }
            let helper = self.helper_hwnd();
            if !helper.0.is_null() {
                unsafe {
                    let _ = SetTimer(helper, RECREATE_TIMER, RECREATE_RETRY_MS, None);
                }
            }
        } else if self.recreate_attempts.swap(0, Ordering::Relaxed) != 0 {
            crate::dblog::log("strip: instance recreate recovered");
        }
    }

    /// 单次窗口创建（不重试、不 sleep；失败由 sync 的 500ms 定时器接力，
    /// blue "recreate 管线"同款）。窗口创建期间的早鸟消息会走到 wndproc 的
    /// 分流失败分支 → DefWindowProc，实例登记发生在 CreateWindowExW 返回之后。
    fn create_strip_instance(&self, taskbar: isize) -> Option<StripInstance> {
        unsafe {
            let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
            // 起手是普通非置顶 popup；heal 会改成 WS_CHILD + SetParent。
            // WS_EX_TOPMOST 刻意永不使用：条绝不能浮在全屏视频/游戏上面
            // （blue 原注释照搬）。
            let ex = WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_LAYERED.0);
            let hwnd = CreateWindowExW(
                ex,
                w!("QuotaBarStrip"),
                w!("QuotaBarStrip"),
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
                    // 色键：CHROMA 被 LWA_COLORKEY 透明掉，条因此"没有背景"
                    // （只画内容，见 theme.rs）。
                    let _ = SetLayeredWindowAttributes(
                        h,
                        COLORREF(theme::CHROMA),
                        255,
                        LAYERED_WINDOW_ATTRIBUTES_FLAGS(LWA_COLORKEY.0),
                    );
                    let mut inst = StripInstance::new(taskbar);
                    inst.hwnd = hi(h);
                    let d = GetDpiForWindow(h) as i32;
                    let tp = self.text_pct.load(Ordering::Relaxed) as u32;
                    recreate_fonts(&mut inst, if d <= 0 { BASE_DPI } else { d }, tp);
                    crate::dblog::log(&format!(
                        "strip: created for taskbar {:#x} (dpi {})",
                        taskbar, inst.dpi
                    ));
                    Some(inst)
                }
                Err(e) => {
                    crate::dblog::log(&format!("strip: create window failed ({e})"));
                    None
                }
            }
        }
    }

    /// 重建入口（MSG_RECREATE_STRIP / 500ms 重试定时器 / 消息循环兜底）。
    /// 只做一次幂等 sync，绝不在销毁栈里同步重建（blue 同款）。
    fn try_recreate(&self) {
        if self.shutting_down.load(Ordering::Acquire) {
            self.pending_recreate.store(false, Ordering::Release);
            return;
        }
        if self.recreating.swap(true, Ordering::AcqRel) {
            return;
        }
        self.pending_recreate.store(false, Ordering::Release);
        self.sync_instances();
        self.recreating.store(false, Ordering::Release);
    }

    /// 任务栏句柄全变（TaskbarCreated）→ 全部推倒重建。
    fn rebuild_all_instances(&self) {
        let removed: Vec<StripInstance> = {
            let mut insts = self.instances.lock().unwrap();
            insts.drain(..).collect()
        };
        for mut inst in removed {
            destroy_instance_window(&mut inst);
        }
        self.sync_instances();
    }

    /// WM_DESTROY 调度：只置标志并向 helper POST 消息，真正的重建稍后在消息
    /// 循环里跑——绝不在销毁栈里做，否则会饿死在 GetMessage 里（blue 原注释）。
    fn schedule_recreate(&self) {
        self.pending_recreate.store(true, Ordering::Release);
        self.recreate_attempts.store(0, Ordering::Relaxed);
        let helper = self.helper_hwnd();
        if !helper.0.is_null() {
            unsafe {
                let _ = PostMessageW(helper, MSG_RECREATE_STRIP, WPARAM(0), LPARAM(0));
            }
        }
    }

    // ============================================================== 嵌入自愈
    /// 每秒对**全部实例**跑一遍自愈 + DPI 刷新（与数据脏不脏无关——保持
    /// Explorer 自愈状态新鲜，blue "HEAL EVERY tick" 纪律照搬）。
    ///
    /// 快照在锁内取，SetParent 等 Win32 调用在**锁外**做：这些调用可能同步
    /// 发消息回到 wndproc（要锁 instances），持锁调用会死锁。
    fn heal_all(&self) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let content = self.content();
        let jobs: Vec<(isize, isize, bool, i32)> = {
            self.instances
                .lock()
                .unwrap()
                .iter()
                .map(|i| (i.hwnd, i.taskbar, i.embedded, i.dpi))
                .collect()
        };
        for (hs, ts, was_embedded, dpi) in jobs {
            if hs == 0 || self.monitor_anchors.lock().unwrap().contains(&ts) {
                continue;
            }
            let strip = hw(hs);
            let taskbar = hw(ts);

            // DPI：PMv2 下每块屏一个 DPI；每秒兜底刷新（WM_DPICHANGED 覆盖
            // 不到的静默变化也接得住），变了就重建该实例字体。
            let d = unsafe { GetDpiForWindow(strip) } as i32;
            let d = if d <= 0 { BASE_DPI } else { d };
            if d != dpi {
                let mut insts = self.instances.lock().unwrap();
                if let Some(inst) = insts.iter_mut().find(|i| i.hwnd == hs) {
                    let tp = self.text_pct.load(Ordering::Relaxed) as u32;
                    recreate_fonts(inst, d, tp);
                    inst.layout_valid = false;
                }
                crate::dblog::log("strip: dpi changed -> recreate fonts");
            }

            // 任务栏没了：兜底先脱壳成浮动条（通常子窗口已随父窗口被销毁，
            // sync 紧接着会摘掉本实例；这条路径保证中间态可见且不越界）。
            if !unsafe { IsWindow(taskbar) }.as_bool() {
                self.detach_to_floating(strip);
                let mut insts = self.instances.lock().unwrap();
                if let Some(inst) = insts.iter_mut().find(|i| i.hwnd == hs) {
                    let changed = inst.embedded;
                    inst.embedded = false;
                    if changed {
                        let font_dpi=inst.dpi;
                        recreate_fonts(inst,font_dpi,self.text_pct.load(Ordering::Relaxed) as u32);
                        inst.layout_valid=false;
                    }
                }
                continue;
            }

            // 健康检查：仍是活任务栏的 child → 不动。
            let parent_ok = was_embedded
                && unsafe { GetParent(strip) }
                    .map(|p| p == taskbar)
                    .unwrap_or(false);
            if parent_ok {
                continue;
            }
            if was_embedded {
                // 父窗口陈旧：先脱开再重挂。
                unsafe {
                    let _ = SetParent(strip, HWND::default());
                }
                let mut insts = self.instances.lock().unwrap();
                if let Some(inst) = insts.iter_mut().find(|i| i.hwnd == hs) {
                    let changed = inst.embedded;
                    inst.embedded = false;
                    if changed {
                        let font_dpi=inst.dpi;
                        recreate_fonts(inst,font_dpi,self.text_pct.load(Ordering::Relaxed) as u32);
                        inst.layout_valid=false;
                    }
                }
            }

            let (ok, fail_code) = self.embed_into_taskbar(strip, taskbar);
            {
                let mut insts = self.instances.lock().unwrap();
                if let Some(inst) = insts.iter_mut().find(|i| i.hwnd == hs) {
                    let changed = inst.embedded != ok;
                    inst.embedded = ok;
                    if changed {
                        let font_dpi=inst.dpi;
                        recreate_fonts(inst,font_dpi,self.text_pct.load(Ordering::Relaxed) as u32);
                        inst.layout_valid=false;
                    }
                    if ok {
                        inst.embed_fail_logged = 0;
                    } else if fail_code != 0 && inst.embed_fail_logged != fail_code {
                        // heal 每秒重试：只在**首次失败 / 错误码变化**时记日志——
                        // blue 真机里这一行曾以每秒一条的速度刷了 60+ 次。
                        inst.embed_fail_logged = fail_code;
                        crate::dblog::log(&format!(
                            "taskbar: SetParent failed (code {fail_code}); degrading to floating non-topmost bar"
                        ));
                    }
                }
            }
            if !ok && Self::visibility_of(&content) {
                // 降级可见性兜底：嵌不进去时浮动条必须看得见——仅当可见性策略
                // 允许（用户隐藏的条绝不翻出来，blue 同款纪律）。
                unsafe {
                    let _ = ShowWindow(strip, SW_SHOWNA);
                }
            }
        }
    }

    /// 把条切到 WS_CHILD 并 SetParent 进任务栏。失败返回错误码并还原 popup
    /// 样式（调用方降级为浮动条）。
    ///
    /// 为什么不用 AppBar（SHAppBarMessage）：AppBar 会把条注册成任务栏的
    /// **兄弟窗口**并参与工作区排布（挤掉桌面可用区、还要处理 ABN_* 通知）；
    /// 我们只要任务栏**内部**贴托盘的一小条，SetParent 进去直接用任务栏客户区
    /// 坐标摆放即可——不抢工作区、不注册 appbar、不接收多余广播。
    fn embed_into_taskbar(&self, strip: HWND, shell: HWND) -> (bool, u32) {
        if strip.0.is_null() || shell.0.is_null() {
            return (false, 0);
        }
        unsafe {
            let style = GetWindowLongPtrW(strip, GWL_STYLE) as u32;
            let new_style = (style & !WS_POPUP.0) | WS_CHILD.0;
            if new_style != style {
                SetWindowLongPtrW(strip, GWL_STYLE, new_style as isize);
            }
            // 无论浮着还是嵌着都永不置顶（全屏视频/游戏不被遮挡）。
            let ex = GetWindowLongPtrW(strip, GWL_EXSTYLE) as u32;
            let new_ex = ex & !WS_EX_TOPMOST.0;
            if new_ex != ex {
                SetWindowLongPtrW(strip, GWL_EXSTYLE, new_ex as isize);
            }
            match SetParent(strip, shell) {
                Ok(_) => {
                    crate::dblog::log("taskbar: embedded (WS_CHILD + SetParent)");
                    (true, 0)
                }
                Err(e) => {
                    let code = e.code().0 as u32;
                    // 还原 popup 样式，降级浮动照常工作。
                    SetWindowLongPtrW(
                        strip,
                        GWL_STYLE,
                        ((new_style & !WS_CHILD.0) | WS_POPUP.0) as isize,
                    );
                    (false, code)
                }
            }
        }
    }

    /// 离开（垂死/已死的）任务栏：撤销 WS_CHILD/SetParent，回到独立
    /// WS_POPUP——刻意**不加** WS_EX_TOPMOST，全屏视频/游戏保持不被遮挡
    /// （blue 原注释照搬）。
    fn detach_to_floating(&self, strip: HWND) {
        if strip.0.is_null() {
            return;
        }
        unsafe {
            let _ = SetParent(strip, HWND::default());
            let style = GetWindowLongPtrW(strip, GWL_STYLE) as u32;
            SetWindowLongPtrW(strip, GWL_STYLE, ((style & !WS_CHILD.0) | WS_POPUP.0) as isize);
            let ex = GetWindowLongPtrW(strip, GWL_EXSTYLE) as u32;
            SetWindowLongPtrW(strip, GWL_EXSTYLE, (ex & !WS_EX_TOPMOST.0) as isize);
        }
        crate::dblog::log("taskbar: detached to floating non-topmost mode");
    }

    // ============================================================== 布局/绘制
    /// 布局 + 摆位（`force=true` 数据变化全量重算；`false` 只补 layout 失效的
    /// 实例并做空闲重锚——blue 的 dirty/reanchor 合流版）。
    ///
    /// 锁纪律：settings/store 锁在 instances 锁**外**；SetWindowPos /
    /// InvalidateRect 在锁**外**（可能同步发消息回 wndproc）。
    fn refresh_layouts(&self, force: bool, content: &Content) {
        let settings = self.settings.read().unwrap().clone();
        let devices = self.available_taskbars().into_iter().filter_map(|tb|
            geometry::monitor_device_of(tb).map(|(device, _)| device)).collect::<Vec<_>>();
        let moved = settings.provider_positions.iter().filter(|(_, pos)| devices.contains(&pos.device))
            .filter_map(|(key, _)| ProviderId::parse(key)).collect::<Vec<_>>();
        let jobs = {
            let mut insts = self.instances.lock().unwrap();
            // Positioned providers reserve their space before the remaining shared strip.
            insts.sort_by_key(|i| i.provider.is_none());
            let mut reservations = std::collections::BTreeMap::<isize, Vec<(i32, i32)>>::new();
            let mut jobs = Vec::new();
            for inst in insts.iter_mut() {
                if inst.hwnd == 0 || !unsafe { IsWindow(hw(inst.hwnd)) }.as_bool() { continue; }
                let device = geometry::monitor_device_of(hw(inst.taskbar)).map(|(device,_)| device).unwrap_or_default();
                let mut local = if inst.drag_preview { local_content(content, inst.provider, &moved) }
                    else { screen_content(content, &settings, &device, &devices) };
                if !inst.drag_preview && local.rows.is_empty() && content.any_enabled {
                    local.any_enabled = true; local.any_content = true;
                }
                inst.position = inst.provider.and_then(|p| settings.provider_positions.get(p.as_str()).cloned());
                let m = self.metrics(inst.dpi);
                let want_h = self.strip_content_height(inst, &m);
                let fallback = self.monitor_anchors.lock().unwrap().contains(&inst.taskbar);
                if !inst.drag_preview {
                    let lanes = if fallback {
                        taskbar_client_geometry(hw(inst.taskbar)).map(|(tb,_)|
                            vec![(tb.left+m.sc(8),tb.right-m.sc(8))]).unwrap_or_default()
                    } else { docking::subtract_bounds_for_instance(inst.taskbar, &m,
                        reservations.get(&inst.taskbar).map(Vec::as_slice).unwrap_or(&[])) };
                    let preferred = inst.position.as_ref().filter(|p| p.x_fraction.is_finite())
                        .and_then(|pos| taskbar_client_geometry(hw(inst.taskbar)).map(|(tb, _)|
                            tb.left + ((tb.right - tb.left) as f64 * pos.x_fraction.clamp(0.0, 1.0)) as i32))
                        .unwrap_or(if content.dock_left { i32::MIN / 2 } else { i32::MAX / 2 });
                    let old_lane = inst.lane;
                    // Measure against the largest vacancy, then prefer a nearby lane that fits.
                    inst.lane = lanes.iter().copied().max_by_key(|(a,b)| b-a);
                    if force || !inst.layout_valid || inst.layout_height != want_h || old_lane != inst.lane {
                        inst.layout = compute_layout(inst, &local, &m, want_h);
                    }
                    inst.lane = docking::choose_lane(&lanes, preferred, inst.layout.width, m.st(OVERFLOW_PILL_MIN + MARGIN_X * 2));
                    if inst.lane != old_lane || force || !inst.layout_valid || inst.layout_height != want_h {
                        inst.layout = compute_layout(inst, &local, &m, want_h);
                    }
                } else if force || !inst.layout_valid || inst.layout_height != want_h {
                    inst.layout = compute_layout(inst, &local, &m, want_h);
                }
                inst.layout_valid = true; inst.layout_height = want_h;
                let visible = Self::visibility_of(&local) && (inst.drag_preview || !inst.embedded || inst.lane.is_some());
                let place = self.desired_placement(inst, content.dock_left);
                if visible && !inst.drag_preview {
                    let offset = if inst.embedded { 0 } else { unsafe { geometry::client_origin_screen(hw(inst.taskbar)).x } };
                    reservations.entry(inst.taskbar).or_default().push((place.0 - offset - m.sc(4), place.0 - offset + place.2 + m.sc(4)));
                }
                let moved = inst.last_place != place;
                let vis_now = unsafe { IsWindowVisible(hw(inst.hwnd)) }.as_bool();
                jobs.push((inst.hwnd, place, visible, force || moved || vis_now != visible, fallback));
                inst.last_place = place;
            }
            jobs
        };
        for (hs, place, visible, changed, topmost) in jobs {
            if changed {
                self.apply_placement(hw(hs), place, visible, topmost);
                unsafe { let _ = InvalidateRect(hw(hs), None, false); }
            }
        }
    }

    fn strip_content_height(&self, inst: &StripInstance, m: &Metrics) -> i32 {
        if inst.embedded
            && let Some((tb, _)) = taskbar_client_geometry(hw(inst.taskbar))
        {
            return (tb.bottom - tb.top).max(m.sc(MIN_STRIP_HEIGHT));
        }
        m.sc(STRIP_HEIGHT)
    }

    /// 期望位置：
    /// - 嵌入：任务栏客户区坐标下的纯数学（`strip_placement_in_taskbar`），
    ///   右沿永远停在 `托盘左沿 - GAP_TRAY` 之前（**永不遮挡托盘**）；
    /// - 降级浮动：贴**该任务栏所在 monitor** 的工作区下沿（rcWork 底边就在
    ///   任务栏上沿；本窗口从不置顶，盖任务栏只会被挡住 = 用户看不见——blue
    ///   真机 SetParent 连败 64 次后改成贴工作区的原因，注释照搬）。
    fn desired_placement(&self, inst: &StripInstance, dock_left: bool) -> (i32, i32, i32, i32) {
        let m = self.metrics(inst.dpi);
        if let Some(place) = inst.drag_place {
            return (place.0, place.1, inst.layout.width, place.3);
        }
        if inst.embedded {
            if let Some((tb_client, tray_left)) = taskbar_client_geometry(hw(inst.taskbar)) {
                let mut place = strip_placement_in_taskbar(tb_client, tray_left, dock_left, inst.layout.width, &m);
                if let Some((left, right)) = inst.lane {
                    place.2 = place.2.min(right-left).max(1);
                    let preferred = inst.position.as_ref().filter(|p| p.x_fraction.is_finite())
                        .map(|p| tb_client.left + ((tb_client.right-tb_client.left) as f64 * p.x_fraction.clamp(0.0, 1.0)) as i32)
                        .unwrap_or(if dock_left { left } else { right - place.2 });
                    place.0 = preferred.clamp(left, (right-place.2).max(left));
                }
                return place;
            }
            // 几何抖动这一拍原地不动（blue 同款）。
            return inst.last_place;
        }
        let wa = monitor_work_area(hw(inst.taskbar)).unwrap_or_else(|| {
            let mut r = RECT::default();
            unsafe {
                let _ = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut r as *mut _ as *mut _),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
            }
            r
        });
        let strip_h = m.sc(STRIP_HEIGHT);
        let dock_left = dock_left && !self.monitor_anchors.lock().unwrap().contains(&inst.taskbar);
        let mut x = inst.position.as_ref().filter(|p| p.x_fraction.is_finite())
            .map(|p| wa.left + ((wa.right-wa.left) as f64*p.x_fraction.clamp(0.0, 1.0)) as i32)
            .unwrap_or(if dock_left { wa.left+m.sc(LEFT_INSET) }
                else { wa.right-inst.layout.width-m.sc(GAP_TRAY) });
        if let Some((left, right)) = inst.lane {
            let origin = unsafe { geometry::client_origin_screen(hw(inst.taskbar)) };
            x = x.clamp(origin.x + left, (origin.x + right - inst.layout.width).max(origin.x + left));
        }
        x = x.clamp(wa.left, (wa.right - inst.layout.width).max(wa.left));
        (
            x,
            wa.bottom - strip_h - m.sc(GAP_TRAY),
            inst.layout.width,
            strip_h,
        )
    }

    /// 唯一的显示/隐藏出口（blue 纪律：embed/heal/重建都不得绕过可见性策略）。
    fn apply_placement(&self, hwnd: HWND, place: (i32, i32, i32, i32), visible: bool, topmost: bool) {
        unsafe {
            let mut flags =
                SET_WINDOW_POS_FLAGS(SWP_NOZORDER.0 | SWP_NOACTIVATE.0 | SWP_FRAMECHANGED.0);
            if visible {
                flags = SET_WINDOW_POS_FLAGS(flags.0 | SWP_SHOWWINDOW.0);
            }
            if topmost { flags = SET_WINDOW_POS_FLAGS(flags.0 & !SWP_NOZORDER.0); }
            let _ = SetWindowPos(
                hwnd,
                if topmost { windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST } else { HWND::default() },
                place.0,
                place.1,
                place.2,
                place.3,
                flags,
            );
            if !visible {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    /// WM_PAINT：双缓冲 + 色键底 + 段序列（色点 / 短名 / 百分比 / ` · ` /
    /// `+n` 折叠胶囊）。
    fn paint_strip(&self, hwnd: HWND) {
        // 快照布局与字体（锁内），GDI 绘制在锁外。
        let snap = {
            let insts = self.instances.lock().unwrap();
            insts
                .iter()
                .find(|i| i.hwnd == hi(hwnd))
                .map(|i| (i.layout.clone(), i.name_font, i.pill_font, i.dpi, i.embedded))
        };
        let Some((lay, name_font, pill_font, dpi, embedded)) = snap else {
            return; // 窗口正在销毁
        };
        if name_font == 0 || pill_font == 0 {
            return;
        }
        let m = self.metrics(dpi);
        let theme = theme::detect();
        // Opaque floating bars can render clean glyphs without any shadow. The
        // taskbar child remains transparent, with a thin contrast-preserving shadow.
        let draw_text_halo = |dc, x, y, text: &str, fg, outline| {
            if embedded { paint::draw_text_halo(dc,x,y,text,fg,outline); }
            else { paint::draw_text_plain(dc,x,y,text,fg); }
        };
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if rc.right <= 0 || rc.bottom <= 0 {
                let _ = EndPaint(hwnd, &ps);
                return;
            }

            let md = CreateCompatibleDC(hdc);
            let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
            let oldbmp = SelectObject(md, HGDIOBJ(bmp.0));

            // 色键底：CHROMA 被 LWA_COLORKEY 透明掉 → 条"没有背景"。
            let bg = CreateSolidBrush(COLORREF(if embedded { theme::CHROMA } else { theme.panel_bg }));
            let _ = FillRect(md, &rc, bg);
            let _ = DeleteObject(HGDIOBJ(bg.0));

            let cy = (rc.bottom - rc.top) / 2;
            let mut cx = m.st(MARGIN_X);
            let name_f = HGDIOBJ(name_font as _);
            let pill_f = HGDIOBJ(pill_font as _);

            // 段间分隔与测宽严格同源（见 compute_layout）：盒式段之间 = 纯
            // 间隙 CHIP_GAP（目标图盒与盒只留空隙，不画 ` · ` 文本分隔，
            // 余额也是独立盒），text 段 = ` · `。
            let boxed = lay.segs.iter().any(|s| s.chip.is_some());
            let (sep_w, sep_h) = {
                let old = SelectObject(md, name_f);
                let s = text_size(md, SEP_TEXT);
                SelectObject(md, old);
                s
            };
            let chip_gap = m.sc(CHIP_GAP);
            // 条内容高：行块 / tile 垂直居中的基准。
            let content_h = (rc.bottom - rc.top).max(1);

            for (i, seg) in lay.segs.iter().enumerate() {
                if i > 0 {
                    if boxed {
                        cx += chip_gap;
                    } else {
                        let old = SelectObject(md, name_f);
                        draw_text_halo(
                            md,
                            cx,
                            cy - sep_h / 2,
                            SEP_TEXT,
                            theme.fg_dim,
                            theme.fg_outline,
                        );
                        SelectObject(md, old);
                        cx += sep_w;
                    }
                }

                if let Some(chip) = &seg.chip {
                    // ===== chips 段（QuotaDock 血条形态，**底改透明**——不画
                    // 容器盒）：tile（跨行垂直居中）→ 行区（标签 | 血条 | 数值），
                    // 内容直接贴任务栏，段间只留 CHIP_GAP 间隙 =====
                    let g = {
                        let old = SelectObject(md, name_f);
                        let g = measure_chip(md, &m, chip);
                        SelectObject(md, old);
                        g
                    };
                    // Real provider artwork, no tile plate or colored frame.
                    let tile_x = cx + m.st(CHIP_PAD_X);
                    let plate = m.sc(TILE);
                    let logo_px = (plate - 2 * m.sc(LOGO_PAD)).max(8);
                    let off = (plate - logo_px) / 2;
                    if !logos::draw_strip_logo(md, seg.provider, tile_x + off,
                        cy - plate / 2 + off, logo_px as u32) {
                        let old = SelectObject(md, name_f);
                        paint::draw_text_plain(md, tile_x, cy - plate / 2,
                            &chip.tile.glyph, theme.fg);
                        SelectObject(md, old);
                    }
                    let row_x = cx + g.row_x;
                    let old = SelectObject(md, name_f);
                    if chip.single_row() {
                        // 单行段（余额/错误/占位）：label + 空格 + text。
                        let row = &chip.rows[0];
                        let mut x = row_x;
                        if !row.label.is_empty() {
                            let (lw, lh) = text_size(md, &row.label);
                            draw_text_halo(
                                md,
                                x,
                                cy - lh / 2,
                                &row.label,
                                row.label_color,
                                theme.fg_outline,
                            );
                            x += lw + text_size(md, " ").0;
                        }
                        let (tw, th) = text_size(md, &row.text);
                        let _ = tw; // 宽度已在 measure_chip 计入，这里只取高做基线
                        draw_text_halo(md, x, cy - th / 2, &row.text, row.text_color, theme.fg_outline);
                        
                    } else {
                        // 血条行：行块整体垂直居中；标签左、血条中（弹性列）、
                        // 数值右对齐行区右缘。
                        let row_h = m.st(CHIP_ROW_H);
                        let total_h = chip.rows.len() as i32 * row_h;
                        let mut y = (content_h - total_h) / 2;
                        for row in &chip.rows {
                            let ry = y + row_h / 2;
                            let (_lw, lh) = text_size(md, &row.label);
                            // 标签列宽以 measure_chip 统一的最宽值为准（右列对齐），
                            // 这里只取高做基线。
                            draw_text_halo(
                                md,
                                row_x,
                                ry - lh / 2,
                                &row.label,
                                row.label_color,
                                theme.fg_outline,
                            );
                            let bar_x = row_x + g.label_w + m.st(CHIP_COL_GAP);
                            // 轨道 #3A3A44；填充 = 该窗口 level 色（与数值同色）；
                            // 缺失窗口 bar_frac=None → 只画空轨道（无填充，不造 0）。
                            paint::draw_bar(
                                md,
                                bar_x,
                                ry,
                                g.bar_w,
                                m.sc(CHIP_BAR_H),
                                theme.panel_border,
                                row.bar_frac,
                                row.bar_color,
                            );
                            let (tw, th) = text_size(md, &row.text);
                            let tx = bar_x + g.bar_w + m.st(CHIP_COL_GAP) + (g.text_w - tw);
                            draw_text_halo(
                                md,
                                tx,
                                ry - th / 2,
                                &row.text,
                                row.text_color,
                                theme.fg_outline,
                            );
                            
                            y += row_h;
                        }
                    }
                    SelectObject(md, old);
                    cx += g.width;
                } else {
                    // ===== text 一行式段：色点 + 片段（原逻辑冻结）=====
                    draw_status_dot(md, cx, cy, m.st(DOT), seg.dot);
                    cx += m.st(DOT) + m.st(DOT_GAP);
                    let old = SelectObject(md, name_f);
                    for (text, color) in &seg.spans {
                        let (tw, th) = text_size(md, text);
                        draw_text_halo(md, cx, cy - th / 2, text, *color, theme.fg_outline);
                        cx += tw;
                    }
                    SelectObject(md, old);
                }
            }

            // "+n" 折叠胶囊：放不下的段折到这里；点击整条（含胶囊）开面板。
            if lay.overflow > 0 {
                if !lay.segs.is_empty() {
                    if boxed {
                        cx += chip_gap;
                    } else {
                        let old = SelectObject(md, name_f);
                        draw_text_halo(
                            md,
                            cx,
                            cy - sep_h / 2,
                            SEP_TEXT,
                            theme.fg_dim,
                            theme.fg_outline,
                        );
                        SelectObject(md, old);
                        cx += sep_w;
                    }
                }
                let text = format!("+{}", lay.overflow);
                let old = SelectObject(md, pill_f);
                let (pw, ph) = text_size(md, &text);
                let pad = m.st(PILL_PAD);
                let pill_w = pw + pad * 2;
                // 胶囊底色随可见段的整体严重度（低>中>好>未知），与色点/托盘
                // 饼环同一套阈值色；无可见段时退回中性灰。
                let pill_bg = if lay.segs.iter().any(|s| s.dot == theme.dot_low) {
                    theme.pill_low
                } else if lay.segs.iter().any(|s| s.dot == theme.dot_mid) {
                    theme.pill_mid
                } else if lay.segs.iter().any(|s| s.dot == theme.dot_ok) {
                    theme.pill_ok
                } else {
                    theme.pill_off_bg
                };
                let pill_fg = if pill_bg == theme.pill_off_bg {
                    theme.pill_off_fg
                } else {
                    theme::rgb(255, 255, 255)
                };
                let pbr = CreateSolidBrush(COLORREF(pill_bg));
                let prect = RECT {
                    left: cx,
                    top: cy - ph / 2 - m.st(2),
                    right: cx + pill_w,
                    bottom: cy + ph / 2 + m.st(2),
                };
                let _ = FillRect(md, &prect, pbr);
                let _ = DeleteObject(HGDIOBJ(pbr.0));
                draw_text_halo(md, cx + pad, cy - ph / 2, &text, pill_fg, theme.fg_outline);
                SelectObject(md, old);
            }

            let _ = BitBlt(
                hdc,
                0,
                0,
                rc.right,
                rc.bottom,
                md,
                0,
                0,
                ROP_CODE(0x00CC0020),
            );
            let _ = SelectObject(md, oldbmp);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(md);
            let _ = EndPaint(hwnd, &ps);
        }
    }

    // ============================================================== 心跳
    /// 1s 定时器：气泡 → 实例同步 → 嵌入自愈 → 脏则全量重绘，否则空闲重锚。
    fn tick(&self) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        // 契约：take_alert 在 1s 定时器里取 → 托盘气泡（NIF_INFO/NIIF_WARNING）。
        if let Some(text) = self.store.take_alert() {
            self.show_balloon("额度提醒", &text, true);
        }
        // 先补/删实例（副屏任务栏可能晚于 displaychange 出现 → 本拍补上），
        // 再 heal（新实例本拍就嵌入），最后按脏标记重绘。
        let dragging = self.drag.lock().unwrap().as_ref().is_some_and(|d| d.active);
        if dragging {
            if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 as i32) } < 0 { self.drag_finish(false); }
            return;
        }
        self.sync_instances();
        self.heal_all();
        let content = self.content();
        let revision = docking::revision();
        let geometry_changed = self.docking_revision.swap(revision, Ordering::Relaxed) != revision;
        if self.store.take_dirty() || geometry_changed {
            self.refresh_layouts(true, &content);
            self.update_tray();
            if self.panel_visible() {
                self.size_and_paint_panel();
            }
        } else {
            self.refresh_layouts(false, &content);
        }
    }

    // ============================================================== 菜单
    fn show_menu(&self, owner: HWND) {
        let cfg = { self.settings.read().unwrap().clone() };
        // 账号表 / 显示器列表在**打开菜单前**实时取（keyring 会话 + 配置账号 +
        // 热插拔屏后的最新枚举），交给菜单层做勾选/radio。账号表只对**已添加**
        // 的 provider 取（与「供应商管理只列已添加」同一口径）。
        let accounts: Vec<(ProviderId, Vec<crate::oauth::AccountInfo>)> = ProviderId::ALL
            .iter()
            .filter(|id| crate::providers::is_configured(&cfg, **id))
            .map(|id| (*id, crate::accounts::list(&cfg, *id)))
            .collect();
        let displays = geometry::detect_displays();
        if let Some(cmd) = menu::show(owner, &cfg, &accounts, &displays) {
            // owner 传给命令执行：AddAccount 的输入框用它做 DPI/居中基准。
            self.on_menu_cmd(cmd, owner);
        }
    }

    /// 托盘右键菜单的 owner：优先某条 strip 的 hwnd（嵌入后 root = 任务栏，
    /// blue 验证过的"菜单需要前台 owner"路径）；没有条就用 helper。
    fn menu_owner(&self) -> HWND {
        let first = self
            .instances
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.hwnd != 0)
            .map(|i| i.hwnd);
        match first {
            Some(h) => hw(h),
            None => self.helper_hwnd(),
        }
    }

    /// 设置变更统一出口（契约硬约定）：
    /// **磁盘为权威源** → `f` 施加变更 → `save()` 写盘 → 同步内存副本 →
    /// `poll.kick()` → `store.mark_dirty()`；`f` 的返回值原样带回
    /// （账号增删这类"可能失败"的操作据此拿结果，如 keyring 写失败）。
    ///
    /// 必须先 `Settings::load()` 再改：CLI（setkey/enable/login）可能在上一次
    /// 轮询重载之后写过配置，内存副本最长滞后一个轮询周期——直接在内存上
    /// 改了再存盘会把那些 CLI 变更覆盖丢（曾把 opencode-go 的 secret_ref 冲掉）。
    fn commit<R>(&self, f: impl FnOnce(&mut Settings) -> R) -> R {
        let result = {
            let mut fresh = Settings::load();
            let result = f(&mut fresh);
            fresh.save();
            let mut s = self.settings.write().unwrap();
            *s = fresh;
            self.text_pct.store(s.text_pct() as i32, Ordering::Relaxed);
            result
        };
        self.poll.kick();
        self.store.mark_dirty();
        result
    }

    fn on_menu_cmd(&self, cmd: MenuCmd, owner: HWND) {
        match cmd {
            MenuCmd::ToggleVisible => {
                self.commit(|s| s.strip_visible = !s.strip_visible);
            }
            MenuCmd::Dock(left) => {
                self.commit(|s| {
                    s.strip_side = if left { "left" } else { "right" }.into();
                });
            }
            MenuCmd::Monitors(all) => {
                // 旧 bool 模式与新白名单互斥：选全部/仅主屏 = 回到 None 模式。
                self.commit(|s| {
                    s.show_all_monitors = all;
                    s.display_devices = None;
                });
                // 契约：多屏切换 → save+kick+dirty **并立即重建实例集合**。
                self.sync_instances();
                self.heal_all();
            }
            MenuCmd::DisplaySet(devices) => {
                // 显示器多选：写白名单（Some），立即按新门控重建实例集合
                //（期望集合每 tick 也会重算，这里是"跟手"的即时版）。
                self.commit(|s| s.display_devices = Some(devices));
                self.sync_instances();
                self.heal_all();
            }
            MenuCmd::AccountDisplay(ids) => {
                // 账号显示白名单（**存储即视图** → 条/面板/告警同拍生效，
                // 走既有尾部刷新）；Some(空) = 全部账号行隐藏（AGY 退占位），
                // 照实生效。
                self.commit(|s| s.display_accounts = Some(ids));
            }
            MenuCmd::Font(pct) => {
                self.commit(|s| s.text_pct = pct);
                // 字号决定字体、圆点、间距、胶囊——必须全量重建字体再重排。
                self.recreate_fonts_all();
            }
            MenuCmd::Style(st) => {
                // 样式切换只换段的构建分支，**不改字体**（与字号档不同）——
                // 只走 commit：save + kick + mark_dirty，下拍按新样式重排。
                self.commit(|s| s.strip_style = st.as_str().into());
            }
            MenuCmd::Interval(secs) => {
                self.commit(|s| s.poll_seconds = secs); // commit 已 kick
            }
            MenuCmd::Refresh => self.poll.kick(),
            MenuCmd::Provider(id, enable) => {
                self.commit(|s| {
                    let _ = s.set_enabled(id, enable);
                });
            }
            MenuCmd::ResetPosition(provider) => {
                self.commit(|s| {
                    if let Some(provider) = provider { s.provider_positions.remove(provider.as_str()); }
                    else { s.provider_positions.clear(); s.provider_order.clear(); }
                });
                self.sync_instances(); self.heal_all();
            }
            MenuCmd::ProviderScreen(provider, device) => {
                self.commit(|s| {
                    if let Some(device) = device { s.provider_positions.insert(provider.as_str().into(),
                        crate::config::ProviderPosition { device, x_fraction: 0.0 }); }
                    else { s.provider_positions.remove(provider.as_str()); }
                });
                self.sync_instances(); self.heal_all();
            }
            MenuCmd::ProviderOrder(provider, direction) => {
                let visible = self.content().rows.iter().map(|r| r.provider).collect::<Vec<_>>();
                self.commit(|s| {
                    let order = s.ordered_providers().into_iter().filter(|p| visible.contains(p)).collect::<Vec<_>>();
                    if let Some(index) = order.iter().position(|p| *p == provider) {
                        let next = (index as i32 + direction).clamp(0,order.len() as i32-1) as usize;
                        s.swap_providers(provider,order[next]);
                    }
                });
            }
            MenuCmd::DisplayMode(id, mode) => {
                self.commit(|s| { s.display_modes.insert(id.as_str().into(), mode); });
            }
            MenuCmd::AddAccount(id) => {
                if id.credential_kind() == CredentialKind::Cookie { self.start_auth(id, true); }
                else { self.add_credential(id, owner); }
            }
            MenuCmd::PasteCookie(id) => self.add_credential(id, owner),
            MenuCmd::RemoveAccount(id, account_id) => self.remove_account(id, &account_id),
            MenuCmd::Notify => {
                self.commit(|s| s.notify_low = !s.notify_low);
            }
            MenuCmd::Autostart => {
                // 以磁盘为准取反（与 commit 的磁盘权威模型一致，不读可能滞后的内存副本）
                let on = !Settings::load().autostart;
                self.commit(|s| s.autostart = on);
                set_autostart(on);
            }
            MenuCmd::Exit => self.begin_shutdown(),
        }
        // 立即刷新视觉（不等下一个 1s tick，菜单操作必须跟手）。
        let content = self.content();
        self.refresh_layouts(true, &content);
        self.update_tray();
        if self.panel_visible() {
            self.size_and_paint_panel();
        }
    }

    /// 「添加账号」：按凭据形态分流 —— OAuth（Antigravity / Codex）走浏览器登录
    /// 线程；Cookie / API key 弹**模态输入框**，值写入凭据管理器并自动启用。
    ///
    /// 多账号：同一 provider 已有账号时，追加**新账号**（不覆盖旧的），并再弹一次
    /// 「账号名称」（可留空 → 按凭据尾号自动命名）。输入框取消或留空 = 不改动
    /// （删除走菜单「移除账号」或 CLI `setkey <id> --delete`）。
    fn add_credential(&self, id: ProviderId, owner: HWND) {
        let kind = id.credential_kind();
        crate::dblog::log(&format!("menu: AddAccount({}) kind={:?}", id.as_str(), kind));
        if kind == CredentialKind::OAuth {
            // Antigravity / Codex：PKCE 登录（后台线程 + 气泡反馈，已带防重入）。
            self.start_auth(id, true);
            return;
        }
        let existing = crate::accounts::list(&self.settings.read().unwrap(), id).len();
        let (label, hint): (String, &'static str) = match kind {
            CredentialKind::Cookie => (
                format!("粘贴 {} 的平台 Cookie", id.display_name()),
                // 各站取 Cookie 的路径不同（MiMo 用 Cookie 请求头 / StepFun 用
                // Application → Cookies 里的 Oasis-Token）。
                id.cookie_source_hint(),
            ),
            _ => (
                format!("粘贴 {} 的 API Key", id.display_name()),
                "保存到 Windows 凭据管理器（配置文件不落密钥）；留空取消。",
            ),
        };
        let title = format!("添加账号 — {}", id.display_name());
        // owner = 刚收起菜单的条/托盘窗：输入框按它所在屏取 DPI 并居中。
        let Some(text) = prompt::run(owner, &title, &label, hint) else {
            return; // 取消 / 进程内已有框 / 窗口创建失败
        };
        let value = text.trim().to_string();
        if value.is_empty() {
            return; // 留空 = 取消（不覆盖旧值）
        }
        // 已有账号时问一次名字（可留空）；首个账号不必问（尾号足够区分）。
        let name = if existing > 0 {
            prompt::run(
                owner,
                &format!("账号名称 — {}", id.display_name()),
                "给这个账号起个名字（可留空）",
                "留空 = 按凭据尾号自动命名（如 …1f2a）；名称只影响展示。",
            )
            .unwrap_or_default()
        } else {
            String::new()
        };
        // keyring 写入 + 配置改动一起进 commit（先写 keyring，成功才落盘配置）。
        let name = name.trim().to_string();
        let result = self.commit(|s| crate::accounts::add(s, id, &name, &value));
        match result {
            Ok(acc) => {
                crate::dblog::log(&format!(
                    "menu: AddAccount({}) saved+enabled ({acc})",
                    id.as_str()
                ));
                self.show_balloon(
                    "已添加",
                    &format!("{} 已保存并启用", id.display_name()),
                    false,
                );
            }
            Err(e) => {
                crate::dblog::log(&format!("menu: AddAccount({}) failed: {e}", id.as_str()));
                self.show_balloon("保存失败", &format!("{}：{e}", id.display_name()), true);
            }
        }
    }

    /// 「移除账号」：OAuth 删账号表条目，API key / Cookie 删 keyring 凭据 + 配置；
    /// 同时从显示白名单摘掉。结果走托盘气泡（成功/失败都反馈）。
    fn remove_account(&self, id: ProviderId, account_id: &str) {
        crate::dblog::log(&format!(
            "menu: RemoveAccount({}) account={account_id}",
            id.as_str()
        ));
        let result = self.commit(|s| crate::accounts::remove(s, id, account_id));
        match result {
            Ok(()) => self.show_balloon(
                "已移除",
                &format!("{} 的账号已移除", id.display_name()),
                false,
            ),
            Err(e) => self.show_balloon("移除失败", &format!("{}：{e}", id.display_name()), true),
        }
    }

    /// 登录/登出：**必须后台线程**（浏览器 PKCE + 回环回调可能分钟级），结果
    /// `PostMessageW` 回主线程 → save/kick/dirty/气泡提示（契约硬约定）。
    fn start_auth(&self, id: ProviderId, login: bool) {
        if self.auth_running.swap(true, Ordering::AcqRel) {
            self.show_balloon(id.display_name(), "登录/登出进行中，请稍候", false);
            return;
        }
        let Some(app) = APP.get().cloned() else {
            self.auth_running.store(false, Ordering::Relaxed);
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("quota-auth".into())
            .spawn(move || {
                let res = if login {
                    if id.credential_kind() == CredentialKind::Cookie {
                        crate::browser_login::login_blocking(id)
                    } else { crate::oauth::login_blocking(id) }
                } else {
                    crate::oauth::logout(id)
                };
                let (ok, title, text) = match res {
                    Ok(()) => (
                        true,
                        if login { "登录成功" } else { "已登出" },
                        if login {
                            format!("{} 已登录", id.display_name())
                        } else {
                            format!("{} 已登出", id.display_name())
                        },
                    ),
                    Err(e) => (
                        false,
                        if login { "登录失败" } else { "登出失败" },
                        e,
                    ),
                };
                *app.auth_msg.lock().unwrap() = Some(AuthOutcome {
                    ok,
                    login,
                    provider: id,
                    title: title.to_string(),
                    text,
                });
                let helper = app.helper_hwnd();
                if !helper.0.is_null() {
                    unsafe {
                        let _ = PostMessageW(helper, MSG_AUTH_DONE, WPARAM(0), LPARAM(0));
                    }
                }
                app.auth_running.store(false, Ordering::Relaxed);
            });
        if let Err(e) = spawned {
            crate::dblog::log(&format!("auth: spawn failed ({e})"));
            self.auth_running.store(false, Ordering::Relaxed);
        }
    }

    fn on_auth_done(&self) {
        let msg = self.auth_msg.lock().unwrap().take();
        if let Some(AuthOutcome {
            ok,
            login,
            provider,
            title,
            text,
        }) = msg
        {
            // **登录并集边界（本侧）**：登录成功且处于 Some 账号白名单模式 →
            // 把**新活动账号**（`"{provider}:{id}"`）并入名单再 commit 写回，否则
            // Some 模式下新登录的账号不显示（CLI 登录的同款并集在 main.rs）。
            // 先在锁外算好 active id（keyring 读取不进 settings 写锁）。
            let active = if ok && login {
                crate::oauth::list_accounts(provider)
                    .iter()
                    .find(|a| a.active)
                    .map(|a| crate::config::account_key(provider, &a.id))
            } else {
                None
            };
            // 契约：登录/登出完成 → save → kick → dirty → 气泡提示。
            self.commit(move |s| {
                if ok && login { s.set_enabled(provider, true); }
                if let Some(key) = active.as_ref()
                    && let Some(list) = s.display_accounts.as_mut()
                    && !list.contains(key)
                {
                    list.push(key.clone());
                }
            });
            self.show_balloon(&title, &text, !ok);
            let content = self.content();
            self.refresh_layouts(true, &content);
        }
    }

    fn begin_shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::Acquire) {
            return;
        }
        unsafe {
            PostQuitMessage(0);
        }
    }

    /// 字号/广播变化 → 全量重建实例字体（先删旧再建，防 GDI 泄漏）。
    fn recreate_fonts_all(&self) {
        let tp = self.text_pct.load(Ordering::Relaxed).max(0) as u32;
        let mut insts = self.instances.lock().unwrap();
        for inst in insts.iter_mut() {
            if inst.hwnd == 0 {
                continue;
            }
            let d = unsafe { GetDpiForWindow(hw(inst.hwnd)) } as i32;
            recreate_fonts(inst, if d <= 0 { BASE_DPI } else { d }, tp);
            inst.layout_valid = false;
        }
    }

    /// 托盘左键：在光标所在屏的 strip 处打开面板；无条时退到主任务栏锚点。
    fn show_panel_for_cursor(&self) {
        let mut pt = POINT::default();
        let cursor_mon = unsafe { GetCursorPos(&mut pt) }
            .is_ok()
            .then(|| unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) });
        let insts: Vec<(isize, isize)> = {
            self.instances
                .lock()
                .unwrap()
                .iter()
                .filter(|i| i.hwnd != 0)
                .map(|i| (i.hwnd, i.taskbar))
                .collect()
        };
        let mut chosen: Option<(isize, isize)> = None;
        if let Some(mon) = cursor_mon {
            for &(hs, ts) in &insts {
                let m = unsafe { MonitorFromWindow(hw(ts), MONITOR_DEFAULTTONEAREST) };
                if m == mon {
                    chosen = Some((hs, ts));
                    break;
                }
            }
        }
        if chosen.is_none() {
            chosen = insts.first().copied();
        }
        let (strip, taskbar) = match chosen {
            Some(v) => v,
            None => match find_primary_taskbar() {
                Some(tb) => (0, hi(tb)),
                None => return, // 连主任务栏都没有（Explorer 未就绪）
            },
        };
        self.show_panel_at(hw(strip), hw(taskbar));
    }

    // ============================================================== 清理
    /// 正常退出清理（幂等；消息循环结束后跑一次）。
    fn teardown(&self) {
        let helper = self.helper_hwnd();
        if !helper.0.is_null() {
            unsafe {
                let _ = KillTimer(helper, TICK_TIMER);
                let _ = KillTimer(helper, RECREATE_TIMER);
            }
        }
        self.destroy_panel();
        self.remove_tray_icon();
        // 先摘集合再销毁（窗口已不在集合里 → WM_DESTROY 只走 DefWindowProc，
        // 不会触发重建调度）。
        let removed: Vec<StripInstance> = {
            let mut insts = self.instances.lock().unwrap();
            insts.drain(..).collect()
        };
        for mut inst in removed {
            destroy_instance_window(&mut inst);
        }
        if !helper.0.is_null() {
            unsafe {
                let _ = DestroyWindow(helper);
            }
            self.helper.store(0, Ordering::Relaxed);
        }
        let anchors = self.monitor_anchors.lock().unwrap().drain(..).collect::<Vec<_>>();
        for anchor in anchors { unsafe { let _ = DestroyWindow(hw(anchor)); } }
        crate::dblog::log("strip: teardown done");
    }

    // ============================================================ 消息处理
    fn handle_strip_message(
        &self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_ERASEBKGND => LRESULT(1), // 背景自己画（双缓冲），别让系统擦
            WM_PAINT => {
                self.paint_strip(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                self.drag_start(hwnd, lparam);
                // 点条：面板此刻开着 → 就地关掉并记住"已由本次按下关闭"，
                // 抬起时不再打开（helper/条都是 NOACTIVATE，点条不会先失焦，
                // 所以这个抑制标记能做出真 toggle）。
                if self.panel_visible() {
                    self.hide_panel();
                    self.press_closed.store(true, Ordering::Relaxed);
                } else {
                    self.press_closed.store(false, Ordering::Relaxed);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => { self.drag_move(hwnd); LRESULT(0) }
            WM_CAPTURECHANGED | WM_CANCELMODE => {
                let own_drag = self.drag.lock().unwrap().as_ref().is_some_and(|d| d.source == hi(hwnd));
                if own_drag { self.drag_finish(false); }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if self.drag_finish(true) { self.press_closed.store(false, Ordering::Relaxed); return LRESULT(0); }
                if self.press_closed.swap(false, Ordering::Relaxed) {
                    return LRESULT(0);
                }
                let anchor = {
                    let insts = self.instances.lock().unwrap();
                    insts
                        .iter()
                        .find(|i| i.hwnd == hi(hwnd))
                        .map(|i| (i.hwnd, i.taskbar))
                };
                if let Some((hs, ts)) = anchor {
                    self.show_panel_at(hw(hs), hw(ts));
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                self.show_menu(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                // 显式关闭请求 = 退出（与菜单"退出"同路径）。
                self.begin_shutdown();
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // 该 strip 被挪到不同 DPI 的屏 / 系统缩放变化 → 重建本实例字体。
                let content = self.content();
                {
                    let mut insts = self.instances.lock().unwrap();
                    if let Some(inst) = insts.iter_mut().find(|i| i.hwnd == hi(hwnd)) {
                        let d = unsafe { GetDpiForWindow(hwnd) } as i32;
                        let d = if d <= 0 { BASE_DPI } else { d };
                        let tp = self.text_pct.load(Ordering::Relaxed) as u32;
                        recreate_fonts(inst, d, tp);
                        inst.layout_valid = false;
                    }
                }
                self.refresh_layouts(false, &content);
                LRESULT(0)
            }
            WM_DESTROY => {
                // 区分"用户要退出"（菜单退出/WM_CLOSE 先置 shutting_down）与
                // "Explorer 死了带走嵌入子窗口"。**决策不探测任务栏是否存在**：
                // 重启后的 Explorer 可能已建好新 Shell_TrayWnd，探测会把"该重建"
                // 误判成"静默退出"（blue 原注释照搬）。集合里已摘掉的实例
                // （同步销毁/退出清理）根本走不到这里——分流时查不到 hwnd。
                let shutting_down = self.shutting_down.load(Ordering::Acquire);
                if should_recreate_after_destroy(shutting_down) {
                    let was_embedded = {
                        let mut insts = self.instances.lock().unwrap();
                        match insts.iter().position(|i| i.hwnd == hi(hwnd)) {
                            Some(idx) => {
                                let mut inst = insts.remove(idx);
                                let emb = inst.embedded;
                                delete_instance_fonts(&mut inst);
                                emb
                            }
                            None => false,
                        }
                    };
                    crate::dblog::log(&format!(
                        "strip: destroyed unexpectedly (embedded={was_embedded}); scheduling recreate"
                    ));
                    // 面板是独立顶层弹窗：条没了要一并收掉，否则永远悬在屏上。
                    self.destroy_panel();
                    self.schedule_recreate();
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn handle_helper_message(
        &self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TIMER if wparam.0 == TICK_TIMER => {
                self.tick();
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == RECREATE_TIMER => {
                // 延迟重建重试：每拍只试一次，失败由 sync 再次 arm
                // （绝不 sleep UI 线程，blue 同款）。
                unsafe {
                    let _ = KillTimer(hwnd, RECREATE_TIMER);
                }
                self.try_recreate();
                LRESULT(0)
            }
            MSG_RECREATE_STRIP => {
                unsafe {
                    let _ = KillTimer(hwnd, RECREATE_TIMER);
                }
                self.try_recreate();
                LRESULT(0)
            }
            MSG_AUTH_DONE => {
                self.on_auth_done();
                LRESULT(0)
            }
            tray::TRAY_MSG => {
                let code = (lparam.0 & 0xFFFF) as u32;
                match code {
                    0x0201 => {
                        // WM_LBUTTONDOWN：切换面板。helper 是 WS_EX_NOACTIVATE，
                        // 点托盘不改变前台 → 面板保持焦点，这里是真 toggle。
                        if self.panel_visible() {
                            self.hide_panel();
                        } else {
                            self.show_panel_for_cursor();
                        }
                    }
                    0x0205 => {
                        // WM_RBUTTONUP → 右键菜单。
                        self.show_menu(self.menu_owner());
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            m if {
                let tbc = self.wm_taskbar_created.load(Ordering::Relaxed);
                tbc != 0 && m == tbc
            } => {
                // Explorer 重启：任务栏句柄全变 → 全量重建实例 + 重挂托盘。
                crate::dblog::log("taskbar: TaskbarCreated -> rebuild instances + re-add tray");
                self.rebuild_all_instances();
                self.add_tray_icon();
                let content = self.content();
                self.refresh_layouts(true, &content);
                self.update_tray();
                LRESULT(0)
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                // 显示器拓扑 / 系统设置（含缩放比例）变化：全量 DPI 刷新 + 实例
                // 同步 + 重排。新增屏的任务栏可能稍晚出现 → 每秒 tick 自动补齐。
                crate::dblog::log("broadcast via helper -> refresh dpi + sync + relayout");
                self.recreate_fonts_all();
                self.sync_instances();
                let content = self.content();
                self.refresh_layouts(true, &content);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // 系统 DPI 变化（只会送到 DPI-aware 窗口）。
                self.recreate_fonts_all();
                let content = self.content();
                self.refresh_layouts(true, &content);
                LRESULT(0)
            }
            WM_QUERYENDSESSION => LRESULT(1), // 注销/关机：允许结束会话
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    // 会话真的要结束了：趁窗口都活着主动摘托盘，否则图标宿主
                    // 消失后会残留在通知区（下一次登录才清掉，blue 同款）。
                    crate::dblog::log("session ending -> remove tray icon");
                    self.shutting_down.store(true, Ordering::Release);
                    self.remove_tray_icon();
                    unsafe {
                        PostQuitMessage(0);
                    }
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }
}

/// 对外契约入口（main.rs 调 `strip::run`）。
pub fn run(
    settings: Arc<RwLock<Settings>>,
    store: Arc<SnapshotStore>,
    poll: PollHandle,
) -> i32 {
    App::run_loop(settings, store, poll)
}

/// 释放实例的窗口与字体（从集合摘除后在锁外调用）。
fn destroy_instance_window(inst: &mut StripInstance) {
    if inst.hwnd != 0 && unsafe { IsWindow(hw(inst.hwnd)) }.as_bool() {
        unsafe {
            let _ = DestroyWindow(hw(inst.hwnd));
        }
        inst.hwnd = 0;
    }
    delete_instance_fonts(inst);
}

fn delete_instance_fonts(inst: &mut StripInstance) {
    paint::delete_font(&mut inst.name_font);
    paint::delete_font(&mut inst.pill_font);
}

/// 按 DPI × 字号档重建实例字体：**先删旧再建新**，GDI 句柄不泄漏。
fn recreate_fonts(inst: &mut StripInstance, dpi: i32, text_pct: u32) {
    delete_instance_fonts(inst);
    let m = Metrics::with_text_pct(dpi, text_pct);
    let make_font = if inst.embedded { paint::create_font } else { paint::create_panel_font };
    inst.name_font = make_font(-m.st(NAME_FONT_PT), 700).0 as i64;
    inst.pill_font = make_font(-m.st(PILL_FONT_PT), 600).0 as i64;
    inst.dpi = dpi;
}

/// 计算某实例的条内容布局：构建段 → DC 实测宽度 → `+n` 折叠。
/// 预算 = `LEFT_INSET → 托盘左沿 - GAP_TRAY`（几何拿不到时退到工作区 70%），
/// **永不越界遮挡托盘**由折叠 + placement 双重钳制保证。
fn compute_layout(
    inst: &StripInstance,
    content: &Content,
    m: &Metrics,
    avail_h: i32,
) -> StripLayout {
    if inst.hwnd == 0 || !content.any_enabled {
        return StripLayout::default();
    }
    let theme = theme::detect();
    let mut segs = layout::build_segments(
        &content.rows,
        &theme,
        content.low_threshold,
        content.style,
        avail_h,
        m,
    );

    if content.rows.is_empty() && content.any_enabled {
        segs = vec![SegmentText { provider: ProviderId::Mimo, dot: theme.dot_off,
            spans: vec![("拖入供应商".into(),theme.fg_dim)], chip: None }];
    }

    // The graphical style keeps each provider's artwork while fetching. Only
    // the legacy text style collapses the initial wait state into one message.
    if !content.any_content && content.in_grace && content.style == StripStyle::Text {
        let provider = content
            .rows
            .first()
            .map(|r| r.provider)
            .unwrap_or(ProviderId::Mimo);
        segs = vec![SegmentText {
            provider,
            dot: theme.dot_off,
            spans: vec![("获取中…".to_string(), theme.fg_dim)],
            chip: None,
        }];
    }

    let budget_full = match taskbar_client_geometry(hw(inst.taskbar)) {
        Some((tb, tray)) => width_budget(tb, tray, m),
        None => monitor_work_area(hw(inst.taskbar))
            .map(|wa| ((wa.right - wa.left) * 70) / 100)
            .unwrap_or_else(|| m.st(400)),
    };
    let budget_full = inst.lane.map(|(a, b)| b - a).unwrap_or(budget_full);
    let budget = (budget_full - m.st(MARGIN_X) * 2).max(m.st(MIN_STRIP_WIDTH));

    // DC 实测段宽：text 段 = 色点 + 片段；chips 段 = 整个盒宽。
    // 段间分隔（fold 的 `sep`）：chips = 纯间隙 CHIP_GAP（不画 ` · `），
    // text = 实测 ` · ` 宽——paint 侧同源判定，保证测/画一致。
    let dc = unsafe { GetDC(HWND::default()) };
    let name_f = HGDIOBJ(inst.name_font as _);
    let mut widths: Vec<i32> = Vec::with_capacity(segs.len());
    let sep_w;
    unsafe {
        let old = SelectObject(dc, name_f);
        sep_w = match content.style {
            StripStyle::Chips => m.sc(CHIP_GAP),
            StripStyle::Text => text_size(dc, SEP_TEXT).0,
        };
        SelectObject(dc, old);
        for s in &segs {
            let oldn = SelectObject(dc, name_f);
            let w = if let Some(chip) = &s.chip {
                measure_chip(dc, m, chip).width
            } else {
                let mut w = m.st(DOT) + m.st(DOT_GAP);
                for (text, _) in &s.spans {
                    w += text_size(dc, text).0;
                }
                w
            };
            SelectObject(dc, oldn);
            widths.push(w);
        }
        let _ = ReleaseDC(HWND::default(), dc);
    }
    let pill_w = m.st(OVERFLOW_PILL_MIN);
    let (visible, overflow, inner_w) = layout::fold_segments(&widths, sep_w, budget, pill_w);
    let mut x = m.st(MARGIN_X);
    let mut drag_targets = Vec::new();
    for (index, segment) in segs.iter().take(visible).enumerate() {
        if index > 0 { x += sep_w; }
        let (left, size) = if segment.chip.is_some() { (x + m.st(CHIP_PAD_X), m.sc(TILE)) }
            else { (x, m.st(DOT).max(m.sc(16))) };
        if !content.rows.is_empty() {
            drag_targets.push((segment.provider, RECT { left, right: left + size,
                top: (avail_h - size) / 2, bottom: (avail_h + size) / 2 }));
        }
        x += widths[index];
    }
    StripLayout {
        drag_targets,
        segs: segs.into_iter().take(visible).collect(),
        overflow,
        width: (inner_w + m.st(MARGIN_X) * 2).max(m.st(MIN_STRIP_WIDTH)),
    }
}

/// chips 盒的内部几何（`compute_layout` 测宽与 `paint_strip` 布局**共用**，
/// 保证测出来的宽度就是画出来的宽度）。
struct ChipGeom {
    /// 行区左缘相对盒左缘的偏移（pad_x + tile + tile_gap）。
    row_x: i32,
    /// 标签列宽（多行：所有行取最宽统一；单行：label 宽）。
    label_w: i32,
    /// 数值列宽（多行：右对齐基准；单行：text 宽）。
    text_w: i32,
    /// 血条列宽（单行盒 = 0）。
    bar_w: i32,
    /// 盒总宽。
    width: i32,
}

/// 测量一个 chips 盒（**调用方需已选入名字体**）：
/// 盒宽 = 左右内边距 + tile + 间距 + 行区宽；
/// 行区多行 = `标签列 | 血条列 | 数值列`（列间距 `CHIP_COL_GAP`，血条列
/// 设计宽 `CHIP_BAR_W` 弹性段），单行 = 内容整体（label+空格+text）。
fn measure_chip(dc: HDC, m: &Metrics, chip: &layout::ChipBadge) -> ChipGeom {
    let row_x = m.st(CHIP_PAD_X) + m.sc(TILE) + m.st(CHIP_TILE_GAP);
    if chip.single_row() {
        let row = &chip.rows[0];
        let (lw, _) = text_size(dc, &row.label);
        let (tw, _) = text_size(dc, &row.text);
        let space = if row.label.is_empty() {
            0
        } else {
            text_size(dc, " ").0
        };
        ChipGeom {
            row_x,
            label_w: lw,
            text_w: tw,
            bar_w: 0,
            width: row_x + lw + space + tw + m.st(CHIP_PAD_X),
        }
    } else {
        let mut label_w = 0i32;
        let mut text_w = 0i32;
        for row in &chip.rows {
            label_w = label_w.max(text_size(dc, &row.label).0);
            text_w = text_w.max(text_size(dc, &row.text).0);
        }
        let bar_w = m.st(CHIP_BAR_W);
        let inner = label_w + m.st(CHIP_COL_GAP) + bar_w + m.st(CHIP_COL_GAP) + text_w;
        ChipGeom {
            row_x,
            label_w,
            text_w,
            bar_w,
            width: row_x + inner + m.st(CHIP_PAD_X),
        }
    }
}

// ---------------------------------------------------------------------------
/// 隐藏顶层 helper 窗（复用 strip 窗口类，wndproc 按 hwnd 分流）：
/// 消息-only 窗（HWND_MESSAGE）和 WS_CHILD 条都收不到
/// TaskbarCreated / WM_DISPLAYCHANGE / WM_SETTINGCHANGE 广播，所以必须是
/// **真正的顶层 WS_POPUP**（blue 原注释照搬）。约定：
/// - 从不 ShowWindow（不可见）；
/// - WS_EX_TOOLWINDOW：不进任务栏 / Alt-Tab；
/// - WS_EX_NOACTIVATE：不抢焦点。
/// 它同时是 1s 心跳定时器、500ms 重建重试定时器与托盘图标的宿主。
fn create_helper_window(hinst: HINSTANCE) -> HWND {
    unsafe {
        let helper = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
            w!("QuotaBarStrip"),
            w!("QuotaBarHelper"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        )
        .unwrap_or_default();
        if helper.0.is_null() {
            crate::dblog::log("helper window: creation failed");
        } else {
            crate::dblog::log("helper window: hidden top-level popup (broadcasts + tick + tray)");
        }
        helper
    }
}

// ---------------------------------------------------------------------------
/// strip / helper 窗口过程：按 hwnd 分流（panel 有自己的 `panel_wndproc`）。
/// 分流不到的 hwnd（正在销毁的实例、创建期间的早鸟消息）→ DefWindowProc。
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(app) = APP.get() {
        if app.helper_hwnd() == hwnd {
            return app.handle_helper_message(hwnd, msg, wparam, lparam);
        }
        if app.instance_index(hwnd).is_some() {
            return app.handle_strip_message(hwnd, msg, wparam, lparam);
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ---------------------------------------------------------------------------
/// 开机自启：HKCU\...\Run 的 `QuotaBar` 值（blue :2786-2807 模式）。
/// exe 路径用 `GetModuleFileNameW` 取（相对 blue 的 `current_exe()` 更原生，
/// 也避免重解析路径差异），加引号防空格路径被拆。
fn set_autostart(enabled: bool) {
    unsafe {
        let run = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, run, 0, KEY_SET_VALUE, &mut key).0 != 0 {
            crate::dblog::log("autostart: RegOpenKeyExW failed");
            return;
        }
        if enabled {
            let mut buf = vec![0u16; 1024];
            let n = GetModuleFileNameW(HMODULE::default(), &mut buf) as usize;
            if n > 0 && n < buf.len() {
                let path = String::from_utf16_lossy(&buf[..n]).replace('/', "\\");
                let value = format!("\"{path}\"");
                let mut bytes: Vec<u8> = value
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(|u| u.to_le_bytes())
                    .collect();
                let _ = RegSetValueExW(key, w!("QuotaBar"), 0, REG_SZ, Some(&mut bytes));
                crate::dblog::log(&format!("autostart: set -> {value}"));
            } else {
                crate::dblog::log("autostart: GetModuleFileNameW failed");
            }
        } else {
            let _ = RegDeleteValueW(key, w!("QuotaBar"));
            crate::dblog::log("autostart: removed");
        }
        let _ = RegCloseKey(key);
    }
}

// ---------------------------------------------------------------------------
// 纯策略单测：可见性 / WM_DESTROY 决策（blue policy_tests 的额度多屏版）。
#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn screens_group_providers_in_order_without_coordinate_placement() {
        let content = Content { rows: vec![ProviderSnapshot::new(ProviderId::Mimo),ProviderSnapshot::new(ProviderId::Codex)],
            low_threshold: 10,text_pct:100,dock_left:true,strip_visible:true,show_all_monitors:true,
            style:StripStyle::Chips,any_enabled:true,any_content:true,in_grace:false };
        let mut settings = Settings::default();
        settings.provider_positions.insert("codex".into(),crate::config::ProviderPosition { device:"second".into(),x_fraction:0.999 });
        settings.swap_providers(ProviderId::Mimo,ProviderId::Codex);
        let devices = vec!["first".into(),"second".into()];
        assert_eq!(screen_content(&content,&settings,"first",&devices).rows.len(),1);
        let rows = screen_content(&content,&settings,"second",&devices).rows;
        assert_eq!(rows[0].provider,ProviderId::Codex);
        assert_eq!(rows[1].provider,ProviderId::Mimo);
        assert_eq!(screen_content(&content,&settings,"first",&["first".into()]).rows.len(),2);
    }

    #[test]
    fn moving_one_provider_keeps_its_accounts_together_and_others_in_shared_strip() {
        let mut account1 = ProviderSnapshot::new(ProviderId::Codex);
        account1.row_key = "codex:a".into();
        let mut account2 = ProviderSnapshot::new(ProviderId::Codex);
        account2.row_key = "codex:b".into();
        let other = ProviderSnapshot::new(ProviderId::Mimo);
        let content = Content { rows: vec![account1, account2, other], low_threshold: 10,
            text_pct: 100, dock_left: true, strip_visible: true, show_all_monitors: true,
            style: StripStyle::Chips, any_enabled: true, any_content: true, in_grace: false };
        let moved = local_content(&content, Some(ProviderId::Codex), &[ProviderId::Codex]);
        assert_eq!(moved.rows.len(), 2);
        assert!(moved.rows.iter().all(|row| row.provider == ProviderId::Codex));
        let shared = local_content(&content, None, &[ProviderId::Codex]);
        assert_eq!(shared.rows.len(), 1);
        assert_eq!(shared.rows[0].provider, ProviderId::Mimo);
        // An unavailable destination is removed from the active-move list.
        assert_eq!(local_content(&content, None, &[]).rows.len(), 3);
    }

    #[test]
    fn hidden_preference_never_shows_strip() {
        // 用户隐藏的条，嵌入/heal/重建都不得翻出来（blue 同款纪律）。
        assert_eq!(strip_visibility(false, true, true, true), StripVisibility::Hidden);
        assert_eq!(strip_visibility(false, true, false, false), StripVisibility::Hidden);
    }

    #[test]
    fn no_enabled_providers_hides_strip_but_tray_stays() {
        assert_eq!(strip_visibility(true, false, false, true), StripVisibility::Hidden);
        // 有内容也一样：没有启用项就没有内容来源
        assert_eq!(strip_visibility(true, false, true, true), StripVisibility::Hidden);
    }

    #[test]
    fn content_shows_and_grace_covers_first_start() {
        // 正常：有数据 / 有错误 → 显示
        assert_eq!(strip_visibility(true, true, true, false), StripVisibility::Shown);
        // 首启 60s 宽限内：无内容也显示（画"获取中…"）
        assert_eq!(strip_visibility(true, true, false, true), StripVisibility::Shown);
        // 超过宽限期仍全空（poll 一轮都没回来）→ 隐藏、保留托盘
        assert_eq!(strip_visibility(true, true, false, false), StripVisibility::Hidden);
    }

    /// WM_DESTROY 决策（多屏版）：非主动销毁一律调度重建；主动退出绝不重建。
    /// 与 blue 的差异说明见 `should_recreate_after_destroy` 注释。
    #[test]
    fn destroy_rebuild_decision_follows_shutdown_flag_only() {
        assert!(should_recreate_after_destroy(false));
        assert!(!should_recreate_after_destroy(true));
    }
}
