//! 账号视图与变更（菜单 / CLI 共用的**唯一入口**）。
//!
//! 两套存储在此合并成同一个「账号」概念：
//! - **OAuth 类**（Antigravity / Codex）：账号表在 `oauth`（keyring
//!   `session/{provider}`，有"活动账号"概念，抓取时逐个账号并行）；
//! - **API key / Cookie 类**：账号表在配置（`config.providers[id].accounts`，
//!   凭据值在 keyring `provider/{id}/api-key[/{acc}]`），没有活动账号概念 ——
//!   每个账号各自独立抓取。
//!
//! 显示白名单（`config.display_accounts`）的**键 = `"{provider}:{account_id}"`**，
//! 勾选态 = 生效态：菜单每勾一次就按当前生效集合整份重算（旧格式裸 id 只对
//! Antigravity 生效，见 `config::should_show_account`）。

use crate::config::{self, Settings};
use crate::model::{CredentialKind, ProviderId};
use crate::oauth::{self, AccountInfo};
use crate::providers;
use crate::secrets;

/// 环境变量兜底账号的 id（`credentials()` 同源；不可删除）。
pub const ENV_ACCOUNT_ID: &str = "env";

/// 该 provider 的全部账号（顺序 = 展示顺序；两套存储合并）。
///
/// - OAuth 类 → `oauth` 账号表（`active` 标记有效）；
/// - API key / Cookie 类 → 配置账号表 +（无配置账号时的）旧单账号 Cookie 槽
///   +（仍无时的）环境变量兜底账号（后两者 `active` 恒 false）。
pub fn list(settings: &Settings, id: ProviderId) -> Vec<AccountInfo> {
    if id.uses_oauth_accounts() {
        return oauth::list_accounts(id);
    }
    let cookie_kind = id.credential_kind() == CredentialKind::Cookie;
    let mut out: Vec<AccountInfo> = settings
        .accounts(id)
        .iter()
        .map(|acc| AccountInfo {
            id: acc.id.clone(),
            email: Some(label_of(acc)),
            active: false,
        })
        .collect();
    if out.is_empty() && cookie_kind {
        // 旧单账号 Cookie 槽（`setcookie` 写的）→ 隐式账号（可显示/隐藏/删除）
        if let Some(v) = providers::legacy_cookie_value(id) {
            out.push(AccountInfo {
                id: providers::legacy_cookie_account_id(id),
                email: Some(mask_secret(&v)),
                active: false,
            });
        }
    }
    // 环境变量兜底（无配置账号且无旧槽时才成立，与 providers::credentials 同源）。
    if out.is_empty() {
        let has_env = if cookie_kind {
            providers::resolve_cookie(id, providers::cookie_env_names(id)).is_some()
        } else {
            providers::env_api_key(settings, id).is_some()
        };
        if has_env {
            out.push(AccountInfo {
                id: ENV_ACCOUNT_ID.into(),
                email: Some("环境变量".into()),
                active: false,
            });
        }
    }
    out
}

/// 配置账号的展示名：用户填的 label 优先，否则按凭据尾号（`…1f2a`）兜底。
pub fn label_of(acc: &crate::config::ProviderAccount) -> String {
    if !acc.label.is_empty() {
        return acc.label.clone();
    }
    match providers::resolve_ref(&acc.secret_ref) {
        Some(v) => mask_secret(&v),
        None => "（凭据缺失）".into(),
    }
}

/// 凭据尾号掩码（只露最后 4 位；太短则整体打码）。
pub fn mask_secret(value: &str) -> String {
    let trimmed = value.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        return "…".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("…{tail}")
}

/// 该账号当前是否显示在条/面板（勾选态 = 生效态）。
pub fn is_visible(settings: &Settings, id: ProviderId, account_id: &str) -> bool {
    config::should_show_account(&settings.display_accounts, id, account_id)
}

/// 切换某账号的显示状态：按**当前生效集合**整份重算白名单并写回
/// （`Some(vec)`；空表照实生效 = 全部账号行隐藏）。
pub fn set_visible(settings: &mut Settings, id: ProviderId, account_id: &str, visible: bool) {
    let mut keys: Vec<String> = Vec::new();
    for pid in ProviderId::ALL {
        for acc in list(settings, pid) {
            if is_visible(settings, pid, &acc.id) {
                keys.push(config::account_key(pid, &acc.id));
            }
        }
    }
    if visible {
        let key = config::account_key(id, account_id);
        if !keys.contains(&key) {
            keys.push(key);
        }
    } else {
        keys.retain(|k| k != &config::account_key(id, account_id));
    }
    settings.display_accounts = Some(keys);
}

/// 添加账号（API key / Cookie 类）：值写 keyring，引用进配置，并自动启用。
/// 返回新账号 id。OAuth 类不走这里（它们走登录）。
pub fn add(
    settings: &mut Settings,
    id: ProviderId,
    label: &str,
    secret_value: &str,
) -> Result<String, String> {
    let value = secret_value.trim();
    if value.is_empty() {
        return Err("拒绝保存空凭据".into());
    }
    let acc_id = config::new_account_id();
    let secret_ref = match id.credential_kind() {
        CredentialKind::Cookie => {
            format!("{}/{acc_id}", secrets::provider_cookie_ref(id.as_str()))
        }
        _ => format!(
            "{}/{acc_id}",
            secrets::provider_api_key_ref(id.as_str())
        ),
    };
    secrets::put(&secret_ref, value)?;
    // 引用后缀与账号 id 用**同一个** id（`add_account_with_id`）——否则白名单
    // 会写进一个指向不存在账号的键，新账号在条上永远不显示。
    settings.add_account_with_id(id, &acc_id, label, secret_ref);
    let _ = settings.set_enabled(id, true); // 添加即启用（setkey/setcookie 同语义）
    // Some 白名单模式：新账号并入显示集（否则"添加了却看不见"）。
    if settings.display_accounts.is_some() {
        set_visible(settings, id, &acc_id, true);
    }
    Ok(acc_id)
}

/// 移除账号：
/// - OAuth 类 → 删 `oauth` 账号表条目（被删的是活动账号时自动切到第一个）；
/// - API key / Cookie 类 → 删 keyring 条目 + 配置账号（镜像 `secret_ref` 前移）；
/// - **旧单账号 Cookie 槽**（隐式账号）→ 只删 keyring 里的旧槽条目。
///
/// 同时把该账号从显示白名单里摘掉。环境变量兜底账号不可删（返回 Err）。
pub fn remove(settings: &mut Settings, id: ProviderId, account_id: &str) -> Result<(), String> {
    if account_id == ENV_ACCOUNT_ID && !id.uses_oauth_accounts() {
        return Err("环境变量兜底账号不能删除（请清除对应的环境变量）".into());
    }
    if id.uses_oauth_accounts() {
        oauth::remove_account(id, account_id)?;
    } else if account_id == providers::legacy_cookie_account_id(id) {
        // 隐式账号：不在配置表里，删凭据槽即可（下次 list 自然消失）
        if !settings.remove_account(id, account_id) {
            secrets::delete(&providers::legacy_cookie_ref(id))
                .map_err(|e| format!("删除凭据失败：{e}"))?;
        }
    } else {
        let secret_ref = settings
            .accounts(id)
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.secret_ref.clone());
        let Some(secret_ref) = secret_ref else {
            return Err("账号不存在".into());
        };
        if !settings.remove_account(id, account_id) {
            return Err("账号不存在".into());
        }
        let _ = secrets::delete(&secret_ref);
    }
    if let Some(list) = settings.display_accounts.as_mut() {
        let key = config::account_key(id, account_id);
        list.retain(|k| k != &key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secret_keeps_only_tail() {
        assert_eq!(mask_secret("sk-abcdef1234"), "…1234");
        assert_eq!(mask_secret("abcd"), "…", "太短整体打码");
        assert_eq!(mask_secret(""), "…");
    }

    /// 白名单重算：勾选 = 生效集合；取消 = 摘除；旧格式只影响 AGY。
    /// 账号由 `add_account` 造出（不依赖真实 keyring 里有没有凭据）。
    #[test]
    fn set_visible_recomputes_whole_whitelist() {
        let mut s = Settings::default();
        let a = s.add_account(ProviderId::Mimo, "主号", "provider/mimo/cookie/test-a".into());
        let b = s.add_account(ProviderId::Mimo, "备用", "provider/mimo/cookie/test-b".into());
        // None 模式（全显示）→ 取消某账号 → 写成显式白名单：该账号被摘、其余保留
        set_visible(&mut s, ProviderId::Mimo, &a, false);
        let list = s.display_accounts.clone().expect("写成了 Some");
        assert!(!list.contains(&config::account_key(ProviderId::Mimo, &a)));
        assert!(list.contains(&config::account_key(ProviderId::Mimo, &b)));
        assert!(!is_visible(&s, ProviderId::Mimo, &a));
        assert!(is_visible(&s, ProviderId::Mimo, &b), "未取消的账号照旧显示");
        // 再勾回来 → 名单里重新出现
        set_visible(&mut s, ProviderId::Mimo, &a, true);
        assert!(is_visible(&s, ProviderId::Mimo, &a));
        // 旧格式名单：只影响 AGY，其它 provider 恒显示
        let mut legacy = Settings::default();
        legacy.display_accounts = Some(vec!["983960aba4e3fcf0bd4b2221".into()]);
        assert!(is_visible(&legacy, ProviderId::Mimo, "acc-1"));
        assert!(!is_visible(&legacy, ProviderId::Antigravity, "other"));
    }

    /// 白名单重算不丢行：取消一个账号后，其余账号（含 OAuth 与 key 类）仍在名单里。
    #[test]
    fn set_visible_preserves_other_accounts() {
        let mut s = Settings::default();
        // 造两个配置账号（不写 keyring：list() 会按"凭据缺失"兜底展示名）
        s.add_account(ProviderId::DeepSeek, "主号", "provider/deepseek/api-key/a".into());
        s.add_account(ProviderId::DeepSeek, "备用", "provider/deepseek/api-key/b".into());
        set_visible(&mut s, ProviderId::DeepSeek, "first-id", false);
        let list = s.display_accounts.clone().expect("Some");
        // 两个账号都进了名单（id 由 new_account_id 生成 → 逐个校验存在性）
        for acc in s.accounts(ProviderId::DeepSeek) {
            assert!(list.contains(&config::account_key(ProviderId::DeepSeek, &acc.id)));
        }
        assert!(!list.contains(&config::account_key(ProviderId::DeepSeek, "first-id")));
    }

    /// 环境变量兜底账号：可显示/隐藏，但不可删除。
    #[test]
    fn env_account_cannot_be_removed() {
        let mut s = Settings::default();
        let err = remove(&mut s, ProviderId::Mimo, ENV_ACCOUNT_ID).unwrap_err();
        assert!(err.contains("环境变量"), "{err}");
    }
}
