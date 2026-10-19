//! 持久化配置：%LOCALAPPDATA%\QuotaBar\config.json（原子写，缺字段回默认）。
//!
//! 密钥**不落**本文件：`secret_ref` 指向 Credential Manager 条目；
//! 环境变量（见 `ProviderId::env_key_names`）作为免配置兜底。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{CredentialKind, ProviderId};

/// 条内文字基准百分比（100 = 96 DPI 设计值）。
pub const DEFAULT_TEXT_PCT: u32 = 100;
pub const MIN_TEXT_PCT: u32 = 70;
pub const MAX_TEXT_PCT: u32 = 160;

/// provider 下的一个账号（API key / Cookie 类；OAuth 类账号在 `oauth` 存储里）。
///
/// `id` 是**稳定账号键**（8 hex）：行键 = `"{provider}:{id}"`（与 OAuth 行同构），
/// 也是 `display_accounts` 显示白名单里的账号标识。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderAccount {
    pub id: String,
    /// 展示名（用户可留空 → 展示层按 key 尾号 / 序号兜底）。
    pub label: String,
    /// keyring 用户名，形如 `provider/mimo/api-key/3f2a1b0c`。
    pub secret_ref: String,
}

/// 单个 provider 的配置（密钥值在 Credential Manager，这里只存引用）。
///
/// 多账号：`accounts` 是权威表（OAuth 类除外 —— 它们的账号表在 `oauth` 存储里）；
/// `secret_ref` 是**旧单账号字段**，加载时由 [`ProviderEntry::normalize`] 物化进
/// `accounts[0]` 并保持镜像，保证旧配置 / CLI / `resolve_api_key` 行为不变。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderEntry {
    pub enabled: bool,
    /// keyring 用户名，形如 `provider/mimo/api-key`（= `accounts[0].secret_ref` 镜像）。
    pub secret_ref: Option<String>,
    /// 该 provider 的全部账号（顺序 = 展示顺序）。
    pub accounts: Vec<ProviderAccount>,
}

impl ProviderEntry {
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            secret_ref: None,
            accounts: Vec::new(),
        }
    }

    /// 把旧 `secret_ref` 物化进 `accounts`（**幂等**）并回写镜像。
    ///
    /// 物化 id = `legacy_account_id(secret_ref)`（确定性哈希 → 行键跨次加载稳定）；
    /// 用户删掉该账号时 `remove_account` 会先清镜像，不会在这里被复活。
    ///
    /// **Cookie 类 provider 不物化**：它们的 `secret_ref` 是历史遗留的 API key 槽
    /// （Bearer 端点已下线），拿它当 Cookie 发会 401；这类 provider 的单账号凭据
    /// 在专用槽 `provider/{id}/cookie`（见 `providers::credentials`）。
    pub fn normalize(&mut self, provider: Option<ProviderId>) {
        let skip = provider.is_some_and(|id| id.credential_kind() == CredentialKind::Cookie);
        if !skip {
            if let Some(r) = self.secret_ref.clone() {
                if !r.is_empty() && !self.accounts.iter().any(|a| a.secret_ref == r) {
                    self.accounts.insert(
                        0,
                        ProviderAccount {
                            id: legacy_account_id(&r),
                            label: String::new(),
                            secret_ref: r,
                        },
                    );
                }
            }
        }
        self.accounts.retain(|a| !a.secret_ref.is_empty());
        self.secret_ref = self.accounts.first().map(|a| a.secret_ref.clone());
    }
}

/// 旧单账号 `secret_ref` → 确定性账号 id（sha256 前 8 hex）。
/// 同一 ref 每次加载都得到同一个 id，行键与显示白名单因此稳定。
pub fn legacy_account_id(secret_ref: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"quota-bar-account-v1");
    hasher.update([0]);
    hasher.update(secret_ref.as_bytes());
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    hex.chars().take(8).collect()
}

/// 新账号 id（8 hex 随机）：不泄露 keyring 引用、不随 label 变化。
pub fn new_account_id() -> String {
    let mut buf = [0u8; 4];
    if getrandom::getrandom(&mut buf).is_err() {
        // 应急回退（正常环境 getrandom 不会失败）：时间戳低 32 位 + 地址熵。
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let mixed = (now ^ (buf.as_ptr() as u64)).to_le_bytes();
        buf.copy_from_slice(&mixed[..4]);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 条的视觉样式：`chips` = 紧凑图文（QuotaDock 风，默认）；`text` = 平铺数字。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StripStyle {
    Chips,
    Text,
}

impl StripStyle {
    /// 从配置字符串解析；**非法值一律钳回 `chips`**（手改 config.json 不炸）。
    pub fn parse(s: &str) -> Self {
        match s {
            "text" => StripStyle::Text,
            _ => StripStyle::Chips,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StripStyle::Chips => "chips",
            StripStyle::Text => "text",
        }
    }
}

impl Default for StripStyle {
    fn default() -> Self {
        StripStyle::Chips
    }
}

/// A provider's destination display. Legacy x_fraction is ignored by grouped layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderPosition {
    pub device: String,
    #[serde(default)]
    pub x_fraction: f64,
}

/// Per-provider information shown in the taskbar, panel and tooltip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayMode {
    #[default]
    All,
    Plan,
    Balance,
}

impl DisplayMode {
    pub const ALL: [Self; 3] = [Self::All, Self::Plan, Self::Balance];
    pub fn label(self) -> &'static str {
        match self { Self::All => "套餐额度和余额", Self::Plan => "仅套餐额度", Self::Balance => "仅余额" }
    }
    pub fn project(self, mut row: crate::model::ProviderSnapshot) -> crate::model::ProviderSnapshot {
        match self {
            Self::All => {},
            Self::Plan => { row.balance = None; row.use_balance = false; },
            Self::Balance => {
                row.rolling = None; row.weekly = None; row.monthly = None;
                row.level = None; row.use_balance = false;
                if row.balance.is_some() { row.error = None; row.message = None; }
            },
        }
        if self != Self::All && !row.has_data() && row.error.is_none() {
            row.error = Some("no-data".into());
            row.message = Some(match self {
                Self::Plan => "平台未返回套餐额度",
                _ => "平台未返回余额",
            }.into());
        }
        row
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 轮询间隔秒（dsh 插件同款默认 60s）。
    pub poll_seconds: u64,
    pub strip_visible: bool,
    pub autostart: bool,
    /// 任务栏位置："left" | "right"（默认）。
    pub strip_side: String,
    /// 所有显示器都显示（false = 仅主屏）。
    /// 仅当 `display_devices = None`（旧模式）时生效。
    pub show_all_monitors: bool,
    /// 显示器白名单（多选）：`None` = 沿用旧 `show_all_monitors` 布尔语义
    /// （**完全兼容旧配置文件**，缺字段落到 None）；`Some(v)` = 只在这些
    /// `szDevice`（`\\.\DISPLAY1` 这类）上显示，**主屏也受管**。
    pub display_devices: Option<Vec<String>>,
    /// 账号显示白名单（多选）：`None` = 显示**全部**账号
    /// （默认，旧配置缺字段落 None 完全兼容）；`Some(v)` = 只显示 v 中列出的
    /// 账号（空 vec = 全部账号行隐藏）。
    ///
    /// 键 = `"{provider}:{account_id}"`（如 `antigravity:983960aba4e3fcf0bd4b2221`、
    /// `mimo:3f2a1b0c`）；**裸 id（不含冒号）是旧格式**，只匹配 Antigravity 账号
    /// ——旧配置无需迁移即保持原语义（见 [`should_show_account`]）。
    pub display_accounts: Option<Vec<String>>,
    /// Missing entries retain the old behavior (show both categories).
    pub display_modes: BTreeMap<String, DisplayMode>,
    pub provider_positions: BTreeMap<String, ProviderPosition>,
    pub provider_order: Vec<String>,
    /// 条内文字缩放（相对 96 DPI 设计值）。
    pub text_pct: u32,
    /// 条视觉样式："chips"（紧凑图文，默认）| "text"（平铺数字）。
    /// 结构级 `#[serde(default)]` 保证旧配置缺该字段时落到 Default（chips）。
    pub strip_style: String,
    /// 剩余额度气泡提醒。
    pub notify_low: bool,
    /// **套餐已过期就不显示**（默认 true）：上游明确告知套餐过期（MiMo
    /// `tokenPlan/detail.expired`、StepFun `subscription.expired_at` 已过）时，
    /// 整行从条/面板/告警里隐去 —— 过期套餐的 0% 窗口与余额没有参考价值。
    /// 设 false 则照常显示（`quota-bar json` 恒照常输出，与开关无关）。
    pub hide_expired_plans: bool,
    /// 剩余低于该百分比时提醒（10 = 剩余 ≤10% 已用 ≥90%）。
    pub low_threshold_percent: u8,
    /// 同一提醒的最短重发间隔（分钟）。
    pub resend_minutes: u64,
    /// provider 开关表；缺项 = 未启用。
    pub providers: BTreeMap<String, ProviderEntry>,
}

impl Default for Settings {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        // 核心三家开箱即显示（没配凭证时优雅显示"未配置"，不报错刷屏）。
        for id in [
            ProviderId::Mimo,
            ProviderId::OpenCodeGo,
            ProviderId::Antigravity,
        ] {
            providers.insert(id.as_str().to_string(), ProviderEntry::enabled());
        }
        Self {
            poll_seconds: 60,
            strip_visible: true,
            autostart: false,
            strip_side: "right".into(),
            show_all_monitors: true,
            display_devices: None,
            display_accounts: None,
            display_modes: BTreeMap::new(),
            provider_positions: BTreeMap::new(),
            provider_order: Vec::new(),
            text_pct: DEFAULT_TEXT_PCT,
            strip_style: StripStyle::default().as_str().into(),
            notify_low: true,
            hide_expired_plans: true,
            low_threshold_percent: 10,
            resend_minutes: 30,
            providers,
        }
    }
}

impl Settings {
    pub fn ordered_providers(&self) -> Vec<ProviderId> {
        let mut order = Vec::new();
        for key in &self.provider_order {
            if let Some(provider) = ProviderId::parse(key) {
                if !order.contains(&provider) { order.push(provider); }
            }
        }
        for provider in ProviderId::ALL { if !order.contains(&provider) { order.push(provider); } }
        order
    }
    pub fn swap_providers(&mut self, first: ProviderId, second: ProviderId) {
        let mut order = self.ordered_providers();
        let a = order.iter().position(|p| *p == first).unwrap();
        let b = order.iter().position(|p| *p == second).unwrap();
        order.swap(a,b);
        self.provider_order = order.iter().map(|p| p.as_str().into()).collect();
    }
    pub fn display_mode(&self, id: ProviderId) -> DisplayMode {
        self.display_modes.get(id.as_str()).copied().unwrap_or_default()
    }


    pub fn path() -> PathBuf {
        let dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("QuotaBar").join("config.json")
    }

    pub fn load() -> Self {
        let p = Self::path();
        let mut s = match std::fs::read_to_string(&p) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        // 旧单账号 `secret_ref` → 物化进 `accounts`（幂等；不写盘，等下次 save 落盘）。
        s.normalize();
        s
    }

    /// 全表归一（见 [`ProviderEntry::normalize`]）。**任何**改动 provider 表的
    /// 路径都应经过它，保证 `accounts` 与 `secret_ref` 镜像永远一致。
    /// 顺带修一次显示白名单里的**陈旧键**（见 `repair_display_accounts`）。
    pub fn normalize(&mut self) {
        for (key, entry) in self.providers.iter_mut() {
            entry.normalize(ProviderId::parse(key));
        }
        self.repair_display_accounts();
    }

    /// 修复显示白名单里指向**不存在账号**的键：
    ///
    /// 早期写入路径曾把 keyring 引用的**后缀**当账号 id 写进白名单
    /// （`mimo:087fcf36` 而账号 id 实为 `8a2eb9a0`）——这类键永远匹配不上账号，
    /// 结果是"新加的账号在条上不显示"。若该后缀唯一命中某个账号的引用末尾，
    /// 就把键改写成那个账号的真实 id；认不出的键原样保留（绝不乱猜）。
    fn repair_display_accounts(&mut self) {
        let Some(list) = self.display_accounts.as_mut() else {
            return;
        };
        for key in list.iter_mut() {
            let Some((provider, stale)) = key.split_once(':') else {
                continue;
            };
            let Some(entry) = self.providers.get(provider) else {
                continue;
            };
            if entry.accounts.iter().any(|a| a.id == stale) {
                continue; // 已经是真实账号 id
            }
            let suffix = format!("/{stale}");
            let hit = entry
                .accounts
                .iter()
                .find(|a| a.secret_ref.ends_with(&suffix));
            if let Some(acc) = hit {
                *key = format!("{provider}:{}", acc.id);
            }
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        // 原子写：半截 JSON 会被 load() 当成损坏静默回退默认值，必须避免。
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn poll_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.poll_seconds.clamp(10, 3600))
    }

    pub fn text_pct(&self) -> u32 {
        self.text_pct.clamp(MIN_TEXT_PCT, MAX_TEXT_PCT)
    }

    pub fn dock_left(&self) -> bool {
        self.strip_side.eq_ignore_ascii_case("left")
    }

    /// 条视觉样式；非法值（手改 config.json）钳回 `chips`。
    pub fn style(&self) -> StripStyle {
        StripStyle::parse(&self.strip_style)
    }

    pub fn is_enabled(&self, id: ProviderId) -> bool {
        self.providers
            .get(id.as_str())
            .map(|e| e.enabled)
            .unwrap_or(false)
    }

    pub fn set_enabled(&mut self, id: ProviderId, enabled: bool) -> bool {
        let entry = self.providers.entry(id.as_str().to_string()).or_default();
        if entry.enabled == enabled {
            return false;
        }
        entry.enabled = enabled;
        true
    }

    pub fn secret_ref(&self, id: ProviderId) -> Option<&str> {
        self.providers
            .get(id.as_str())
            .and_then(|e| e.secret_ref.as_deref())
    }

    /// 该 provider 的全部账号（API key / Cookie 类；OAuth 类恒空 —— 见 `oauth`）。
    pub fn accounts(&self, id: ProviderId) -> &[ProviderAccount] {
        self.providers
            .get(id.as_str())
            .map(|e| e.accounts.as_slice())
            .unwrap_or(&[])
    }

    /// 追加一个账号（id 由调用方给定），返回该 id。
    ///
    /// 多账号写入路径（`accounts::add`）用**同一个 id** 同时做 keyring 引用后缀与
    /// 账号 id —— 两者必须一致，否则显示白名单会写进一个指向不存在账号的键。
    pub fn add_account_with_id(
        &mut self,
        id: ProviderId,
        account_id: &str,
        label: &str,
        secret_ref: String,
    ) {
        let entry = self.providers.entry(id.as_str().to_string()).or_default();
        entry.accounts.push(ProviderAccount {
            id: account_id.to_string(),
            label: label.trim().to_string(),
            secret_ref,
        });
        entry.normalize(Some(id));
    }

    /// 追加一个账号（id 随机生成），返回该 id。**仅测试用** —— 生产路径走
    /// [`Settings::add_account_with_id`]（keyring 引用后缀与账号 id 必须是同一个）。
    #[cfg(test)]
    pub fn add_account(&mut self, id: ProviderId, label: &str, secret_ref: String) -> String {
        let acc_id = new_account_id();
        self.add_account_with_id(id, &acc_id, label, secret_ref);
        acc_id
    }

    /// 移除账号；镜像 `secret_ref` 跟着前移（清空则回到 None）。返回是否有变更。
    pub fn remove_account(&mut self, id: ProviderId, account_id: &str) -> bool {
        let Some(entry) = self.providers.get_mut(id.as_str()) else {
            return false;
        };
        let before = entry.accounts.len();
        entry.accounts.retain(|a| a.id != account_id);
        if entry.accounts.len() == before {
            return false;
        }
        // 先清镜像再 normalize：否则被删账号的 ref 会被"物化"逻辑复活。
        entry.secret_ref = None;
        entry.normalize(Some(id));
        true
    }

    /// 启用列表（按 `ProviderId::ALL` 顺序，即核心三家在前）。
    pub fn enabled_providers(&self) -> Vec<ProviderId> {
        ProviderId::ALL
            .into_iter()
            .filter(|id| self.is_enabled(*id))
            .collect()
    }
}

/// 显示器门控真值表（strip 的期望实例集合与菜单勾选态**共用**同一函数）：
/// - `None`（旧模式）+ `legacy_all=true` → 恒显示；
/// - `None` + 仅主屏（`legacy_all=false`）→ 只有主屏显示；
/// - `Some(v)` → `v.contains(device)`——主屏身份与 `legacy_all` 均不参与
///   （白名单模式下主屏同样受管）。
pub fn should_show_on(
    filter: &Option<Vec<String>>,
    device: &str,
    is_primary: bool,
    legacy_all: bool,
) -> bool {
    match filter {
        Some(v) => v.iter().any(|d| d == device),
        None => legacy_all || is_primary,
    }
}

/// 账号显示白名单里的键（`"{provider}:{account_id}"`）。
pub fn account_key(provider: ProviderId, account_id: &str) -> String {
    format!("{}:{}", provider.as_str(), account_id)
}

/// 白名单键是否命中该账号：带前缀键按 provider+id 精确匹配；
/// **裸 id（无冒号）= 旧格式**，只对 Antigravity 生效（旧配置零迁移）。
pub fn account_key_matches(key: &str, provider: ProviderId, account_id: &str) -> bool {
    match key.split_once(':') {
        Some((p, a)) => p == provider.as_str() && a == account_id,
        None => provider == ProviderId::Antigravity && key == account_id,
    }
}

/// 白名单是否为**旧格式**（含裸 id）：旧配置里的名单只表达 Antigravity 账号
/// 意图（当时只有 AGY 多账号），因此对其它 provider 不生效。
pub fn is_legacy_account_filter(filter: &Option<Vec<String>>) -> bool {
    matches!(filter, Some(v) if v.iter().any(|k| !k.contains(':')))
}

/// 账号门控真值表（与 [`should_show_on`] 同构，菜单勾选态与条/面板/告警共用）：
/// `None` → 恒显示（全部账号）；`Some(v)` → 只显示白名单命中的账号
/// （空白名单 = 全部账号行隐藏）。
///
/// **旧格式兼容**：名单里含裸 id 时（旧配置）只作用于 Antigravity，其它
/// provider 的账号一律显示 —— 否则用户既有的 AGY 名单会静默隐藏别家的新账号行。
/// 菜单勾选一次即整份重算为带前缀的新格式，之后按值严格生效。
pub fn should_show_account(
    filter: &Option<Vec<String>>,
    provider: ProviderId,
    account_id: &str,
) -> bool {
    match filter {
        Some(v) => {
            if provider != ProviderId::Antigravity && is_legacy_account_filter(filter) {
                return true;
            }
            v.iter()
                .any(|k| account_key_matches(k, provider, account_id))
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_order_swaps_only_the_selected_pair_and_survives_save() {
        let mut settings = Settings::default();
        settings.swap_providers(ProviderId::Mimo,ProviderId::Codex);
        let restored: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        let order = restored.ordered_providers();
        assert_eq!(order[0],ProviderId::Codex);
        assert_eq!(order[4],ProviderId::Mimo);
        assert_eq!(order[1],ProviderId::MimoSg);
    }

    #[test]
    fn provider_positions_persist_without_changing_display_preferences() {
        let mut settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.provider_positions.is_empty());
        settings.provider_positions.insert("codex".into(), ProviderPosition {
            device: "display2".into(), x_fraction: 0.25 });
        settings.display_modes.insert("codex".into(), DisplayMode::Plan);
        let restored: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.provider_positions, settings.provider_positions);
        assert_eq!(restored.display_mode(ProviderId::Codex), DisplayMode::Plan);
        assert!(!restored.provider_positions.contains_key("mimo"));
    }

    #[test]
    fn display_modes_survive_save_and_legacy_config_keeps_both() {
        let mut settings: Settings = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(settings.display_mode(ProviderId::Codex), DisplayMode::All);
        settings.display_modes.insert("codex".into(), DisplayMode::Plan);
        settings.display_modes.insert("mimo".into(), DisplayMode::Balance);
        let restored: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.display_mode(ProviderId::Codex), DisplayMode::Plan);
        assert_eq!(restored.display_mode(ProviderId::Mimo), DisplayMode::Balance);
        assert_eq!(restored.display_mode(ProviderId::StepFun), DisplayMode::All);
    }

    #[test]
    fn display_projection_keeps_raw_data_and_reports_missing_selected_category() {
        use crate::model::{ProviderSnapshot, Money, MoneyUnit, UsageWindow};
        let mut raw = ProviderSnapshot::new(ProviderId::Mimo);
        raw.monthly = Some(UsageWindow::new_percent(35.0));
        raw.balance = Some(Money { amount: 12.34, unit: MoneyUnit::Cny });
        let plan = DisplayMode::Plan.project(raw.clone());
        assert!(plan.monthly.is_some() && plan.balance.is_none());
        let balance = DisplayMode::Balance.project(raw.clone());
        assert!(balance.monthly.is_none() && balance.balance.is_some());
        assert!(raw.monthly.is_some() && raw.balance.is_some());
        raw.error = Some("api".into());
        assert!(DisplayMode::Balance.project(raw.clone()).error.is_none());
        raw.error = None; raw.balance = None;
        assert_eq!(DisplayMode::Balance.project(raw).error.as_deref(), Some("no-data"));
    }

    #[test]
    fn defaults_enable_core_three_with_right_dock_and_60s() {
        let s = Settings::default();
        assert!(s.is_enabled(ProviderId::Mimo));
        assert!(s.is_enabled(ProviderId::OpenCodeGo));
        assert!(s.is_enabled(ProviderId::Antigravity));
        assert!(!s.is_enabled(ProviderId::Codex));
        assert!(!s.dock_left());
        assert!(s.show_all_monitors);
        assert_eq!(s.poll_seconds, 60);
        assert_eq!(
            s.enabled_providers(),
            vec![
                ProviderId::Mimo,
                ProviderId::OpenCodeGo,
                ProviderId::Antigravity
            ]
        );
    }

    #[test]
    fn legacy_json_without_new_fields_parses() {
        // 蓝项目式旧字段子集 + 缺 providers 表
        let s: Settings = serde_json::from_str(r#"{"poll_seconds":30}"#).unwrap();
        assert_eq!(s.poll_seconds, 30);
        // providers 缺失 → 整表走 Default（核心三家启用）
        assert!(s.is_enabled(ProviderId::Mimo));
        assert_eq!(s.text_pct(), DEFAULT_TEXT_PCT);
    }

    #[test]
    fn text_pct_clamps_hand_edited_values() {
        let mut s = Settings::default();
        s.text_pct = 5;
        assert_eq!(s.text_pct(), MIN_TEXT_PCT);
        s.text_pct = 9999;
        assert_eq!(s.text_pct(), MAX_TEXT_PCT);
    }

    #[test]
    fn enabled_and_secret_ref_roundtrip() {
        let mut s = Settings::default();
        assert!(s.set_enabled(ProviderId::DeepSeek, true));
        assert!(!s.set_enabled(ProviderId::DeepSeek, true));
        s.add_account(ProviderId::Mimo, "", "provider/mimo/api-key".into());
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(back.is_enabled(ProviderId::DeepSeek));
        assert_eq!(
            back.secret_ref(ProviderId::Mimo),
            Some("provider/mimo/api-key"),
            "首个账号即 secret_ref 镜像"
        );
    }

    #[test]
    fn poll_interval_clamps_to_sane_range() {
        let mut s = Settings::default();
        s.poll_seconds = 1;
        assert_eq!(s.poll_interval().as_secs(), 10);
        s.poll_seconds = 999999;
        assert_eq!(s.poll_interval().as_secs(), 3600);
    }

    // ---- 条样式（chips 默认 / 兼容 / 钳制 / roundtrip） ----

    #[test]
    fn strip_style_defaults_to_chips() {
        assert_eq!(Settings::default().strip_style, "chips");
        assert_eq!(Settings::default().style(), StripStyle::Chips);
        assert_eq!(StripStyle::default(), StripStyle::Chips);
        assert_eq!(StripStyle::Chips.as_str(), "chips");
        assert_eq!(StripStyle::Text.as_str(), "text");
    }

    #[test]
    fn legacy_json_without_strip_style_parses_to_chips() {
        // 旧配置文件缺该字段 → 结构级 serde(default) 落到 Default（chips）
        let s: Settings = serde_json::from_str(r#"{"poll_seconds":30}"#).unwrap();
        assert_eq!(s.poll_seconds, 30);
        assert_eq!(s.style(), StripStyle::Chips);
        // 显式 "text" 的旧文件（若有）原样保留
        let t: Settings = serde_json::from_str(r#"{"strip_style":"text"}"#).unwrap();
        assert_eq!(t.style(), StripStyle::Text);
    }

    #[test]
    fn illegal_strip_style_value_clamps_to_chips() {
        let mut s = Settings::default();
        s.strip_style = "weird".into();
        assert_eq!(s.style(), StripStyle::Chips);
        s.strip_style = String::new();
        assert_eq!(s.style(), StripStyle::Chips);
        s.strip_style = "TEXT".into(); // 大小写不敏感之外的一律非法
        assert_eq!(s.style(), StripStyle::Chips);
        s.strip_style = "text".into();
        assert_eq!(s.style(), StripStyle::Text);
        assert_eq!(StripStyle::parse("anything-else"), StripStyle::Chips);
    }

    #[test]
    fn strip_style_roundtrips_through_json() {
        let mut s = Settings::default();
        s.strip_style = "text".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.style(), StripStyle::Text);
        assert_eq!(back.strip_style, "text");

        s.strip_style = "chips".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.style(), StripStyle::Chips);
    }

    // ---- 多账号（ProviderEntry.accounts + 旧单账号物化） ----

    #[test]
    fn legacy_secret_ref_materializes_into_accounts_deterministically() {
        // 旧配置：只有 secret_ref、没有 accounts（API key 类 provider）
        let legacy: Settings = serde_json::from_str(
            r#"{"providers":{"opencode-go":{"enabled":true,"secret_ref":"provider/opencode-go/api-key"}}}"#,
        )
        .unwrap();
        assert!(
            legacy.accounts(ProviderId::OpenCodeGo).is_empty(),
            "反序列化本身不物化"
        );
        let mut s = legacy.clone();
        s.normalize();
        let accs = s.accounts(ProviderId::OpenCodeGo);
        assert_eq!(accs.len(), 1, "旧单账号物化成一条");
        assert_eq!(accs[0].secret_ref, "provider/opencode-go/api-key");
        assert_eq!(
            accs[0].id,
            legacy_account_id("provider/opencode-go/api-key"),
            "id = 确定性哈希（跨次加载稳定）"
        );
        assert_eq!(accs[0].id.len(), 8);
        assert!(accs[0].id.chars().all(|c| c.is_ascii_hexdigit()));
        // 幂等：再归一不追加、镜像不变
        let snapshot = accs.to_vec();
        s.normalize();
        assert_eq!(s.accounts(ProviderId::OpenCodeGo), snapshot.as_slice());
        assert_eq!(
            s.secret_ref(ProviderId::OpenCodeGo),
            Some("provider/opencode-go/api-key")
        );
    }

    /// Cookie 类 provider 的旧 `secret_ref`（历史 API key 槽）**不物化**：
    /// 拿 API key 当 Cookie 发会 401，单账号凭据在专用槽
    /// `provider/{id}/cookie`（见 providers::credentials）。
    /// 归一后该字段随镜像规则清空（配置表里不再有账号 → 无镜像）。
    #[test]
    fn cookie_kind_legacy_secret_ref_is_not_materialized() {
        let mut s: Settings = serde_json::from_str(
            r#"{"providers":{"mimo":{"enabled":true,"secret_ref":"provider/mimo/api-key"}}}"#,
        )
        .unwrap();
        s.normalize();
        assert!(
            s.accounts(ProviderId::Mimo).is_empty(),
            "Cookie 类不把 API key 槽当账号"
        );
        assert_eq!(
            s.secret_ref(ProviderId::Mimo),
            None,
            "镜像规则：无账号 → 无镜像（旧 API key 槽不再驱动任何抓取）"
        );
        // 显式添加的 Cookie 账号照常保留并成为镜像
        let acc = s.add_account(ProviderId::Mimo, "主号", "provider/mimo/cookie/abc".into());
        assert_eq!(s.accounts(ProviderId::Mimo).len(), 1);
        assert_eq!(s.accounts(ProviderId::Mimo)[0].id, acc);
        assert_eq!(s.secret_ref(ProviderId::Mimo), Some("provider/mimo/cookie/abc"));
        // 再归一：Cookie 账号不被物化逻辑动到（幂等）
        s.normalize();
        assert_eq!(s.accounts(ProviderId::Mimo).len(), 1);
        assert_eq!(s.accounts(ProviderId::Mimo)[0].id, acc);
    }

    #[test]
    fn add_and_remove_accounts_keep_secret_ref_mirror_in_sync() {
        let mut s = Settings::default();
        let a = s.add_account(ProviderId::Mimo, "主号", "provider/mimo/cookie/aaa".into());
        let b = s.add_account(ProviderId::Mimo, "", "provider/mimo/cookie/bbb".into());
        assert_ne!(a, b, "新账号 id 互不相同");
        assert_eq!(s.accounts(ProviderId::Mimo).len(), 2);
        assert_eq!(
            s.secret_ref(ProviderId::Mimo),
            Some("provider/mimo/cookie/aaa"),
            "镜像 = 首个账号"
        );
        assert_eq!(s.accounts(ProviderId::Mimo)[0].label, "主号");
        assert_eq!(s.accounts(ProviderId::Mimo)[1].label, "", "空 label 原样保存");
        // 删首个 → 镜像前移到剩余账号
        assert!(s.remove_account(ProviderId::Mimo, &a));
        assert_eq!(s.accounts(ProviderId::Mimo).len(), 1);
        assert_eq!(s.secret_ref(ProviderId::Mimo), Some("provider/mimo/cookie/bbb"));
        // 删不存在的 → false 且不动
        assert!(!s.remove_account(ProviderId::Mimo, "nope"));
        assert_eq!(s.accounts(ProviderId::Mimo).len(), 1);
        // 删最后一个 → 空表 + 镜像清空，且不会在 normalize 里被复活
        assert!(s.remove_account(ProviderId::Mimo, &b));
        assert!(s.accounts(ProviderId::Mimo).is_empty());
        assert_eq!(s.secret_ref(ProviderId::Mimo), None);
        s.normalize();
        assert!(s.accounts(ProviderId::Mimo).is_empty(), "删除后不得复活");
    }

    /// 清空账号（CLI `--delete` 的落点 = 逐条 `remove_account`）：删光后
    /// `secret_ref` 镜像与 `normalize` 都不会把旧账号复活。
    #[test]
    fn removing_all_accounts_leaves_no_mirror_and_no_resurrection() {
        let mut s = Settings::default();
        let a = s.add_account(ProviderId::Mimo, "一号", "provider/mimo/api-key/1".into());
        let b = s.add_account(ProviderId::Mimo, "二号", "provider/mimo/api-key/2".into());
        assert!(s.remove_account(ProviderId::Mimo, &a));
        assert!(s.remove_account(ProviderId::Mimo, &b));
        assert!(s.accounts(ProviderId::Mimo).is_empty());
        assert_eq!(s.secret_ref(ProviderId::Mimo), None);
        // 再归一也不会复活（secret_ref 已随删除清空）
        s.normalize();
        assert!(s.accounts(ProviderId::Mimo).is_empty());
        assert!(!s.remove_account(ProviderId::Mimo, &a), "已删光 → 无变更");
    }

    #[test]
    fn accounts_roundtrip_through_json() {
        let mut s = Settings::default();
        let a = s.add_account(ProviderId::DeepSeek, "备用", "provider/deepseek/api-key/1".into());
        let json = serde_json::to_string(&s).unwrap();
        let mut back: Settings = serde_json::from_str(&json).unwrap();
        back.normalize();
        assert_eq!(back.accounts(ProviderId::DeepSeek).len(), 1);
        assert_eq!(back.accounts(ProviderId::DeepSeek)[0].id, a);
        assert_eq!(back.accounts(ProviderId::DeepSeek)[0].label, "备用");
    }

    #[test]
    fn new_account_ids_are_unique_8hex() {
        let a = new_account_id();
        let b = new_account_id();
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    // ---- 账号显示门控（should_show_account / account_key_matches） ----

    #[test]
    fn account_gate_truth_table_with_prefixed_and_legacy_keys() {
        // None → 恒显示（任何 provider / 账号）
        let none: Option<Vec<String>> = None;
        assert!(should_show_account(&none, ProviderId::Mimo, "3f2a1b0c"));
        assert!(should_show_account(&none, ProviderId::Antigravity, "abc"));
        // Some + 带前缀键：只命中该 provider 的该账号
        let some = Some(vec![account_key(ProviderId::Mimo, "3f2a1b0c")]);
        assert_eq!(account_key(ProviderId::Mimo, "3f2a1b0c"), "mimo:3f2a1b0c");
        assert!(should_show_account(&some, ProviderId::Mimo, "3f2a1b0c"));
        assert!(!should_show_account(&some, ProviderId::Mimo, "other"));
        // 同 id 不同 provider 不串门
        assert!(!should_show_account(&some, ProviderId::OpenCodeGo, "3f2a1b0c"));
        // 旧格式裸 id：只对 Antigravity 生效
        let legacy = Some(vec!["983960aba4e3fcf0bd4b2221".to_string()]);
        assert!(should_show_account(
            &legacy,
            ProviderId::Antigravity,
            "983960aba4e3fcf0bd4b2221"
        ));
        assert!(
            should_show_account(&legacy, ProviderId::Mimo, "983960aba4e3fcf0bd4b2221"),
            "旧格式名单（含裸 id）不影响非 AGY provider"
        );
        assert!(!should_show_account(&legacy, ProviderId::Antigravity, "other"));
        assert!(is_legacy_account_filter(&legacy));
        assert!(!is_legacy_account_filter(&Some(vec![account_key(
            ProviderId::Mimo,
            "3f2a1b0c"
        )])));
        assert!(!is_legacy_account_filter(&None));
        // 空白名单 → 全隐藏
        assert!(!should_show_account(&Some(Vec::new()), ProviderId::Mimo, "x"));
        // 前缀解析：冒号后为空 / 前缀是别的 provider 都不命中
        assert!(!should_show_account(
            &Some(vec!["mimo:".into()]),
            ProviderId::Mimo,
            "3f2a1b0c"
        ));
        assert!(!should_show_account(
            &Some(vec!["codex:abc".into()]),
            ProviderId::Antigravity,
            "abc"
        ));
    }

    /// 白名单陈旧键自愈：早期写入路径把 keyring 引用后缀当账号 id 写进白名单
    /// （`mimo:087fcf36` 而账号 id 是 `8a2eb9a0`）→ 永远匹配不上、新账号不上条。
    /// 归一后应改写成真实账号 id；认不出的键原样保留。
    #[test]
    fn normalize_repairs_stale_display_keys_via_ref_suffix() {
        let mut s: Settings = serde_json::from_str(
            r#"{"display_accounts":["mimo:087fcf36","deepseek:deadbeef","antigravity:abc"],
                "providers":{"mimo":{"enabled":true,"accounts":[
                    {"id":"8a2eb9a0","label":"","secret_ref":"provider/mimo/cookie/087fcf36"}]}}}"#,
        )
        .unwrap();
        s.normalize();
        let list = s.display_accounts.clone().unwrap();
        assert!(list.contains(&"mimo:8a2eb9a0".to_string()), "{list:?}");
        assert!(!list.contains(&"mimo:087fcf36".to_string()), "{list:?}");
        // 认不出的键（无对应账号 / 非本表 provider）原样保留
        assert!(list.contains(&"deepseek:deadbeef".to_string()), "{list:?}");
        assert!(list.contains(&"antigravity:abc".to_string()), "{list:?}");
        // 幂等：再归一不再变化
        let once = list.clone();
        s.normalize();
        assert_eq!(s.display_accounts.unwrap(), once);
        // None 模式不受影响
        let mut none = Settings::default();
        none.normalize();
        assert_eq!(none.display_accounts, None);
    }

    // ---- 显示器白名单（display_devices） ----

    #[test]
    fn display_devices_defaults_none_and_legacy_json_stays_none() {
        // 默认 = None（沿用旧 show_all_monitors 布尔语义）
        assert_eq!(Settings::default().display_devices, None);
        // 旧配置文件缺该字段 → None（完全兼容）
        let legacy: Settings = serde_json::from_str(r#"{"poll_seconds":30}"#).unwrap();
        assert_eq!(legacy.display_devices, None);
        assert!(legacy.show_all_monitors);
        // Some 白名单 roundtrip
        let mut s = Settings::default();
        s.display_devices = Some(vec!["\\\\.\\DISPLAY1".into(), "\\\\.\\DISPLAY2".into()]);
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.display_devices,
            Some(vec!["\\\\.\\DISPLAY1".into(), "\\\\.\\DISPLAY2".into()])
        );
    }

    /// 门控真值表：None 走旧布尔语义；Some 按值（主屏也受管）。
    #[test]
    fn should_show_on_truth_table() {
        let none: Option<Vec<String>> = None;
        // None + 全部屏 → 恒显示（主屏与否都显示）
        assert!(should_show_on(&none, "\\\\.\\DISPLAY1", true, true));
        assert!(should_show_on(&none, "\\\\.\\DISPLAY2", false, true));
        // None + 仅主屏 → 只有主屏
        assert!(should_show_on(&none, "\\\\.\\DISPLAY1", true, false));
        assert!(!should_show_on(&none, "\\\\.\\DISPLAY2", false, false));
        // Some → 只看白名单，主屏身份/旧布尔都不参与
        let some = Some(vec!["\\\\.\\DISPLAY1".to_string()]);
        assert!(should_show_on(&some, "\\\\.\\DISPLAY1", true, true));
        assert!(should_show_on(&some, "\\\\.\\DISPLAY1", true, false));
        // 主屏不在白名单 → 即使它是主屏、即使 legacy_all=true 也不显示
        assert!(!should_show_on(&some, "\\\\.\\DISPLAY3", true, true));
        // 副屏在白名单 → 显示
        let some2 = Some(vec!["\\\\.\\DISPLAY2".to_string()]);
        assert!(should_show_on(&some2, "\\\\.\\DISPLAY2", false, false));
        // 空白名单 → 全都不显示（托盘仍在）
        let empty = Some(Vec::new());
        assert!(!should_show_on(&empty, "\\\\.\\DISPLAY1", true, true));
    }

    // ---- 账号显示白名单（display_accounts）——与 display_devices 同款迁移语义 ----

    #[test]
    fn display_accounts_defaults_none_and_legacy_json_stays_none() {
        // 默认 = None → 显示全部账号
        assert_eq!(Settings::default().display_accounts, None);
        // 旧配置文件缺该字段 → None（完全兼容）
        let legacy: Settings = serde_json::from_str(r#"{"poll_seconds":30}"#).unwrap();
        assert_eq!(legacy.display_accounts, None);
        // Some 白名单 roundtrip（含空表 = 全隐藏账号行）
        let mut s = Settings::default();
        s.display_accounts = Some(vec!["983960aba4e3fcf0bd4b2221".into()]);
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.display_accounts,
            Some(vec!["983960aba4e3fcf0bd4b2221".to_string()])
        );
        s.display_accounts = Some(Vec::new());
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.display_accounts, Some(Vec::new()));
    }
}
