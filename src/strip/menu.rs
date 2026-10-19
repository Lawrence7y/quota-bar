//! 右键菜单：TrackPopupMenu 框架 + 中文菜单项（移植 blue `show_menu` 的
//! 结构：动态字符串用 keep 缓冲保活、`TPM_RETURNCMD` 直接返回命令 id、
//! 关闭后 `WM_NULL` 唤醒外层消息循环）。本文件只负责"建菜单 + 取命令"，
//! 命令的执行（save → kick → dirty …）在 `mod.rs`。
//!
//! 菜单结构（**供应商管理只列已添加的**；全部供应商只在「添加供应商」里出现）：
//! ```text
//! 显示额度条 / 位置 / 多屏显示 / 字号 / 样式 / 轮询间隔 / 立即刷新
//! ─────────────────────────────────────────────
//! 供应商管理 ▸                     ← 仅**已添加**的供应商
//!     小米 MiMo ▸
//!         ✓ 在状态栏显示            （provider 总开关）
//!         ─────
//!         ✓ 主号 / ✓ 备用           （逐账号显示勾选 → 条/面板/告警）
//!         ─────
//!         ＋ 添加账号…              （API key/Cookie 弹输入框；OAuth 走登录）
//!         移除账号… / 移除账号 ▸
//!     …
//! 添加供应商 ▸                     ← 全表供应商（✓ = 已添加，点开＝再加一个账号）
//! ─────────────────────────────────────────────
//! 低额度气泡通知 / 开机自启 / 退出
//! ```

use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, DestroyMenu, GA_ROOT, GetAncestor,
    GetCursorPos, HMENU, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MENU_ITEM_FLAGS, PostMessageW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TRACK_POPUP_MENU_FLAGS, TrackPopupMenu, WM_NULL,
};

use crate::config::{account_key, should_show_account, should_show_on, Settings, StripStyle};
use crate::model::{CredentialKind, ProviderId};
use crate::oauth::AccountInfo;
use crate::providers;
use crate::strip::geometry::DisplayInfo;

// ---------------------------------------------------------------------------
// 菜单 id（字号/位置/多屏/轮询/样式五组是连续区段，供 CheckMenuRadioItem 用）。

pub const MID_TOGGLE: usize = 1000;
pub const MID_REFRESH: usize = 1002;
pub const MID_AUTOSTART: usize = 1006;
pub const MID_NOTIFY: usize = 1007;
pub const MID_EXIT: usize = 1008;
pub const MID_POS_LEFT: usize = 1011;
pub const MID_POS_RIGHT: usize = 1012;
pub const MID_FONT_S: usize = 1020;
pub const MID_FONT_M: usize = 1021;
pub const MID_FONT_L: usize = 1022;
pub const MID_FONT_XL: usize = 1023;
pub const MID_MON_ALL: usize = 1030;
pub const MID_MON_PRIMARY: usize = 1031;
pub const MID_INT30: usize = 1040;
pub const MID_INT60: usize = 1041;
pub const MID_INT300: usize = 1042;
/// 「样式」radio 连续区段：紧凑图文 / 平铺数字。
pub const MID_STYLE_CHIPS: usize = 1060;
pub const MID_STYLE_TEXT: usize = 1061;
/// 「显示器n」勾选项连续段基址（槽 1100..1107）。
pub const MID_DISP_BASE: usize = 1100;
/// 显示器勾选上限：物理屏 ≥8 块时按枚举序截断（8 屏以上极罕见，够用）。
pub const MAX_MENU_DISPLAYS: usize = 8;
/// 每 provider 的**账号槽位**上限（显示勾选段与移除段共用布局）：
/// 超出的账号从菜单截断（CLI `quota-bar accounts` 仍看得到全部）。
pub const MAX_MENU_ACCOUNTS: usize = 6;
/// provider 总开关基址：`MID_PROV_BASE + ProviderId::ALL 下标`
/// （仅**已添加**的 provider 才会以该 id 发出 —— 供应商管理只列已添加的）。
pub const MID_DATA_BASE: usize = 3500;
pub const MID_PASTE_BASE: usize = 3600;
pub const MID_RESET_POS_BASE: usize = 3700;
pub const MID_RESET_POS_ALL: usize = 1070;
pub const MID_SCREEN_BASE: usize = 3800;
pub const MID_SCREEN_AUTO_BASE: usize = 4000;
pub const MID_ORDER_BASE: usize = 4100;
pub const MID_PROV_BASE: usize = 3100;
/// 「添加账号」子菜单项基址：`MID_ADD_BASE + ALL 下标`（全表供应商，与是否已添加
/// 无关 —— 点击弹凭据输入框 / OAuth 登录，语义 = 再加一个账号）。
pub const MID_ADD_BASE: usize = 3200;
/// 账号显示勾选基址：`MID_ACC_BASE + 下标*MAX_MENU_ACCOUNTS + 账号槽`。
pub const MID_ACC_BASE: usize = 3300;
/// 移除账号基址：`MID_DEL_BASE + 下标*MAX_MENU_ACCOUNTS + 账号槽`。
pub const MID_DEL_BASE: usize = 3400;

/// 字号档位：菜单 id → 百分比（80/100/120/140，id 连续）。
pub const FONT_PRESETS: [(usize, u32); 4] = [
    (MID_FONT_S, 80),
    (MID_FONT_M, 100),
    (MID_FONT_L, 120),
    (MID_FONT_XL, 140),
];

/// 菜单命令（由 `mod.rs::on_menu_cmd` 执行）。
/// 注：`AccountDisplay`/`DisplaySet` 携带 Vec，故只有 Clone 不能 Copy。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuCmd {
    ToggleVisible,
    Dock(bool),
    /// true = 所有屏。
    Monitors(bool),
    Font(u32),
    /// 条视觉样式（chips / text）——切换不改字体，只 save+kick+dirty。
    Style(StripStyle),
    /// 轮询间隔秒。
    Interval(u64),
    Refresh,
    /// (provider, 目标启用状态) —— provider 总开关。
    Provider(ProviderId, bool),
    /// 「添加账号」：API key / Cookie 弹输入框；OAuth 走浏览器登录。
    AddAccount(ProviderId),
    PasteCookie(ProviderId),
    DisplayMode(ProviderId, crate::config::DisplayMode),
    ResetPosition(Option<ProviderId>),
    ProviderScreen(ProviderId, Option<String>),
    ProviderOrder(ProviderId, i32),
    /// 「移除账号」：删该账号（OAuth 账号表 / keyring 凭据 + 配置）。
    RemoveAccount(ProviderId, String),
    /// 账号显示白名单整体替换（菜单按当前勾选态算出的新列表 → `Some(vec)`；
    /// 空表照实生效 = 全部账号行隐藏）。
    AccountDisplay(Vec<String>),
    /// 显示器白名单整体替换（菜单按当前勾选态算出的新集合 → `Some(vec)`）。
    DisplaySet(Vec<String>),
    Notify,
    Autostart,
    Exit,
}

// ---------------------------------------------------------------------------
fn checked_flag(on: bool) -> MENU_ITEM_FLAGS {
    MENU_ITEM_FLAGS(MF_STRING.0 | if on { MF_CHECKED.0 } else { 0 })
}

fn append_item(menu: HMENU, id: usize, text: PCWSTR, checked: bool) {
    unsafe {
        let _ = AppendMenuW(menu, checked_flag(checked), id, text);
    }
}

fn append_popup(parent: HMENU, child: HMENU, text: PCWSTR) {
    unsafe {
        let _ = AppendMenuW(parent, MF_POPUP, child.0 as usize, text);
    }
}

/// 动态字符串条目（显示名/账号等非 `w!` 字面量）：UTF-16 存进 `keep`
/// 保活到 TrackPopupMenu 返回（blue 同款手法）。
fn append_flags(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str, keep: &mut Vec<Vec<u16>>) {
    keep.push(text.encode_utf16().chain(std::iter::once(0)).collect());
    let p = PCWSTR(keep.last().unwrap().as_ptr());
    unsafe {
        let _ = AppendMenuW(menu, flags, id, p);
    }
}

/// 灰字占位条目（id=0，点不到）。
fn append_placeholder(menu: HMENU, text: &str, keep: &mut Vec<Vec<u16>>) {
    append_flags(
        menu,
        MENU_ITEM_FLAGS(MF_STRING.0 | MF_GRAYED.0),
        0,
        text,
        keep,
    );
}

fn preset_label(pct: u32) -> PCWSTR {
    match pct {
        80 => w!("80%"),
        100 => w!("100%"),
        120 => w!("120%"),
        _ => w!("140%"),
    }
}

/// Direct footer controls reuse the existing commands and persistence path.
pub fn show_preference(owner: HWND, cfg: &Settings, font: bool) -> Option<MenuCmd> {
    unsafe {
        let root = CreatePopupMenu().ok()?;
        if font {
            for (id, pct) in FONT_PRESETS {
                append_item(root,id,preset_label(pct),cfg.text_pct()==pct);
            }
        } else {
            for (id, seconds, label) in [(MID_INT30,30,w!("30秒")),
                (MID_INT60,60,w!("60秒")),(MID_INT300,300,w!("5分钟"))] {
                append_item(root,id,label,cfg.poll_interval().as_secs()==seconds);
            }
        }
        let mut pt=POINT::default(); let _=GetCursorPos(&mut pt);
        let _=SetForegroundWindow(owner);
        let selected=TrackPopupMenu(root,TRACK_POPUP_MENU_FLAGS(TPM_RETURNCMD.0),
            pt.x,pt.y,0,owner,None);
        let _=DestroyMenu(root);
        let _=PostMessageW(owner,WM_NULL,WPARAM(0),LPARAM(0));
        if selected.as_bool() { map_id(selected.0 as usize,cfg,&[],&[]) } else { None }
    }
}

fn provider_icon(menu: HMENU, position: u32, provider: ProviderId) {
    use windows::Win32::UI::WindowsAndMessaging::{MENUITEMINFOW, MIIM_BITMAP, SetMenuItemInfoW};
    let Some(bitmap) = super::logos::logo_bitmap(provider,20) else { return; };
    let info=MENUITEMINFOW { cbSize:std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask:MIIM_BITMAP,hbmpItem:bitmap,..Default::default() };
    unsafe { let _=SetMenuItemInfoW(menu,position,true,&info); }
}

/// 「添加账号」条目文案：OAuth = 登录；Cookie = 平台 Cookie；其余 = API key。
fn add_account_label(id: ProviderId) -> String {
    match id.credential_kind() {
        CredentialKind::OAuth => format!("＋ 登录新账号（{}）", id.display_name()),
        CredentialKind::Cookie => format!("＋ 添加账号…（{} · 网页登录）", id.display_name()),
        CredentialKind::ApiKey => format!("＋ 添加账号…（{}）", id.display_name()),
    }
}

/// 账号菜单文案：展示名（邮箱 / 自定义名 / key 尾号），活动账号追加**纯文本**
/// ` ●`（仅作显示标记，不再用 radio 切活动）。
fn account_item_label(acc: &AccountInfo) -> String {
    let name = acc
        .email
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("（未命名账号）");
    if acc.active {
        format!("{name} ●")
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
/// 构建并弹出菜单，返回选中的命令（取消 / 点空白 → None）。
///
/// `owner` = 触发菜单的窗口（strip 实例或 helper 托盘窗口）。菜单要能靠
/// 点击外部自动收起，因此前景窗口设为 owner 的**根窗口**（嵌入条的根是
/// 任务栏——blue 同款处理；helper 自身就是根）。
/// `accounts` = 打开菜单前实时取的**每 provider 账号表**快照
/// （`accounts::list`；只对已添加的 provider 有内容）；
/// `displays` = 打开菜单前实时 `detect_displays()` 的快照（热插拔后仍准）。
pub fn show(
    owner: HWND,
    cfg: &Settings,
    accounts: &[(ProviderId, Vec<AccountInfo>)],
    displays: &[DisplayInfo],
) -> Option<MenuCmd> {
    unsafe {
        let Ok(root) = CreatePopupMenu() else {
            return None;
        };
        let mut keep: Vec<Vec<u16>> = Vec::new();
        // 全表配置状态（13 次 keyring/env 判定）在**建菜单前**算好：同一份判定既
        // 决定「供应商管理」列谁、也给「添加供应商」打 ✓，口径永远一致。
        let configured: Vec<bool> = ProviderId::ALL
            .iter()
            .map(|id| providers::is_configured(cfg, *id))
            .collect();
        if !build(root, cfg, &configured, accounts, displays, &mut keep) {
            let _ = DestroyMenu(root);
            return None;
        }

        // 菜单需要一个前台所有者才能在点击外部时收起。
        let root_hwnd = GetAncestor(owner, GA_ROOT);
        if !root_hwnd.0.is_null() {
            let _ = SetForegroundWindow(root_hwnd);
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let ret = TrackPopupMenu(
            root,
            TRACK_POPUP_MENU_FLAGS(TPM_RIGHTBUTTON.0 | TPM_RETURNCMD.0),
            pt.x,
            pt.y,
            0,
            owner,
            None,
        );
        let _ = DestroyMenu(root);
        // TrackPopupMenu 进了自己的模态循环；发个 WM_NULL 唤醒外层消息循环，
        // 保证菜单打开期间排进队列的消息（收起/点击）被可靠取走。
        let _ = PostMessageW(owner, WM_NULL, WPARAM(0), LPARAM(0));
        if ret.as_bool() {
            map_id(ret.0 as usize, cfg, accounts, displays)
        } else {
            None
        }
    }
}

/// 建菜单（不含弹出）：`show` 的纯构建半部 —— **可离线测试**
/// （测试建完直接用 `GetMenuItemCount/GetMenuStringW/GetMenuState` 回读结构）。
/// `configured` = 全表「已添加」判定（由调用方算好传入，测试可完全注入 ——
/// 不再读真实 keyring）；`keep` 必须活到菜单销毁（动态文案的 UTF-16 缓冲）。
/// 返回 false = 某个 `CreatePopupMenu` 失败（调用方销毁 root）。
fn build(
    root: HMENU,
    cfg: &Settings,
    configured: &[bool],
    accounts: &[(ProviderId, Vec<AccountInfo>)],
    displays: &[DisplayInfo],
    keep: &mut Vec<Vec<u16>>,
) -> bool {
    unsafe {
        // 显示/隐藏
        append_item(root, MID_TOGGLE, w!("显示额度条"), cfg.strip_visible);

        // 位置（左侧|右侧，radio）
        let Ok(m_pos) = CreatePopupMenu() else {
            return false;
        };
        append_item(m_pos, MID_POS_LEFT, w!("左侧"), cfg.dock_left());
        append_item(m_pos, MID_POS_RIGHT, w!("右侧"), !cfg.dock_left());
        let _ = CheckMenuRadioItem(
            m_pos,
            MID_POS_LEFT as u32,
            MID_POS_RIGHT as u32,
            if cfg.dock_left() { MID_POS_LEFT } else { MID_POS_RIGHT } as u32,
            MF_BYCOMMAND.0,
        );
        append_popup(root, m_pos, w!("位置"));

        // 多屏（全部屏|仅主屏，radio）
        let Ok(m_mon) = CreatePopupMenu() else {
            return false;
        };
        append_item(m_mon, MID_MON_ALL, w!("全部屏"), cfg.show_all_monitors);
        append_item(m_mon, MID_MON_PRIMARY, w!("仅主屏"), !cfg.show_all_monitors);
        let _ = CheckMenuRadioItem(
            m_mon,
            MID_MON_ALL as u32,
            MID_MON_PRIMARY as u32,
            if cfg.show_all_monitors { MID_MON_ALL } else { MID_MON_PRIMARY } as u32,
            MF_BYCOMMAND.0,
        );
        // ── 每块物理屏一个勾选项（实时探测，每次打开重枚举，热插拔后仍准）：
        // 勾选态 = `should_show_on` 门控的当前生效值——None 模式 all→全勾 /
        // 仅主屏→只勾主屏；Some 模式→按值（主屏同样受管）。
        let _ = AppendMenuW(m_mon, MF_SEPARATOR, 0, PCWSTR::null());
        if displays.is_empty() {
            append_placeholder(m_mon, "未检测到显示器", keep);
        } else {
            for (i, d) in displays.iter().take(MAX_MENU_DISPLAYS).enumerate() {
                let checked =
                    should_show_on(&cfg.display_devices, &d.device, d.primary, cfg.show_all_monitors);
                append_flags(m_mon, checked_flag(checked), MID_DISP_BASE + i, &display_label(i, d), keep);
            }
        }
        append_popup(root, m_mon, w!("多屏显示"));
        append_item(root, MID_RESET_POS_ALL, w!("恢复默认排序和显示屏"), false);

        // 字号（80/100/120/140，radio）
        let Ok(m_font) = CreatePopupMenu() else {
            return false;
        };
        let cur = cfg.text_pct();
        let mut font_id = MID_FONT_M;
        for (id, pct) in FONT_PRESETS {
            append_item(m_font, id, preset_label(pct), cur == pct);
            if cur == pct {
                font_id = id;
            }
        }
        let _ = CheckMenuRadioItem(
            m_font,
            MID_FONT_S as u32,
            MID_FONT_XL as u32,
            font_id as u32,
            MF_BYCOMMAND.0,
        );
        append_popup(root, m_font, w!("字号"));

        // 样式（紧凑图文 chips / 平铺数字 text，radio）
        let Ok(m_style) = CreatePopupMenu() else {
            return false;
        };
        let cur_style = cfg.style();
        append_item(
            m_style,
            MID_STYLE_CHIPS,
            w!("紧凑图文(chips)"),
            cur_style == StripStyle::Chips,
        );
        append_item(
            m_style,
            MID_STYLE_TEXT,
            w!("平铺数字(text)"),
            cur_style == StripStyle::Text,
        );
        let _ = CheckMenuRadioItem(
            m_style,
            MID_STYLE_CHIPS as u32,
            MID_STYLE_TEXT as u32,
            if cur_style == StripStyle::Chips {
                MID_STYLE_CHIPS
            } else {
                MID_STYLE_TEXT
            } as u32,
            MF_BYCOMMAND.0,
        );
        append_popup(root, m_style, w!("样式"));

        // 轮询间隔（30s / 1min / 5min，radio）
        let Ok(m_int) = CreatePopupMenu() else {
            return false;
        };
        append_item(m_int, MID_INT30, w!("30 秒"), cfg.poll_seconds == 30);
        append_item(m_int, MID_INT60, w!("1 分钟"), cfg.poll_seconds == 60);
        append_item(m_int, MID_INT300, w!("5 分钟"), cfg.poll_seconds == 300);
        let int_id = match cfg.poll_seconds {
            30 => MID_INT30,
            300 => MID_INT300,
            _ => MID_INT60,
        };
        let _ = CheckMenuRadioItem(
            m_int,
            MID_INT30 as u32,
            MID_INT300 as u32,
            int_id as u32,
            MF_BYCOMMAND.0,
        );
        append_popup(root, m_int, w!("轮询间隔"));

        append_item(root, MID_REFRESH, w!("立即刷新"), false);
        let _ = AppendMenuW(root, MF_SEPARATOR, 0, PCWSTR::null());

        // ── 供应商管理：**只列已添加的** provider；每家一个子菜单，内含
        //    provider 总开关 + 逐账号显示勾选 + 添加/移除账号。
        let Ok(m_manage) = CreatePopupMenu() else {
            return false;
        };
        let manage_indices = configured_indices(&configured);
        for i in manage_indices.iter().copied() {
            let id = &ProviderId::ALL[i];
            let Ok(m_prov) = CreatePopupMenu() else {
                return false;
            };
            append_item(
                m_prov,
                MID_PROV_BASE + i,
                w!("在状态栏显示"),
                cfg.is_enabled(*id),
            );
            let _ = AppendMenuW(m_prov, MF_SEPARATOR, 0, PCWSTR::null());
            let accs = provider_accounts(accounts, *id);
            if accs.is_empty() {
                append_placeholder(m_prov, "（暂无账号）", keep);
            } else {
                for (slot, acc) in accs.iter().take(MAX_MENU_ACCOUNTS).enumerate() {
                    let checked = should_show_account(&cfg.display_accounts, *id, &acc.id);
                    append_flags(
                        m_prov,
                        checked_flag(checked),
                        MID_ACC_BASE + i * MAX_MENU_ACCOUNTS + slot,
                        &account_item_label(acc),
                        keep,
                    );
                }
            }
            let _ = AppendMenuW(m_prov, MF_SEPARATOR, 0, PCWSTR::null());
            append_flags(
                m_prov,
                MENU_ITEM_FLAGS(MF_STRING.0),
                MID_ADD_BASE + i,
                &add_account_label(*id),
                keep,
            );
            if id.credential_kind() == CredentialKind::Cookie {
                append_flags(m_prov, MENU_ITEM_FLAGS(MF_STRING.0), MID_PASTE_BASE + i,
                    "手动粘贴 Cookie…", keep);
            }
            // 移除账号：单账号 → 直接一项；多账号 → 子菜单逐账号列出。
            if accs.len() == 1 {
                append_flags(
                    m_prov,
                    MENU_ITEM_FLAGS(MF_STRING.0),
                    MID_DEL_BASE + i * MAX_MENU_ACCOUNTS,
                    "移除账号…",
                    keep,
                );
            } else if accs.len() > 1 {
                let Ok(m_del) = CreatePopupMenu() else {
                    return false;
                };
                for (slot, acc) in accs.iter().take(MAX_MENU_ACCOUNTS).enumerate() {
                    append_flags(
                        m_del,
                        MENU_ITEM_FLAGS(MF_STRING.0),
                        MID_DEL_BASE + i * MAX_MENU_ACCOUNTS + slot,
                        &account_item_label(acc),
                        keep,
                    );
                }
                append_popup(m_prov, m_del, w!("移除账号"));
            }
            let Ok(m_data) = CreatePopupMenu() else { return false; };
            for (slot, mode) in crate::config::DisplayMode::ALL.into_iter().enumerate() {
                append_flags(m_data, checked_flag(cfg.display_mode(*id) == mode),
                    MID_DATA_BASE + i * 3 + slot, mode.label(), keep);
            }
            append_popup(m_prov, m_data, w!("显示内容"));
            append_item(m_prov, MID_ORDER_BASE + i*2, w!("排序：向前一位"), false);
            append_item(m_prov, MID_ORDER_BASE + i*2+1, w!("排序：向后一位"), false);
            let Ok(m_screen) = CreatePopupMenu() else { return false; };
            let destination = cfg.provider_positions.get(id.as_str());
            append_item(m_screen, MID_SCREEN_AUTO_BASE+i, w!("跟随多屏显示设置"), destination.is_none());
            for (slot, display) in displays.iter().take(MAX_MENU_DISPLAYS).enumerate() {
                append_flags(m_screen, checked_flag(destination.is_some_and(|pos| pos.device == display.device)),
                    MID_SCREEN_BASE+i*MAX_MENU_DISPLAYS+slot, &display_label(slot,display), keep);
            }
            append_popup(m_prov,m_screen,w!("显示屏"));
            append_item(m_prov, MID_RESET_POS_BASE + i, w!("恢复默认显示屏"), false);
            let prov_label = format!("{}（{}）", id.display_name(), accs.len());
            append_flags(
                m_manage,
                MENU_ITEM_FLAGS(MF_POPUP.0),
                m_prov.0 as usize,
                &prov_label,
                keep,
            );
            provider_icon(m_manage,manage_indices.iter().position(|index| *index==i).unwrap() as u32,*id);
        }
        if manage_indices.is_empty() {
            append_placeholder(m_manage, "（尚未添加供应商）", keep);
        }
        append_popup(root, m_manage, w!("供应商管理"));

        // ── 添加供应商：全部供应商的入口（✓ = 已添加，点开＝再加一个账号 /
        //    重新登录；Cookie / API key 弹输入框，Antigravity / Codex 走浏览器登录）。
        let Ok(m_add) = CreatePopupMenu() else {
            return false;
        };
        for (i, id) in ProviderId::ALL.iter().enumerate() {
            let label = match id.credential_kind() {
                CredentialKind::OAuth => format!("{}（登录）", id.display_name()),
                CredentialKind::Cookie => format!("{}（网页登录）", id.display_name()),
                CredentialKind::ApiKey => id.display_name().to_string(),
            };
            if id.credential_kind() == CredentialKind::Cookie {
                let Ok(m_login) = CreatePopupMenu() else { return false; };
                append_item(m_login, MID_ADD_BASE + i, w!("网页登录（自动保存会话）"), false);
                append_item(m_login, MID_PASTE_BASE + i, w!("手动粘贴 Cookie…"), false);
                append_flags(m_add, MENU_ITEM_FLAGS(checked_flag(configured[i]).0 | MF_POPUP.0),
                    m_login.0 as usize, &label, keep);
            } else {
                append_flags(m_add, checked_flag(configured[i]), MID_ADD_BASE + i, &label, keep);
            }
            provider_icon(m_add,i as u32,*id);
        }
        append_popup(root, m_add, w!("添加供应商"));

        let _ = AppendMenuW(root, MF_SEPARATOR, 0, PCWSTR::null());
        append_item(root, MID_NOTIFY, w!("低额度气泡通知"), cfg.notify_low);
        append_item(root, MID_AUTOSTART, w!("开机自启"), cfg.autostart);
        append_item(root, MID_EXIT, w!("退出"), false);
        true
    }
}

/// 显示器菜单文案：`显示器n · WxH`，主屏缀「（主）」（n = 枚举序）。
fn display_label(idx: usize, d: &DisplayInfo) -> String {
    format!(
        "显示器{} · {}x{}{}",
        d.device.rsplit("DISPLAY").next().and_then(|value| value.parse::<usize>().ok()).unwrap_or(idx+1),
        d.width,
        d.height,
        if d.primary { "（主）" } else { "" }
    )
}

/// 「供应商管理」段的槽位 = **已添加** provider 的 `ALL` 下标（纯函数注入判定，
/// 单测不碰 keyring；顺序保持 ALL 序——`map_id` 按原始下标定位，与过滤无关）。
fn configured_indices(configured: &[bool]) -> Vec<usize> {
    configured
        .iter()
        .enumerate()
        .filter(|(_, c)| **c)
        .map(|(i, _)| i)
        .collect()
}

/// 菜单 id → 命令。provider 总开关的目标状态 = 取反当前状态；
/// 账号/显示器项按下标定位到打开菜单时的实时快照（越界 → None）。
fn map_id(
    id: usize,
    cfg: &Settings,
    accounts: &[(ProviderId, Vec<AccountInfo>)],
    displays: &[DisplayInfo],
) -> Option<MenuCmd> {
    let cmd = match id {
        MID_TOGGLE => MenuCmd::ToggleVisible,
        MID_POS_LEFT => MenuCmd::Dock(true),
        MID_POS_RIGHT => MenuCmd::Dock(false),
        MID_MON_ALL => MenuCmd::Monitors(true),
        MID_MON_PRIMARY => MenuCmd::Monitors(false),
        MID_REFRESH => MenuCmd::Refresh,
        MID_NOTIFY => MenuCmd::Notify,
        MID_AUTOSTART => MenuCmd::Autostart,
        MID_EXIT => MenuCmd::Exit,
        id if FONT_PRESETS.iter().any(|(pid, _)| *pid == id) => {
            let pct = FONT_PRESETS
                .iter()
                .find(|(pid, _)| *pid == id)
                .map(|(_, p)| *p)?;
            MenuCmd::Font(pct)
        }
        MID_STYLE_CHIPS => MenuCmd::Style(StripStyle::Chips),
        MID_STYLE_TEXT => MenuCmd::Style(StripStyle::Text),
        id if (MID_DATA_BASE..MID_DATA_BASE + ProviderId::ALL.len() * 3).contains(&id) => {
            let offset = id - MID_DATA_BASE;
            MenuCmd::DisplayMode(ProviderId::ALL[offset / 3], crate::config::DisplayMode::ALL[offset % 3])
        }
        id if (MID_PASTE_BASE..MID_PASTE_BASE + ProviderId::ALL.len()).contains(&id) => {
            let provider = ProviderId::ALL[id - MID_PASTE_BASE];
            if provider.credential_kind() != CredentialKind::Cookie { return None; }
            MenuCmd::PasteCookie(provider)
        }
        MID_RESET_POS_ALL => MenuCmd::ResetPosition(None),
        id if (MID_RESET_POS_BASE..MID_RESET_POS_BASE + ProviderId::ALL.len()).contains(&id) => {
            MenuCmd::ResetPosition(Some(ProviderId::ALL[id - MID_RESET_POS_BASE]))
        }
        id if (MID_SCREEN_BASE..MID_SCREEN_BASE+ProviderId::ALL.len()*MAX_MENU_DISPLAYS).contains(&id) => {
            let offset = id-MID_SCREEN_BASE;
            MenuCmd::ProviderScreen(ProviderId::ALL[offset/MAX_MENU_DISPLAYS],
                Some(displays.get(offset%MAX_MENU_DISPLAYS)?.device.clone()))
        }
        id if (MID_SCREEN_AUTO_BASE..MID_SCREEN_AUTO_BASE+ProviderId::ALL.len()).contains(&id) =>
            MenuCmd::ProviderScreen(ProviderId::ALL[id-MID_SCREEN_AUTO_BASE], None),
        id if (MID_ORDER_BASE..MID_ORDER_BASE+ProviderId::ALL.len()*2).contains(&id) => {
            let offset = id-MID_ORDER_BASE;
            MenuCmd::ProviderOrder(ProviderId::ALL[offset/2],if offset%2==0 {-1} else {1})
        }
        MID_INT30 => MenuCmd::Interval(30),
        MID_INT60 => MenuCmd::Interval(60),
        MID_INT300 => MenuCmd::Interval(300),
        // 账号显示勾选槽 → 从**当前生效勾选态**取反该账号，算出整份新白名单
        //（None 模式 = 全显示 → 取反即剔除；空表照实生效）。
        id if (MID_ACC_BASE..MID_ACC_BASE + ProviderId::ALL.len() * MAX_MENU_ACCOUNTS)
            .contains(&id) =>
        {
            let (pid, acc_id) = account_slot(cfg, accounts, id - MID_ACC_BASE)?;
            let mut list: Vec<String> = Vec::new();
            for provider in ProviderId::ALL {
                for acc in provider_accounts(accounts, provider) {
                    if should_show_account(&cfg.display_accounts, provider, &acc.id) {
                        list.push(account_key(provider, &acc.id));
                    }
                }
            }
            let key = account_key(pid, &acc_id);
            match list.iter().position(|k| *k == key) {
                Some(pos) => {
                    list.remove(pos);
                }
                None => list.push(key),
            }
            MenuCmd::AccountDisplay(list)
        }
        // 移除账号槽 → 该 provider 的第 slot 个账号（越界 → None）。
        id if (MID_DEL_BASE..MID_DEL_BASE + ProviderId::ALL.len() * MAX_MENU_ACCOUNTS)
            .contains(&id) =>
        {
            let (pid, acc_id) = account_slot(cfg, accounts, id - MID_DEL_BASE)?;
            MenuCmd::RemoveAccount(pid, acc_id)
        }
        // 显示器勾选（1100..1107）→ 从**当前生效勾选态**取反这一块屏，
        // 算出整份新白名单（None 模式也会被折叠成显式 Some）。
        id if (MID_DISP_BASE..MID_DISP_BASE + MAX_MENU_DISPLAYS).contains(&id) => {
            let clicked = displays.get(id - MID_DISP_BASE)?;
            let mut set: Vec<String> = displays
                .iter()
                .filter(|d| should_show_on(&cfg.display_devices, &d.device, d.primary, cfg.show_all_monitors))
                .map(|d| d.device.clone())
                .collect();
            match set.iter().position(|dev| dev == &clicked.device) {
                Some(pos) => {
                    set.remove(pos);
                }
                None => set.push(clicked.device.clone()),
            }
            MenuCmd::DisplaySet(set)
        }
        id if (MID_PROV_BASE..MID_PROV_BASE + ProviderId::ALL.len()).contains(&id) => {
            let target = ProviderId::ALL[id - MID_PROV_BASE];
            MenuCmd::Provider(target, !cfg.is_enabled(target))
        }
        // 「添加供应商」/「添加账号」共用：全表映射（与是否已添加无关——点开即可
        // 再加一个账号 / 重录凭据）。
        id if (MID_ADD_BASE..MID_ADD_BASE + ProviderId::ALL.len()).contains(&id) => {
            MenuCmd::AddAccount(ProviderId::ALL[id - MID_ADD_BASE])
        }
        _ => return None,
    };
    Some(cmd)
}

/// 菜单里某 provider 的账号表（无 → 空切片）。
fn provider_accounts<'a>(
    accounts: &'a [(ProviderId, Vec<AccountInfo>)],
    id: ProviderId,
) -> &'a [AccountInfo] {
    accounts
        .iter()
        .find(|(pid, _)| *pid == id)
        .map(|(_, a)| a.as_slice())
        .unwrap_or(&[])
}

/// 账号槽偏移 → (provider, account_id)：`offset = ALL 下标 * MAX + 账号槽`。
/// 账号表由调用方传入（只含已添加的 provider）——越界/无该槽 → None。
fn account_slot(
    _cfg: &Settings,
    accounts: &[(ProviderId, Vec<AccountInfo>)],
    offset: usize,
) -> Option<(ProviderId, String)> {
    let idx = offset / MAX_MENU_ACCOUNTS;
    let slot = offset % MAX_MENU_ACCOUNTS;
    let pid = *ProviderId::ALL.get(idx)?;
    let acc = provider_accounts(accounts, pid).get(slot)?;
    Some((pid, acc.id.clone()))
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_TEXT_PCT, MAX_TEXT_PCT, MIN_TEXT_PCT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuState, GetMenuItemCount, GetMenuStringW, GetSubMenu, MF_BYPOSITION,
    };

    // ---- 真菜单结构回读（build() 建真 HMENU，逐项读文本/勾选/子菜单） ----

    /// 读一个菜单项文本（按位置）。
    fn item_text(menu: HMENU, pos: i32) -> String {
        let mut buf = vec![0u16; 256];
        let n = unsafe { GetMenuStringW(menu, pos as u32, Some(&mut buf), MF_BYPOSITION) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    /// 该位置的项是否被勾选。
    fn item_checked(menu: HMENU, pos: i32) -> bool {
        let state = unsafe { GetMenuState(menu, pos as u32, MF_BYPOSITION) };
        state & MF_CHECKED.0 != 0
    }

    /// 该位置的项是否是子菜单（返回其 HMENU）。
    fn submenu(menu: HMENU, pos: i32) -> Option<HMENU> {
        let h = unsafe { GetSubMenu(menu, pos) };
        (!h.0.is_null()).then_some(h)
    }

    /// 在菜单里按文本找子菜单。
    fn find_submenu(menu: HMENU, label: &str) -> Option<HMENU> {
        let count = unsafe { GetMenuItemCount(menu) };
        (0..count).find_map(|i| {
            (item_text(menu, i) == label)
                .then(|| submenu(menu, i))
                .flatten()
        })
    }

    /// 菜单项文本列表（跳过分隔符）。
    fn texts(menu: HMENU) -> Vec<String> {
        let count = unsafe { GetMenuItemCount(menu) };
        (0..count)
            .map(|i| item_text(menu, i))
            .filter(|t| !t.is_empty())
            .collect()
    }

    /// 全表「已添加」判定（测试注入：只有列出的下标为 true —— 不读真实 keyring）。
    fn configured_only(ids: &[ProviderId]) -> Vec<bool> {
        ProviderId::ALL
            .iter()
            .map(|p| ids.contains(p))
            .collect()
    }

    /// 真菜单结构自检（用真 HMENU 回读，验证 build 的层级/勾选/文案）：
    /// - 根含「供应商管理」「添加供应商」两个子菜单；
    /// - 供应商管理只列**已添加**的 provider（判定由调用方注入）；
    /// - 添加供应商恒含全表供应商；
    /// - provider 子菜单含总开关 + 逐账号勾选 + 添加/移除。
    #[test]
    fn display_content_submenu_roundtrips_provider_and_selected_mode() {
        let mut cfg = Settings::default();
        cfg.display_modes.insert("mimo".into(), crate::config::DisplayMode::Balance);
        let configured = ProviderId::ALL.iter().map(|p| *p == ProviderId::Mimo).collect::<Vec<_>>();
        let mut keep = Vec::new();
        unsafe {
            let root = CreatePopupMenu().unwrap();
            assert!(build(root, &cfg, &configured, &[], &[], &mut keep));
            let manage = find_submenu(root, "供应商管理").unwrap();
            let provider = find_submenu(manage, &texts(manage)[0]).unwrap();
            let data = find_submenu(provider, "显示内容").unwrap();
            assert_eq!(texts(data), vec!["套餐额度和余额", "仅套餐额度", "仅余额"]);
            assert!(!item_checked(data, 0) && !item_checked(data, 1) && item_checked(data, 2));
            let _ = DestroyMenu(root);
        }
        assert_eq!(map_id(MID_DATA_BASE + 1, &cfg, &[], &[]),
            Some(MenuCmd::DisplayMode(ProviderId::Mimo, crate::config::DisplayMode::Plan)));
        assert_eq!(map_id(MID_PASTE_BASE, &cfg, &[], &[]), Some(MenuCmd::PasteCookie(ProviderId::Mimo)));
    }

    #[test]
    fn built_menu_lists_only_configured_providers_under_manage() {
        let mut cfg = Settings::default();
        let mimo_accs = vec![
            AccountInfo {
                id: "acc-1".into(),
                email: Some("主号".into()),
                active: false,
            },
            AccountInfo {
                id: "acc-2".into(),
                email: Some("备用".into()),
                active: false,
            },
        ];
        let ds_accs = vec![AccountInfo {
            id: "env".into(),
            email: Some("环境变量".into()),
            active: false,
        }];
        cfg.display_accounts = Some(vec![
            account_key(ProviderId::Mimo, "acc-1"),
            account_key(ProviderId::DeepSeek, "env"),
        ]);
        let accounts = vec![
            (ProviderId::Mimo, mimo_accs),
            (ProviderId::DeepSeek, ds_accs),
        ];
        let configured = configured_only(&[ProviderId::Mimo, ProviderId::DeepSeek]);

        let root = unsafe { CreatePopupMenu() }.expect("root 菜单");
        let mut keep: Vec<Vec<u16>> = Vec::new();
        assert!(
            build(root, &cfg, &configured, &accounts, &[], &mut keep),
            "菜单构建成功"
        );

        // 根：两个子菜单都在
        let manage = find_submenu(root, "供应商管理").expect("有「供应商管理」子菜单");
        let add = find_submenu(root, "添加供应商").expect("有「添加供应商」子菜单");
        // 添加供应商 = 全表供应商（与是否已添加无关）
        assert_eq!(
            unsafe { GetMenuItemCount(add) },
            ProviderId::ALL.len() as i32,
            "添加供应商恒列全部 provider"
        );
        assert!(texts(add).iter().any(|t| t.contains("Codex")), "含 Codex 登录项");
        assert!(
            texts(add).iter().any(|t| t.contains("登录")),
            "OAuth 类文案标注（登录）"
        );
        // 供应商管理 = 注入的两家（顺序 = ALL 序：Mimo 在前）
        let manage_texts = texts(manage);
        assert_eq!(manage_texts.len(), 2, "只列已添加的：{manage_texts:?}");
        assert!(manage_texts[0].starts_with("小米 MiMo（2）"), "{manage_texts:?}");
        assert!(manage_texts[1].starts_with("DeepSeek（1）"), "{manage_texts:?}");

        // Mimo 子菜单：总开关 + 两个账号勾选 + 添加账号 + 移除账号（子菜单）
        let mimo = find_submenu(manage, &manage_texts[0]).expect("Mimo 子菜单");
        let mimo_texts = texts(mimo);
        assert_eq!(mimo_texts[0], "在状态栏显示");
        assert!(item_checked(mimo, 0), "provider 默认启用 → 勾选");
        assert_eq!(mimo_texts[1], "主号");
        assert!(item_checked(mimo, 2), "acc-1 在白名单里 → 勾选");
        assert_eq!(mimo_texts[2], "备用");
        assert!(!item_checked(mimo, 3), "acc-2 不在白名单 → 未勾选");
        assert!(
            mimo_texts.iter().any(|t| t.contains("添加账号")),
            "{mimo_texts:?}"
        );
        assert!(mimo_texts.iter().any(|s| s == "移除账号"));
        let del = find_submenu(mimo, "移除账号").expect("多账号 → 移除账号是子菜单");
        assert_eq!(texts(del), vec!["主号".to_string(), "备用".to_string()]);

        // DeepSeek 子菜单：单账号 → 「移除账号…」直接一项（不再套子菜单）
        let ds = find_submenu(manage, &manage_texts[1]).expect("DeepSeek 子菜单");
        let ds_texts = texts(ds);
        assert_eq!(ds_texts[0], "在状态栏显示");
        assert_eq!(ds_texts[1], "环境变量");
        assert!(item_checked(ds, 2), "env 账号在白名单里 → 勾选");
        assert!(ds_texts.iter().any(|s| s == "移除账号…"));

        let _ = unsafe { DestroyMenu(root) };
    }

    /// 全新安装（一家都没添加）：供应商管理只有灰字占位，添加供应商仍列全表。
    #[test]
    fn built_menu_without_any_provider_shows_placeholder() {
        let cfg = Settings::default();
        let root = unsafe { CreatePopupMenu() }.expect("root 菜单");
        let mut keep: Vec<Vec<u16>> = Vec::new();
        assert!(build(root, &cfg, &configured_only(&[]), &[], &[], &mut keep));
        let manage = find_submenu(root, "供应商管理").expect("有「供应商管理」子菜单");
        assert_eq!(texts(manage), vec!["（尚未添加供应商）".to_string()]);
        let add = find_submenu(root, "添加供应商").expect("有「添加供应商」子菜单");
        assert_eq!(
            unsafe { GetMenuItemCount(add) },
            ProviderId::ALL.len() as i32
        );
        let _ = unsafe { DestroyMenu(root) };
    }

    /// 显示器子菜单：每块屏一项、主屏缀「（主）」、勾选态 = 生效门控。
    #[test]
    fn built_menu_lists_displays_with_effective_checks() {
        use crate::strip::geometry::DisplayInfo;
        let displays = vec![
            DisplayInfo {
                device: "\\\\.\\DISPLAY1".into(),
                primary: true,
                width: 2560,
                height: 1440,
            },
            DisplayInfo {
                device: "\\\\.\\DISPLAY2".into(),
                primary: false,
                width: 1920,
                height: 1080,
            },
        ];
        let mut cfg = Settings::default();
        cfg.show_all_monitors = false; // 仅主屏 → 只勾主屏
        let root = unsafe { CreatePopupMenu() }.expect("root 菜单");
        let mut keep: Vec<Vec<u16>> = Vec::new();
        assert!(build(
            root,
            &cfg,
            &configured_only(&[]),
            &[],
            &displays,
            &mut keep
        ));
        let mon = find_submenu(root, "多屏显示").expect("多屏子菜单");
        let t = texts(mon);
        assert_eq!(t[0], "全部屏");
        assert_eq!(t[1], "仅主屏");
        assert!(item_checked(mon, 1), "仅主屏模式 → 主屏项勾选");
        // 屏项：第 3、4 项（分隔符不产生文本）
        assert_eq!(t[2], "显示器1 · 2560x1440（主）");
        assert_eq!(t[3], "显示器2 · 1920x1080");
        assert!(item_checked(mon, 3), "主屏勾选");
        assert!(!item_checked(mon, 4), "副屏未勾选");
        let _ = unsafe { DestroyMenu(root) };
    }

    /// 档位表自检（移植 blue）：id 唯一且连续、百分比在允许区间、含默认档。
    #[test]
    fn font_presets_are_sane() {
        let ids: Vec<usize> = FONT_PRESETS.iter().map(|(id, _)| *id).collect();
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), ids.len(), "菜单 id 必须唯一");
        // CheckMenuRadioItem 需要连续区段
        assert!(ids.windows(2).all(|w| w[1] - w[0] == 1));
        for (_, pct) in FONT_PRESETS {
            assert!(
                (MIN_TEXT_PCT..=MAX_TEXT_PCT).contains(&pct),
                "档位 {pct} 超出允许范围"
            );
        }
        assert!(
            FONT_PRESETS.iter().any(|(_, p)| *p == DEFAULT_TEXT_PCT),
            "必须有默认档"
        );
    }

    /// 各组菜单 id 区段互不重叠（radio/勾选连续区段之间、与四段 provider 基址之间）。
    #[test]
    fn menu_id_ranges_do_not_overlap() {
        let singles = [
            MID_TOGGLE,
            MID_REFRESH,
            MID_NOTIFY,
            MID_AUTOSTART,
            MID_EXIT,
        ];
        let n = ProviderId::ALL.len();
        // (起, 止) 闭区间
        let ranges: [(usize, usize); 10] = [
            (MID_POS_LEFT, MID_POS_RIGHT),
            (MID_FONT_S, MID_FONT_XL),
            (MID_MON_ALL, MID_MON_PRIMARY),
            (MID_INT30, MID_INT300),
            (MID_STYLE_CHIPS, MID_STYLE_TEXT),
            (MID_DISP_BASE, MID_DISP_BASE + MAX_MENU_DISPLAYS - 1),
            (MID_PROV_BASE, MID_PROV_BASE + n - 1),
            (MID_ADD_BASE, MID_ADD_BASE + n - 1),
            (MID_ACC_BASE, MID_ACC_BASE + n * MAX_MENU_ACCOUNTS - 1),
            (MID_DEL_BASE, MID_DEL_BASE + n * MAX_MENU_ACCOUNTS - 1),
        ];
        for &s in &singles {
            for &(lo, hi) in &ranges {
                assert!(!(lo..=hi).contains(&s), "单 id {s} 与区段 [{lo},{hi}] 重叠");
            }
        }
        for (i, &(lo, hi)) in ranges.iter().enumerate() {
            assert!(lo <= hi);
            for &(lo2, hi2) in ranges.iter().skip(i + 1) {
                assert!(
                    hi < lo2 || hi2 < lo,
                    "区段 [{lo},{hi}] 与 [{lo2},{hi2}] 重叠"
                );
            }
        }
        // 四段 provider 相关区段顺序排开、互不重叠（显示段与移除段同布局、各自独立）
        assert!(MID_ADD_BASE >= MID_PROV_BASE + n);
        assert!(MID_ACC_BASE >= MID_ADD_BASE + n);
        assert!(MID_DEL_BASE >= MID_ACC_BASE + n * MAX_MENU_ACCOUNTS);
    }

    /// 「添加账号」id 映射：每家都映射到 AddAccount，越界 → None。
    /// 与是否已配置**无关**——全表供应商永远可点（点开＝再加一个账号）。
    #[test]
    fn add_menu_id_maps_to_add_account() {
        let cfg = Settings::default();
        for (i, id) in ProviderId::ALL.iter().enumerate() {
            assert_eq!(
                map_id(MID_ADD_BASE + i, &cfg, &[], &[]),
                Some(MenuCmd::AddAccount(*id)),
                "MID_ADD_BASE+{i} 应映射 {id:?}"
            );
        }
        assert_eq!(
            map_id(MID_ADD_BASE + ProviderId::ALL.len(), &cfg, &[], &[]),
            None,
            "越界 → None"
        );
    }

    /// 供应商管理段 = 已添加 provider 的 ALL 下标（顺序保持，map_id 按原始下标）。
    #[test]
    fn manage_indices_only_configured() {
        assert_eq!(configured_indices(&[]), Vec::<usize>::new());
        assert_eq!(configured_indices(&[false, false]), Vec::<usize>::new());
        assert_eq!(configured_indices(&[true, false, true]), vec![0, 2]);
        let all = vec![true; ProviderId::ALL.len()];
        assert_eq!(configured_indices(&all).len(), ProviderId::ALL.len());
        // 全未添加（全新安装）→ 空 → show() 发灰字占位而非子菜单
        let none = vec![false; ProviderId::ALL.len()];
        assert!(configured_indices(&none).is_empty());
    }

    /// provider 总开关的 id 映射往返：每个 provider 项都映射成取反启用状态。
    #[test]
    fn provider_menu_id_maps_to_inverted_toggle() {
        let cfg = Settings::default(); // 核心三家启用
        for (i, id) in ProviderId::ALL.iter().enumerate() {
            let cmd = map_id(MID_PROV_BASE + i, &cfg, &[], &[]).expect("provider id 必须可映射");
            match cmd {
                MenuCmd::Provider(pid, enable) => {
                    assert_eq!(pid, *id);
                    assert_eq!(enable, !cfg.is_enabled(*id));
                }
                other => panic!("意外命令 {other:?}"),
            }
        }
        // 核心三家默认启用 → 目标是停用
        assert_eq!(
            map_id(MID_PROV_BASE, &cfg, &[], &[]),
            Some(MenuCmd::Provider(ProviderId::Mimo, false))
        );
        // 未知 id → None
        assert_eq!(map_id(9999, &cfg, &[], &[]), None);
    }

    #[test]
    fn poll_interval_menu_values_match_spec() {
        let cfg = Settings::default();
        assert_eq!(
            map_id(MID_INT30, &cfg, &[], &[]),
            Some(MenuCmd::Interval(30))
        );
        assert_eq!(
            map_id(MID_INT60, &cfg, &[], &[]),
            Some(MenuCmd::Interval(60))
        );
        assert_eq!(
            map_id(MID_INT300, &cfg, &[], &[]),
            Some(MenuCmd::Interval(300))
        );
    }

    /// 「样式」radio 两档的 id 映射。
    #[test]
    fn style_menu_maps_two_radio_values() {
        let cfg = Settings::default();
        assert_eq!(
            map_id(MID_STYLE_CHIPS, &cfg, &[], &[]),
            Some(MenuCmd::Style(StripStyle::Chips))
        );
        assert_eq!(
            map_id(MID_STYLE_TEXT, &cfg, &[], &[]),
            Some(MenuCmd::Style(StripStyle::Text))
        );
        // 默认档是 chips（菜单勾选状态由 cfg.style() 决定）
        assert_eq!(cfg.style(), StripStyle::Chips);
    }

    /// 账号勾选槽 → 从当前生效勾选态重算白名单（None 全勾→取反剔除；Some 按值→
    /// 含增含删；越界/无账号 → None）。**所有 provider 一视同仁**。
    #[test]
    fn account_slot_toggle_recomputes_display_list() {
        let mut cfg = Settings::default();
        // 已添加（配置账号）的 DeepSeek：两个账号
        cfg.add_account(ProviderId::DeepSeek, "主号", "provider/deepseek/api-key/a".into());
        cfg.add_account(ProviderId::DeepSeek, "备用", "provider/deepseek/api-key/b".into());
        let accs = crate::accounts::list(&cfg, ProviderId::DeepSeek);
        assert_eq!(accs.len(), 2);
        let view = vec![(ProviderId::DeepSeek, accs.clone())];
        let ds_idx = ProviderId::ALL
            .iter()
            .position(|p| *p == ProviderId::DeepSeek)
            .unwrap();
        let base = MID_ACC_BASE + ds_idx * MAX_MENU_ACCOUNTS;

        // None（全显示）→ 取消第一个账号 → 白名单里其余账号仍在、该账号被摘
        let cmd = map_id(base, &cfg, &view, &[]).expect("槽位可映射");
        let MenuCmd::AccountDisplay(list) = cmd else {
            panic!("应为 AccountDisplay");
        };
        let removed = account_key(ProviderId::DeepSeek, &accs[0].id);
        let kept = account_key(ProviderId::DeepSeek, &accs[1].id);
        assert!(!list.contains(&removed));
        assert!(list.contains(&kept));

        // 反向：取消后勾回来 → 名单里重新出现
        cfg.display_accounts = Some(list);
        let cmd2 = map_id(base, &cfg, &view, &[]).expect("槽位可映射");
        let MenuCmd::AccountDisplay(list2) = cmd2 else {
            panic!("应为 AccountDisplay");
        };
        assert!(list2.contains(&removed), "再勾一次应重新加入");

        // 越界槽（无对应账号）/ 未添加的 provider → None
        assert_eq!(map_id(base + 2, &cfg, &view, &[]), None, "第三个账号不存在");
        let mimo_idx = ProviderId::ALL
            .iter()
            .position(|p| *p == ProviderId::Mimo)
            .unwrap();
        assert_eq!(
            map_id(MID_ACC_BASE + mimo_idx * MAX_MENU_ACCOUNTS, &cfg, &view, &[]),
            None,
            "未添加的 provider 不出槽位"
        );
    }

    /// 移除账号槽 → (provider, 账号 id)；单账号/多账号两种布局共用同一映射。
    #[test]
    fn remove_account_slot_maps_to_account_id() {
        let mut cfg = Settings::default();
        cfg.add_account(ProviderId::OpenRouter, "主号", "provider/openrouter/api-key/a".into());
        cfg.add_account(ProviderId::OpenRouter, "备用", "provider/openrouter/api-key/b".into());
        let accs = crate::accounts::list(&cfg, ProviderId::OpenRouter);
        let view = vec![(ProviderId::OpenRouter, accs.clone())];
        let idx = ProviderId::ALL
            .iter()
            .position(|p| *p == ProviderId::OpenRouter)
            .unwrap();
        let base = MID_DEL_BASE + idx * MAX_MENU_ACCOUNTS;
        assert_eq!(
            map_id(base, &cfg, &view, &[]),
            Some(MenuCmd::RemoveAccount(ProviderId::OpenRouter, accs[0].id.clone()))
        );
        assert_eq!(
            map_id(base + 1, &cfg, &view, &[]),
            Some(MenuCmd::RemoveAccount(ProviderId::OpenRouter, accs[1].id.clone()))
        );
        assert_eq!(map_id(base + 2, &cfg, &view, &[]), None, "越界 → None");
    }

    /// 账号菜单文案：活动账号追加纯文本 ` ●`；无展示名 →（未命名账号）。
    #[test]
    fn account_item_label_marks_active_with_dot() {
        let active = AccountInfo {
            id: "a".into(),
            email: Some("a@x.com".into()),
            active: true,
        };
        assert_eq!(account_item_label(&active), "a@x.com ●");
        let inactive = AccountInfo {
            id: "b".into(),
            email: Some("b@x.com".into()),
            active: false,
        };
        assert_eq!(account_item_label(&inactive), "b@x.com");
        let unnamed = AccountInfo {
            id: "c".into(),
            email: None,
            active: true,
        };
        assert_eq!(account_item_label(&unnamed), "（未命名账号） ●");
        let empty = AccountInfo {
            id: "d".into(),
            email: Some(String::new()),
            active: false,
        };
        assert_eq!(account_item_label(&empty), "（未命名账号）");
    }

    /// 「添加账号」文案按凭据形态分流（OAuth = 登录；Cookie = Cookie 提示）。
    #[test]
    fn add_account_label_routes_by_credential_kind() {
        assert!(add_account_label(ProviderId::Antigravity).contains("登录新账号"));
        assert!(add_account_label(ProviderId::Codex).contains("登录新账号"));
        assert!(add_account_label(ProviderId::Mimo).contains("网页登录"));
        let plain = add_account_label(ProviderId::DeepSeek);
        assert!(plain.contains("添加账号"));
        assert!(!plain.contains("Cookie"));
    }

    /// 显示器勾选 → 从**当前生效勾选态**取反该屏，重算整份白名单
    /// （与 config::should_show_on 真值表联动）。
    #[test]
    fn display_menu_recomputes_whitelist_from_effective_state() {
        use crate::strip::geometry::DisplayInfo;
        let d1 = DisplayInfo {
            device: "\\\\.\\DISPLAY1".into(),
            primary: true,
            width: 2560,
            height: 1440,
        };
        let d2 = DisplayInfo {
            device: "\\\\.\\DISPLAY2".into(),
            primary: false,
            width: 1920,
            height: 1080,
        };
        let displays = vec![d1.clone(), d2.clone()];

        // None + 全部屏 → 全勾；取消 DISPLAY2 → Some([DISPLAY1])
        let all = Settings::default();
        assert_eq!(
            map_id(MID_DISP_BASE + 1, &all, &[], &displays),
            Some(MenuCmd::DisplaySet(vec!["\\\\.\\DISPLAY1".into()]))
        );
        // Some 模式：不在白名单的屏点击 → 加入
        let mut only1 = Settings::default();
        only1.display_devices = Some(vec!["\\\\.\\DISPLAY1".into()]);
        assert_eq!(
            map_id(MID_DISP_BASE + 1, &only1, &[], &displays),
            Some(MenuCmd::DisplaySet(vec![
                "\\\\.\\DISPLAY1".into(),
                "\\\\.\\DISPLAY2".into()
            ]))
        );
        // None + 仅主屏 → 只勾主屏；取消主屏 → 空白名单（全隐藏、保托盘）
        let mut primary_only = Settings::default();
        primary_only.show_all_monitors = false;
        assert_eq!(
            map_id(MID_DISP_BASE, &primary_only, &[], &displays),
            Some(MenuCmd::DisplaySet(vec![]))
        );
        // 越界槽 / 空显示器列表 → None
        assert_eq!(map_id(MID_DISP_BASE + 8, &all, &[], &displays), None);
        assert_eq!(map_id(MID_DISP_BASE, &all, &[], &[]), None);
    }

    /// 显示器菜单文案：`显示器n · WxH`，主屏缀「（主）」。
    #[test]
    fn display_label_shows_resolution_and_primary_mark() {
        use crate::strip::geometry::DisplayInfo;
        let main = DisplayInfo {
            device: "\\\\.\\DISPLAY1".into(),
            primary: true,
            width: 2560,
            height: 1440,
        };
        let side = DisplayInfo {
            device: "\\\\.\\DISPLAY2".into(),
            primary: false,
            width: 1920,
            height: 1080,
        };
        assert_eq!(display_label(0, &main), "显示器1 · 2560x1440（主）");
        assert_eq!(display_label(1, &side), "显示器2 · 1920x1080");
    }
}
