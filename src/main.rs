//! quota-bar — 轻量 Windows 任务栏 AI 套餐额度显示条（Rust / 原生 Win32）。
//!
//! Usage:
//!   quota-bar                    启动（任务栏内嵌条 + 托盘）
//!   quota-bar json               抓一轮并打印 JSON（调试/与 dsh 比对）
//!   quota-bar setkey <id> <key>  设置 provider API key（写 Credential Manager 并启用）
//!   quota-bar setkey <id> --delete
//!   quota-bar enable|disable <id>
//!   quota-bar login|logout <id>  OAuth 登录（antigravity 等）
//!   quota-bar providers          列出 provider id
//!
//! `<id>` 见 `quota-bar providers`（如 mimo / opencode-go / antigravity / deepseek）。

// GUI 子系统：无参启动不弹控制台；带参从终端启动时重新附着父控制台。
#![windows_subsystem = "windows"]

mod accounts;
mod browser_login;
mod config;
mod dblog;
mod icon;
mod model;
mod oauth;
mod poll;
mod providers;
mod secrets;
mod strip;
mod theme;

use std::sync::{Arc, RwLock};

use config::Settings;
use model::{CredentialKind, ProviderId};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    attach_parent_console(&args);

    // 调试/管理子命令先于单实例检查：应用运行时也要能 setkey/json。
    if let Some(code) = handle_cli(&args) {
        std::process::exit(code);
    }

    if single_instance_taken() {
        return;
    }

    // Per-Monitor V2：双屏不同 DPI 下几何/字体必须按窗口 DPI 缩放。
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    dblog::log("quota-bar starting");

    let settings = Arc::new(RwLock::new(Settings::load()));
    let store = poll::SnapshotStore::new();
    let poll_handle = poll::spawn(settings.clone(), store.clone());

    let code = strip::run(settings, store, poll_handle);
    std::process::exit(code);
}

/// true = 已有实例在运行（互斥体已存在）。
/// 内核互斥体句柄**故意不关闭**：进程存活期间对象存在，退出时系统回收。
fn single_instance_taken() -> bool {
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "QuotaBarSingleInstance\0".encode_utf16().collect();
        CreateMutexW(None, false, PCWSTR(name.as_ptr())).is_ok()
            && GetLastError() == ERROR_ALREADY_EXISTS
    }
}

/// GUI 子系统无控制台；带参（子命令）从终端启动时附着父控制台使 println 可见。
fn attach_parent_console(args: &[String]) {
    if args.is_empty() {
        return;
    }
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn handle_cli(args: &[String]) -> Option<i32> {
    if args.is_empty() {
        return None;
    }
    Some(match args[0].as_str() {
        "json" => cli_json(),
        "setkey" => cli_setkey(&args[1..]),
        "setcookie" => cli_setcookie(&args[1..]),
        "addaccount" => cli_addaccount(&args[1..]),
        "rmaccount" => cli_rmaccount(&args[1..]),
        "enable" => cli_toggle(&args[1..], true),
        "disable" => cli_toggle(&args[1..], false),
        "login" => cli_login(&args[1..]),
        "logout" => cli_logout(&args[1..]),
        "accounts" => cli_accounts(&args[1..]),
        "switch" => cli_switch(&args[1..]),
        "providers" => cli_providers(),
        "help" | "--help" | "-h" => cli_help(),
        other => {
            eprintln!("未知子命令 `{other}`；`quota-bar --help` 查看用法");
            2
        }
    })
}

fn cli_json() -> i32 {
    let settings = Settings::load();
    let snaps = providers::query_all(&settings);
    match serde_json::to_string_pretty(&snaps) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            eprintln!("序列化失败：{e}");
            1
        }
    }
}

fn cli_setkey(args: &[String]) -> i32 {
    let Some(id_str) = args.first() else {
        eprintln!("用法：quota-bar setkey <id> <key> | <id> --delete");
        return 2;
    };
    let Some(id) = ProviderId::parse(id_str) else {
        eprintln!("未知 provider `{id_str}`；`quota-bar providers` 查看列表");
        return 2;
    };
    let mut settings = Settings::load();
    match args.get(1).map(|s| s.as_str()) {
        Some("--delete") => {
            // 删除该 provider 的**全部**账号（凭据 + 配置），与「移除账号」等价。
            let mut removed = 0usize;
            for acc in settings.accounts(id).to_vec() {
                if accounts::remove(&mut settings, id, &acc.id).is_ok() {
                    removed += 1;
                }
            }
            settings.save();
            println!("已删除 {} 的 {removed} 个账号", id.display_name());
            0
        }
        Some(key) => {
            if id.credential_kind() == CredentialKind::Cookie {
                eprintln!(
                    "{} 用平台 Cookie（不是 API key）：quota-bar setcookie {} <cookie>",
                    id.display_name(),
                    id.as_str()
                );
                return 2;
            }
            // 覆盖为**唯一账号**（多账号请用 addaccount）。
            let mut settings = Settings::load();
            let result = (|| {
                for acc in settings.accounts(id).to_vec() {
                    accounts::remove(&mut settings, id, &acc.id)?;
                }
                accounts::add(&mut settings, id, "", key)
            })();
            match result {
                Ok(acc_id) => {
                    settings.save();
                    println!("已保存 {} 的 API key（账号 {acc_id}，已启用）", id.display_name());
                    0
                }
                Err(e) => {
                    eprintln!("写入凭据失败：{e}");
                    1
                }
            }
        }
        None => {
            eprintln!("用法：quota-bar setkey <id> <key> | <id> --delete");
            2
        }
    }
}

/// 追加一个账号（多账号）：`quota-bar addaccount <id> <secret> [label]`。
/// 与菜单「添加账号…」同源（`accounts::add`）：写 keyring + 配置 + 自动启用。
fn cli_addaccount(args: &[String]) -> i32 {
    let (Some(id_str), Some(secret)) = (args.first(), args.get(1)) else {
        eprintln!("用法：quota-bar addaccount <id> <key|cookie> [账号名]");
        return 2;
    };
    let Some(id) = ProviderId::parse(id_str) else {
        eprintln!("未知 provider `{id_str}`；`quota-bar providers` 查看列表");
        return 2;
    };
    if id.credential_kind() == CredentialKind::OAuth {
        eprintln!(
            "{} 用浏览器登录（quota-bar login {}），不走 API key/Cookie",
            id.display_name(),
            id.as_str()
        );
        return 2;
    }
    let label = args.get(2).map(|s| s.as_str()).unwrap_or("");
    let mut settings = Settings::load();
    match accounts::add(&mut settings, id, label, secret) {
        Ok(acc_id) => {
            settings.save();
            println!("已添加 {} 账号 {acc_id}（已启用）", id.display_name());
            0
        }
        Err(e) => {
            eprintln!("写入凭据失败：{e}");
            1
        }
    }
}

/// 移除账号：`quota-bar rmaccount <id> <account_id|唯一前缀>`（先用 accounts 查看）。
fn cli_rmaccount(args: &[String]) -> i32 {
    let (Some(id_str), Some(want)) = (args.first(), args.get(1).map(|s| s.as_str())) else {
        eprintln!("用法：quota-bar rmaccount <id> <account_id|唯一前缀>（先用 accounts 查看）");
        return 2;
    };
    let Some(id) = ProviderId::parse(id_str) else {
        eprintln!("未知 provider `{id_str}`；`quota-bar providers` 查看列表");
        return 2;
    };
    let mut settings = Settings::load();
    let list = accounts::list(&settings, id);
    if list.is_empty() {
        eprintln!("{} 暂无账号", id.display_name());
        return 1;
    }
    let target = list
        .iter()
        .find(|a| a.id == want)
        .or_else(|| {
            let mut hits = list.iter().filter(|a| a.id.starts_with(want));
            let first = hits.next();
            hits.next().is_none().then_some(first).flatten()
        });
    let Some(acc) = target else {
        eprintln!("无匹配账号（id 或前缀不唯一/不存在）");
        return 1;
    };
    match accounts::remove(&mut settings, id, &acc.id) {
        Ok(()) => {
            settings.save();
            println!(
                "已移除 {} 账号 {}",
                id.display_name(),
                acc.email.as_deref().unwrap_or(&acc.id)
            );
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// 平台 Cookie 存取（MiMo 三接口等）：`quota-bar setcookie <id> <cookie> | <id> --delete`。
/// 语义与 `setkey` 一致：**覆盖为唯一账号**（多账号用 `addaccount`）。
fn cli_setcookie(args: &[String]) -> i32 {
    let Some(id_str) = args.first() else {
        eprintln!("用法：quota-bar setcookie <id> <cookie> | <id> --delete");
        return 2;
    };
    let Some(id) = ProviderId::parse(id_str) else {
        eprintln!("未知 provider `{id_str}`；`quota-bar providers` 查看列表");
        return 2;
    };
    match args.get(1).map(|s| s.as_str()) {
        Some("--delete") => {
            let mut settings = Settings::load();
            let mut removed = 0usize;
            for acc in settings.accounts(id).to_vec() {
                if accounts::remove(&mut settings, id, &acc.id).is_ok() {
                    removed += 1;
                }
            }
            settings.save();
            println!("已删除 {} 的 {removed} 个账号", id.display_name());
            0
        }
        Some(cookie) => {
            if id.credential_kind() != CredentialKind::Cookie {
                eprintln!(
                    "{} 用 API key（不是平台 Cookie）：quota-bar setkey {} <key>",
                    id.display_name(),
                    id.as_str()
                );
                return 2;
            }
            let mut settings = Settings::load();
            let result = (|| {
                for acc in settings.accounts(id).to_vec() {
                    accounts::remove(&mut settings, id, &acc.id)?;
                }
                accounts::add(&mut settings, id, "", cookie)
            })();
            match result {
                Ok(acc_id) => {
                    settings.save();
                    println!("已保存 {} 的平台 Cookie（账号 {acc_id}，已启用）", id.display_name());
                    0
                }
                Err(e) => {
                    eprintln!("写入凭据失败：{e}");
                    1
                }
            }
        }
        None => {
            eprintln!("用法：quota-bar setcookie <id> <cookie> | <id> --delete");
            2
        }
    }
}

fn cli_toggle(args: &[String], enabled: bool) -> i32 {
    let Some(id_str) = args.first() else {
        eprintln!("用法：quota-bar {} <id>", if enabled { "enable" } else { "disable" });
        return 2;
    };
    let Some(id) = ProviderId::parse(id_str) else {
        eprintln!("未知 provider `{id_str}`；`quota-bar providers` 查看列表");
        return 2;
    };
    let mut settings = Settings::load();
    settings.set_enabled(id, enabled);
    settings.save();
    println!(
        "{} {}",
        id.display_name(),
        if enabled { "已启用" } else { "已停用" }
    );
    0
}

fn cli_login(args: &[String]) -> i32 {
    let Some(id) = args.first().and_then(|s| ProviderId::parse(s)) else {
        eprintln!("用法：quota-bar login <id>（antigravity / chatgpt / mimo / mimo-sg / step）");
        return 2;
    };
    match if id.credential_kind() == CredentialKind::Cookie {
        browser_login::login_blocking(id)
    } else { oauth::login_blocking(id) } {
        Ok(()) => {
            // Some 显示集模式：新登录的活动账号并入显示集（否则未勾选=不显示）。
            let mut settings = Settings::load();
            settings.set_enabled(id, true);
            if settings.display_accounts.is_some() {
                if let Some(active) = oauth::list_accounts(id)
                    .iter()
                    .find(|a| a.active)
                    .map(|a| a.id.clone())
                {
                    accounts::set_visible(&mut settings, id, &active, true);
                    settings.save();
                }
            }
            settings.save();
            println!("{} 登录成功", id.display_name());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// 切换活动账号：`quota-bar switch <id> <account_id|唯一前缀>`。
/// 菜单的账号组已改为"显示勾选"，活动账号（决定 logout 对象与 CLI 视角）走这里。
fn cli_switch(args: &[String]) -> i32 {
    let (Some(pid_str), Some(want)) = (args.first(), args.get(1).map(|s| s.as_str())) else {
        eprintln!("用法：quota-bar switch <id> <account_id|唯一前缀>（先用 accounts 查看）");
        return 2;
    };
    let Some(pid) = ProviderId::parse(pid_str) else {
        eprintln!("未知 provider `{pid_str}`；`quota-bar providers` 查看列表");
        return 2;
    };
    let list = oauth::list_accounts(pid);
    if list.is_empty() {
        eprintln!("{} 暂无账号", pid.display_name());
        return 1;
    }
    // 精确 id 优先；否则按唯一前缀匹配（24 位 hex 手打太长）。
    let target = list
        .iter()
        .find(|a| a.id == want)
        .or_else(|| {
            let mut hits = list.iter().filter(|a| a.id.starts_with(want));
            let first = hits.next();
            hits.next().is_none().then_some(first).flatten()
        });
    let Some(acc) = target else {
        eprintln!("无匹配账号（id 或前缀不唯一/不存在）");
        return 1;
    };
    match oauth::switch_account(pid, &acc.id) {
        Ok(()) => {
            println!(
                "{} 活动账号 → {}",
                pid.display_name(),
                acc.email.as_deref().unwrap_or(&acc.id)
            );
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn cli_logout(args: &[String]) -> i32 {
    let Some(id) = args.first().and_then(|s| ProviderId::parse(s)) else {
        eprintln!("用法：quota-bar logout <id>");
        return 2;
    };
    match oauth::logout(id) {
        Ok(()) => {
            println!("{} 已登出", id.display_name());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// 列出某 provider 的账号表（多账号管理的 CLI 视图）：`*` 标记活动账号。
fn cli_accounts(args: &[String]) -> i32 {
    let Some(id) = args.first().and_then(|s| ProviderId::parse(s)) else {
        eprintln!("用法：quota-bar accounts <id>");
        return 2;
    };
    let settings = Settings::load();
    let list = accounts::list(&settings, id);
    if list.is_empty() {
        println!("{} 暂无账号（未添加）", id.display_name());
        return 0;
    }
    for a in &list {
        println!(
            "{} {:<26} {}",
            if a.active { "*" } else { " " },
            a.email.as_deref().unwrap_or("（未命名账号）"),
            a.id
        );
    }
    let visible = list
        .iter()
        .filter(|a| accounts::is_visible(&settings, id, &a.id))
        .count();
    println!(
        "共 {} 个账号，{visible} 个显示在状态栏；* 为活动账号（OAuth 抓取/CLI 视角用它）",
        list.len()
    );
    0
}

fn cli_providers() -> i32 {
    for id in ProviderId::ALL {
        let note = match id.credential_kind() {
            CredentialKind::OAuth => format!("OAuth 登录（quota-bar login {}）", id.as_str()),
            CredentialKind::Cookie => format!(
                "平台 Cookie（setcookie/addaccount）· 环境变量 {}",
                providers::cookie_env_names(id).first().copied().unwrap_or("-")
            ),
            CredentialKind::ApiKey => format!(
                "API key（setkey/addaccount）· 环境变量 {}",
                id.env_key_names().first().copied().unwrap_or("-")
            ),
        };
        println!("{:<14} {:<20} {note}", id.as_str(), id.display_name());
    }
    0
}

fn cli_help() -> i32 {
    println!(
        "quota-bar — Windows 任务栏 AI 套餐额度显示条\n\
         \n\
         用法：\n\
         \x20 quota-bar                    启动（任务栏内嵌条 + 托盘）\n\
         \x20 quota-bar json               抓一轮并打印 JSON\n\
         \x20 quota-bar providers          列出 provider id（含凭据形态）\n\
         \x20 quota-bar setkey <id> <key>  设置 API key（覆盖为唯一账号并启用）\n\
         \x20 quota-bar setcookie <id> <cookie>   设置平台 Cookie（同上，覆盖为唯一账号）\n\
         \x20 quota-bar addaccount <id> <key|cookie> [账号名]   追加账号（多账号）\n\
         \x20 quota-bar rmaccount <id> <账号id|前缀>           移除账号\n\
         \x20 quota-bar setkey|setcookie <id> --delete         删除该 provider 全部账号\n\
         \x20 quota-bar enable <id> / disable <id>\n\
         \x20 quota-bar login <id> / logout <id>   OAuth 登录（antigravity / chatgpt，codex 兼容）\n\
         \x20 quota-bar accounts <id>      列出账号（* = 活动账号）\n\
         \x20 quota-bar switch <id> <aid>  切换活动账号（aid 可用唯一前缀）\n\
         \n\
         菜单：托盘/条右键 →「供应商管理」（只列已添加的，逐账号勾选显示）\n\
         \x20     「添加供应商」（全部供应商；点开＝再加一个账号 / 登录）。\n\
         \n\
         配置：%LOCALAPPDATA%\\QuotaBar\\config.json；密钥存 Windows 凭据管理器。"
    );
    0
}
