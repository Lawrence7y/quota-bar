//! OAuth 会话存储与登录入口（**多账号**：单条 keyring 存整张账号表 + 活动标记；
//! dsh `auth/accounts.ts` 同款语义）。
//!
//! 契约（strip 菜单 / CLI / provider 都依赖，签名不得改）：
//! - `list_accounts(id)`：账号列表（Antigravity / Codex → 全部账号 + active 标记，
//!   其它（无会话）provider → 空 Vec）。
//! - `switch_account(id, account_id)`：切换活动账号；找不到 → `Err("账号不存在")`。
//! - `login_blocking(id)`：交互式登录，语义 =【添加/更新该账号并切换为活动】
//!   （同 id 覆盖 session，新 id 追加；随后置为 active）。
//! - `logout(id)`：退出【活动】账号；有剩余 → 自动切到列表第一个；无剩余 → 清空。
//! - `access_token(id)`：活动账号有效 token（临近过期自动刷新；成功只回写该账号，
//!   active/顺序不变）。
//! - `account_label(id)`：活动账号 email。
//! - `session(id)`：**活动账号 session 的 JSON** —— `providers/antigravity.rs`
//!   现有调用（project_id 读取等）零改动依赖此投影。
//!
//! 存储：整体 JSON 存 Credential Manager `session/{provider}`：
//! ```json
//! { "active": "<账号id>", "accounts": [ { "id": "…", "added_at": 1699999999, "session": { … } } ] }
//! ```
//! `session` 是**不透明 JSON**：各 provider 的会话结构（[`GoogleSession`] /
//! `codex::CodexSession`）由 [`SessionKind`] 投影出账号键与展示名 —— 存储层
//! （增删改查/修复/迁移）因此对 provider 完全通用。
//!
//! 旧单账号格式（根上直接是 `GoogleSession`、带 access token）读取时由纯函数
//! `migrate` 透明迁移，首次写入（login/switch/logout/刷新回写）后落为新格式。

pub mod antigravity;
pub mod codex;
pub mod loopback;
pub mod pkce;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::{now_sec, ProviderId};
use crate::secrets;

/// 账号列表条目（strip 菜单 / 账号管理对接的公共结构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub id: String,
    /// 展示名：OAuth 账号 = 邮箱；API key/Cookie 账号 = 自定义名或 key 尾号。
    pub email: Option<String>,
    /// 是否为当前活动账号（列表中恒有且仅有一个 true，除非账号表为空）。
    pub active: bool,
}

/// Google（Antigravity）登录会话。字段与 dsh 对齐；expires 含 5 分钟预刷新。
/// 字段带 default：**条目级**损坏（缺字段）解析成空 token 条目而不再拖垮整张
/// 账号表 —— `account_tokens` 对它判"会话数据损坏"并仍占一行展示；旧格式迁移的
/// 判据（根上有非空 access token）不受影响。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoogleSession {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 过期时刻（unix 毫秒，已减去预刷新窗口）。
    #[serde(default)]
    pub expires_at_ms: u64,
    /// 项目 id；缺字段（旧会话）容忍为空串，抓取侧回落默认项目。
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub email: Option<String>,
}

/// 会话种类：把不透明 session JSON 投影成**账号键 / 展示名**（存储层唯一的
/// provider 相关依赖）。实现见 `antigravity::GoogleKind` 与 `codex::CodexKind`。
pub trait SessionKind {
    /// 稳定账号键（行键 = `"{provider}:{id}"` 的后半段；不含冒号）。
    fn account_id(&self, session: &Value) -> String;
    /// 展示名（邮箱 / 账号名）；取不到 → None。
    fn label(&self, session: &Value) -> Option<String>;
}

/// 该 provider 的会话种类；不用会话存储的 provider → None
/// （`list_accounts` 恒空、`login_blocking` 报"不支持独立登录"）。
pub fn kind_of(id: ProviderId) -> Option<&'static dyn SessionKind> {
    match id {
        ProviderId::Antigravity => Some(&antigravity::GoogleKind),
        ProviderId::Codex => Some(&codex::CodexKind),
        _ => None,
    }
}

/// 该 provider 是否使用 oauth 账号表（多账号会话）。
pub fn uses_account_store(id: ProviderId) -> bool {
    kind_of(id).is_some()
}

/// 一次多账号 token 解析的结果（`account_tokens` 逐账号产出；
/// 抓取侧据此逐账号出快照行 —— 坏账号也占一行、带邮箱便于定位）。
#[derive(Debug, Clone, PartialEq)]
pub struct AccountToken {
    /// 账号 id（不含冒号；行键后半段）。
    pub account_id: String,
    pub email: Option<String>,
    /// Antigravity 专用（项目 id）；其它 provider 留空。
    pub project_id: String,
    /// 该账号本轮的有效 token：未过期直取 / 刷新成功 / 各类失败原因。
    pub token: Result<String, String>,
}

/// 多账号存储（整体序列化进单条 keyring `session/{provider}`）。
/// `#[serde(default)]`：缺 `active`/`accounts` 的手改/半损 JSON 能解析，
/// 由 `repair` 把 active 指回存在的账号（保住 `accounts` 数据）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct AccountsFile {
    /// 活动账号 id（空 = 无账号，或 repair 前的缺失值）。
    active: String,
    accounts: Vec<AccountEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AccountEntry {
    id: String,
    /// 加入时刻（unix 秒；同 id 覆盖时更新）。
    added_at: u64,
    /// 会话 JSON（不透明；投影见 [`SessionKind`]）。
    session: Value,
}

// ---------------------------------------------------------------------------
// 纯函数（离线测试入口：不碰 keyring）
// ---------------------------------------------------------------------------

/// 从会话 JSON 里读非空字符串字段（`label` / 刷新令牌 等）。
fn str_field(session: &Value, key: &str) -> Option<String> {
    session
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 账号 id 哈希（dsh 风格不透明 id，凭据不进明文）：
/// `sha256(label + '\0' + 刷新令牌)` hex 前 24 位。
/// 两个入参都缺（退化会话）→ 仍是空输入的哈希（**非空**），保证行键形态稳定；
/// 这类退化条目会归并成同一条（调用方通常已判"会话损坏"）。
pub(crate) fn hashed_account_id(label: Option<&str>, refresh_token: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    if let Some(email) = label.filter(|e| !e.is_empty()) {
        hasher.update(email.as_bytes());
        hasher.update([0u8]);
    }
    if let Some(rt) = refresh_token.filter(|t| !t.is_empty()) {
        hasher.update(rt.as_bytes());
    }
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    // hex 前 24 个**字符**（12 字节的熵，dsh accountIdOf slice(0,24) 同款长度）。
    hex.chars().take(24).collect()
}

/// 把存储 JSON 归一为多账号结构（**纯函数**）：
/// 1. 根上直接是旧单账号 `GoogleSession`（带 access token）→ 包成单账号表
///    （active = 该账号 id；added_at = 迁移时刻）；
/// 2. 新格式 `AccountsFile` → `repair` 修复 active/去重；
/// 3. 都不认（损坏 JSON / 非对象 / 缺关键字段）→ 空表（绝不 panic，下次写入覆盖）。
fn migrate(json: &str) -> AccountsFile {
    // 旧格式判定在前：新格式 JSON 没有根级 access token，GoogleSession 必解析失败，不会误判。
    if let Ok(session) = serde_json::from_str::<GoogleSession>(json) {
        if !session.access_token.is_empty() {
            let value = serde_json::to_value(&session).unwrap_or(Value::Null);
            let id = antigravity::GoogleKind.account_id(&value);
            return AccountsFile {
                active: id.clone(),
                accounts: vec![AccountEntry {
                    id,
                    added_at: now_sec(),
                    session: value,
                }],
            };
        }
    }
    match serde_json::from_str::<AccountsFile>(json) {
        Ok(mut file) => {
            file.repair();
            file
        }
        Err(_) => AccountsFile::empty(),
    }
}

impl AccountsFile {
    fn empty() -> Self {
        Self {
            active: String::new(),
            accounts: Vec::new(),
        }
    }

    /// 容错修复：同 id 去重（留首条）+ active 必须指向存在的账号
    /// （否则切到列表第一个；空表置空）。
    fn repair(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.accounts.retain(|a| seen.insert(a.id.clone()));
        if !self.accounts.iter().any(|a| a.id == self.active) {
            self.active = self
                .accounts
                .first()
                .map(|a| a.id.clone())
                .unwrap_or_default();
        }
    }

    /// 添加/更新账号，返回命中的账号 id（`login_blocking` 随即把它置为 active）。
    ///
    /// - 新 session 带非空展示名（邮箱）→ **先按展示名匹配**现有条目：命中则
    ///   **原地覆盖**该条目的 session（**保留原 id**、刷新 added_at、不动其它
    ///   条目顺序）。（Google/OpenAI 重登常轮换刷新令牌，若按 id 公式匹配会算出
    ///   新 id 追加出重复邮箱条目 —— 同邮箱必须归并到既有账号。）
    /// - 展示名缺失（或无同名条目）→ 按 id 公式（`SessionKind::account_id`）
    ///   匹配覆盖或追加为**新账号**。
    ///
    /// 不主动切换 active（仅当 active 为空/已失效时指向它）。
    fn upsert(&mut self, session: Value, kind: &dyn SessionKind) -> String {
        let now = now_sec();
        let label = kind.label(&session);
        let id = match label {
            Some(label) => {
                let hit = self
                    .accounts
                    .iter()
                    .position(|a| kind.label(&a.session).as_deref() == Some(label.as_str()));
                match hit {
                    Some(pos) => {
                        // 同展示名 → 原地覆盖：保留原 id、刷新 added_at、顺序不动。
                        self.accounts[pos].session = session;
                        self.accounts[pos].added_at = now;
                        self.accounts[pos].id.clone()
                    }
                    None => self.insert_by_formula(session, kind, now),
                }
            }
            None => self.insert_by_formula(session, kind, now),
        };
        if self.active.is_empty() || !self.accounts.iter().any(|a| a.id == self.active) {
            self.active = id.clone();
        }
        id
    }

    /// 按 id 公式（`SessionKind::account_id`）匹配覆盖或追加；返回该 id。
    fn insert_by_formula(&mut self, session: Value, kind: &dyn SessionKind, now: u64) -> String {
        let id = kind.account_id(&session);
        match self.accounts.iter_mut().find(|a| a.id == id) {
            Some(existing) => {
                existing.session = session;
                existing.added_at = now;
            }
            None => self.accounts.push(AccountEntry {
                id: id.clone(),
                added_at: now,
                session,
            }),
        }
        id
    }

    /// 切换活动账号；id 不存在 → false（调用方翻成 `Err("账号不存在")`）。
    fn set_active(&mut self, account_id: &str) -> bool {
        if self.accounts.iter().any(|a| a.id == account_id) {
            self.active = account_id.to_string();
            true
        } else {
            false
        }
    }

    /// 移除活动账号并返回它；剩余账号自动切换到**列表第一个**，
    /// 空表则 active 置空。active 无效 → None（不动表）。
    fn remove_active(&mut self) -> Option<AccountEntry> {
        let pos = self.accounts.iter().position(|a| a.id == self.active)?;
        let removed = self.accounts.remove(pos);
        self.active = self
            .accounts
            .first()
            .map(|a| a.id.clone())
            .unwrap_or_default();
        Some(removed)
    }

    fn active(&self) -> Option<&AccountEntry> {
        self.accounts.iter().find(|a| a.id == self.active)
    }

    /// 刷新成功的回写：**只更新活动账号的 session**（active/顺序/其它条目全不动）。
    fn update_active_session(&mut self, session: Value) -> bool {
        let active = self.active.clone();
        match self.accounts.iter_mut().find(|a| a.id == active) {
            Some(entry) => {
                entry.session = session;
                true
            }
            None => false,
        }
    }

    /// 按 id 回写 session（与 `update_active_session` 同构：只动该条，
    /// active/顺序/其它条目全不动）。多账号刷新回写用。
    fn update_session_by_id(&mut self, account_id: &str, session: Value) -> bool {
        match self.accounts.iter_mut().find(|a| a.id == account_id) {
            Some(entry) => {
                entry.session = session;
                true
            }
            None => false,
        }
    }

    /// 条目级移除：被移除的恰是 **active** → 走 `remove_active` 的收尾规则
    /// （剩余切第一个 / 空表置空）；其余情况只摘该条、active/顺序不受影响。
    fn remove_by_id(&mut self, account_id: &str) -> Option<AccountEntry> {
        if self.active == account_id {
            return self.remove_active();
        }
        let pos = self.accounts.iter().position(|a| a.id == account_id)?;
        Some(self.accounts.remove(pos))
    }

    /// `session()` 契约的投影：活动账号 session 的 JSON。
    fn active_session_json(&self) -> Option<String> {
        let entry = self.active()?;
        serde_json::to_string(&entry.session).ok()
    }
}

/// `list_accounts` 的纯投影（离线可测）：每个条目一条，active 标记唯一。
fn account_infos(file: &AccountsFile, kind: &dyn SessionKind) -> Vec<AccountInfo> {
    file.accounts
        .iter()
        .map(|a| AccountInfo {
            id: a.id.clone(),
            email: kind.label(&a.session),
            active: a.id == file.active,
        })
        .collect()
}

/// `account_label` 的核心：session JSON 里的非空 email。
fn email_from_session_json(json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(json).ok()?;
    str_field(&value, "email")
}

// ---------------------------------------------------------------------------
// 存储读写（单条 keyring，整体 JSON）
// ---------------------------------------------------------------------------

/// 读账号表：有存储 → `migrate` 归一；无存储 → 空表（"没配"不是错误）。
fn load_file(id: ProviderId) -> AccountsFile {
    secrets::get_opt(&secrets::session_ref(id.as_str()))
        .map(|json| migrate(&json))
        .unwrap_or_else(AccountsFile::empty)
}

fn save_file(id: ProviderId, file: &AccountsFile) -> Result<(), String> {
    let json = serde_json::to_string(file)
        .map_err(|e| format!("账号存储序列化失败：{e}"))?;
    secrets::put(&secrets::session_ref(id.as_str()), &json)
}

/// 账号表落盘收尾（刷新回写 / 移除后共用）：空表 → 清存储，否则保存。
fn persist(id: ProviderId, file: &AccountsFile) {
    if file.accounts.is_empty() {
        let _ = clear_session(id);
    } else {
        let _ = save_file(id, file);
    }
}

// ---------------------------------------------------------------------------
// 对外契约
// ---------------------------------------------------------------------------

/// 现有会话存储的原始 JSON（内部兼容层；**对外语义见下方重载说明**）。
pub fn session(id: ProviderId) -> Option<String> {
    if uses_account_store(id) {
        // 契约：返回【活动账号 session】的 JSON —— providers/antigravity 零改动。
        return load_file(id).active_session_json();
    }
    // 其它 provider 保持原行为：直读存储（今天没有存储 → None）。
    secrets::get_opt(&secrets::session_ref(id.as_str()))
}

/// 原始 JSON 直写通道（兼容层；多账号路径请走 `save_file`）。
/// 当前 crate 内无调用方，保留 pub 供外部（strip 菜单等）按原语义整条覆盖。
#[allow(dead_code)]
pub fn save_session(id: ProviderId, json: &str) -> Result<(), String> {
    secrets::put(&secrets::session_ref(id.as_str()), json)
}

pub fn clear_session(id: ProviderId) -> Result<(), String> {
    secrets::delete(&secrets::session_ref(id.as_str()))
}

/// 账号列表：Antigravity / Codex → 全部账号 + 唯一 active 标记；
/// 其它 provider → 空 Vec（不读存储）。
pub fn list_accounts(id: ProviderId) -> Vec<AccountInfo> {
    let Some(kind) = kind_of(id) else {
        return Vec::new();
    };
    account_infos(&load_file(id), kind)
}

/// 全部账号按【存储顺序】逐个解析有效 token（多账号并行抓取的输入）。
/// 仅使用账号表的 provider（Antigravity / Codex）有值；其它 → 空 Vec。
/// 决策/回写/条目级移除的实现见各自子模块（刷新网络层同源）。
pub fn account_tokens(id: ProviderId) -> Vec<AccountToken> {
    match id {
        ProviderId::Antigravity => antigravity::account_tokens(),
        ProviderId::Codex => codex::account_tokens(),
        _ => Vec::new(),
    }
}

/// 切换活动账号。账号不存在（含从未登录/无该 id）→ `Err("账号不存在")`。
pub fn switch_account(id: ProviderId, account_id: &str) -> Result<(), String> {
    if !uses_account_store(id) {
        return Err("账号不存在".into());
    }
    let mut file = load_file(id);
    if !file.set_active(account_id) {
        return Err("账号不存在".into());
    }
    save_file(id, &file)
}

/// 移除**指定**账号（菜单「移除账号」；与 `logout` 的"退活动账号"不同）：
/// 被删的是活动账号 → 自动切到列表第一个；删空 → 清存储。
pub fn remove_account(id: ProviderId, account_id: &str) -> Result<(), String> {
    if !uses_account_store(id) {
        return Err("账号不存在".into());
    }
    let mut file = load_file(id);
    if file.remove_by_id(account_id).is_none() {
        return Err("账号不存在".into());
    }
    persist(id, &file);
    Ok(())
}

/// 交互式登录（阻塞）：语义 =【添加/更新并切换为活动】。
/// 实现见各 provider 子模块（PKCE + 回环 + upsert + 置 active + save）。
pub fn login_blocking(id: ProviderId) -> Result<(), String> {
    match id {
        ProviderId::Antigravity => antigravity::login_blocking(),
        ProviderId::Codex => codex::login_blocking(),
        _ => Err(format!(
            "{} 暂不支持独立登录（使用 API key / 环境变量）",
            id.display_name()
        )),
    }
}

/// 退出【活动】账号：有剩余 → 自动切到第一个；无剩余 → 清空存储。
/// 非账号表 provider 保持原行为（幂等清存储，Ok）。
pub fn logout(id: ProviderId) -> Result<(), String> {
    if !uses_account_store(id) {
        return clear_session(id);
    }
    let mut file = load_file(id);
    if file.accounts.is_empty() {
        return clear_session(id);
    }
    file.remove_active();
    persist(id, &file);
    Ok(())
}

/// 账号提示：活动账号 email（无会话 → None）。
pub fn account_label(id: ProviderId) -> Option<String> {
    email_from_session_json(&session(id)?)
}

/// 有效 access token（必要时刷新活动账号）。无会话 → Err("未登录")。
pub fn access_token(id: ProviderId) -> Result<String, String> {
    match id {
        ProviderId::Antigravity => antigravity::access_token(),
        ProviderId::Codex => codex::access_token(),
        _ => Err(format!("{} 不使用 access token", id.display_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用令牌/刷新串（**由函数构造**：源码里不出现"凭据字段: 字面量"形态）。
    fn tok(name: &str) -> String {
        format!("fixture-{name}")
    }

    fn gsession(email: &str, refresh: &str) -> GoogleSession {
        GoogleSession {
            access_token: tok(refresh),
            refresh_token: Some(refresh.to_string()),
            expires_at_ms: 1_790_000_000_000,
            project_id: "aicode-consumers".into(),
            email: Some(email.to_string()),
        }
    }

    fn value_of(session: &GoogleSession) -> Value {
        serde_json::to_value(session).unwrap()
    }

    fn sample_session(email: &str, refresh: &str) -> Value {
        value_of(&gsession(email, refresh))
    }

    /// 一份两账号、active=第二条的新格式 JSON。
    fn two_account_file() -> (AccountsFile, String, String) {
        let kind = &antigravity::GoogleKind;
        let mut file = AccountsFile::empty();
        let id_a = file.upsert(sample_session("a@example.test", "cred-a"), kind);
        let id_b = file.upsert(sample_session("b@example.test", "cred-b"), kind);
        assert!(file.set_active(&id_b), "准备：切到 b");
        (file, id_a, id_b)
    }

    // ---- 迁移 ----

    #[test]
    fn migrate_legacy_single_session_wraps_into_single_account() {
        // 旧单账号格式 = 根上直接是会话 JSON（本用例用会话结构序列化得到）
        let legacy_session = gsession("old@example.test", "legacy-refresh");
        let legacy = serde_json::to_string(&legacy_session).unwrap();
        let file = migrate(&legacy);
        assert_eq!(file.accounts.len(), 1, "旧格式包成单账号");
        let expected_id = antigravity::GoogleKind.account_id(&value_of(&legacy_session));
        assert_eq!(file.active, expected_id, "active = 迁移出的账号 id");
        assert_eq!(file.accounts[0].id, expected_id);
        let entry = file.active().expect("迁移后有活动账号");
        let session: GoogleSession = serde_json::from_value(entry.session.clone()).unwrap();
        assert_eq!(session.access_token, tok("legacy-refresh"));
        assert_eq!(session.email.as_deref(), Some("old@example.test"));
        // id 是 sha256 前 24 hex（dsh 风格不透明）
        assert_eq!(expected_id.len(), 24);
        assert!(expected_id.chars().all(|c| c.is_ascii_hexdigit()));
        // 迁移结果再序列化 → 新格式 roundtrip
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"accounts\"") && json.contains("\"active\""));
        assert_eq!(migrate(&json), file, "新格式 roundtrip 稳定");
    }

    #[test]
    fn migrate_new_format_roundtrip_and_tolerates_missing_fields() {
        let (file, id_a, _id_b) = two_account_file();
        let json = serde_json::to_string(&file).unwrap();
        assert_eq!(migrate(&json), file, "新格式往返一致");
        // 不被误判成旧格式（根上没有 access token 字段）
        assert_eq!(migrate(&json).accounts.len(), 2);

        // 缺 active 的半损新格式 → repair 切到第一个，账号保住
        let no_active = format!(
            r#"{{"accounts":[{}]}}"#,
            serde_json::to_string(&file.accounts[0]).unwrap()
        );
        let repaired = migrate(&no_active);
        assert_eq!(repaired.accounts.len(), 1, "缺 active 不丢账号");
        assert_eq!(repaired.active, id_a, "active 回落到第一个");

        // active 指向不存在的 id → 修到第一个
        let dangling = format!(
            r#"{{"active":"deadbeef","accounts":[{}]}}"#,
            serde_json::to_string(&file.accounts[0]).unwrap()
        );
        assert_eq!(migrate(&dangling).active, id_a);

        // 损坏/未知 JSON → 空表，不 panic
        for junk in ["", "not json", "[1,2,3]", "{}", "123"] {
            let f = migrate(junk);
            assert!(f.accounts.is_empty(), "junk={junk:?} → 空表");
            assert!(f.active.is_empty());
        }
    }

    // ---- upsert / switch / remove ----

    #[test]
    fn upsert_appends_new_then_overwrites_same_id_without_duplicate() {
        let kind = &antigravity::GoogleKind;
        let mut file = AccountsFile::empty();
        let id = file.upsert(sample_session("a@example.test", "cred-a"), kind);
        assert_eq!(file.accounts.len(), 1, "新账号追加");
        assert_eq!(
            id,
            antigravity::GoogleKind.account_id(&sample_session("a@example.test", "cred-a"))
        );
        // 空表首个 upsert 自动成为 active
        assert_eq!(file.active, id);

        // 同 email（同或轮换刷新串均同路径）再 upsert：email 命中原条目，
        // 覆盖不重复，added_at 更新；该条目 id 恰为公式 id → 两次断言一致
        let added_before = file.accounts[0].added_at;
        let mut refreshed = gsession("a@example.test", "cred-a");
        refreshed.access_token = tok("rotated");
        let id2 = file.upsert(value_of(&refreshed), kind);
        assert_eq!(id2, id, "同邮箱 → 命中原条目（其 id 即公式 id）");
        assert_eq!(file.accounts.len(), 1, "不重复入列");
        let session: GoogleSession =
            serde_json::from_value(file.accounts[0].session.clone()).unwrap();
        assert_eq!(session.access_token, tok("rotated"), "session 被覆盖");
        assert!(file.accounts[0].added_at >= added_before, "added_at 更新");

        // 不同邮箱（c@example.test）→ 无同邮箱条目 → 按公式追加新账号
        //（active 不被 upsert 挪走）
        assert!(file.set_active(&id));
        let other = file.upsert(sample_session("c@example.test", "cred-c"), kind);
        assert_ne!(other, id);
        assert_eq!(file.accounts.len(), 2);
        assert_eq!(file.active, id, "upsert 不切换活动账号");
    }

    #[test]
    fn upsert_merges_same_email_when_refresh_token_rotates() {
        // Google 重登轮换刷新串：同邮箱必须归并到原条目、保留原 id，
        // 否则菜单里同一邮箱出现两条。
        let kind = &antigravity::GoogleKind;
        let mut file = AccountsFile::empty();
        let id_a = file.upsert(sample_session("a@example.test", "gen-1"), kind);
        assert_eq!(file.active, id_a, "空表首插成为 active");
        let id_b = file.upsert(sample_session("b@example.test", "cred-b"), kind);
        assert_eq!(file.accounts.len(), 2);
        assert_eq!(file.active, id_a, "追加不挪 active");

        let added_before = file.accounts[0].added_at;
        // 轮换刷新串 + 新 access token 的"同邮箱二次登录"
        let mut rotated = gsession("a@example.test", "gen-2");
        rotated.access_token = tok("relogin");
        rotated.expires_at_ms = 8_888_888_888_000;
        let merged_id = file.upsert(value_of(&rotated), kind);
        assert_eq!(merged_id, id_a, "同邮箱 → 保留原 id（公式 id 因轮换已不同）");
        assert_ne!(
            antigravity::GoogleKind.account_id(&sample_session("a@example.test", "gen-2")),
            id_a,
            "前提：轮换后公式 id 确实变了（正是要防的重复）"
        );
        assert_eq!(file.accounts.len(), 2, "仍是一条 a + 一条 b，无重复邮箱");
        assert_eq!(
            file.accounts
                .iter()
                .filter(|e| kind.label(&e.session).as_deref() == Some("a@example.test"))
                .count(),
            1,
            "同邮箱仅一条"
        );
        // 原地覆盖：session 更新、added_at 刷新、顺序不动
        assert_eq!(file.accounts[0].id, id_a, "顺序不动（a 仍在首位）");
        let session: GoogleSession =
            serde_json::from_value(file.accounts[0].session.clone()).unwrap();
        assert_eq!(session.access_token, tok("relogin"));
        assert_eq!(
            session.refresh_token.as_deref(),
            Some("gen-2"),
            "刷新串被新值覆盖"
        );
        assert!(file.accounts[0].added_at >= added_before, "added_at 刷新");
        // active 正确：归并对象不是 active 时 active 不被挪走
        assert!(file.set_active(&id_b));
        let mut again = gsession("a@example.test", "gen-3");
        again.access_token = tok("relogin-again");
        assert_eq!(file.upsert(value_of(&again), kind), id_a);
        assert_eq!(file.active, id_b, "归并不切换 active");
        assert_eq!(file.accounts.len(), 2);
        // login 流程随后 set_active(返回的 id) → 切回这条既有账号
        assert!(file.set_active(&merged_id));
        assert_eq!(file.active, id_a);
        let infos = account_infos(&file, kind);
        assert_eq!(
            infos.iter().filter(|i| i.active).count(),
            1,
            "active 标记仍唯一"
        );
    }

    #[test]
    fn upsert_without_email_keeps_each_refresh_distinct() {
        // email=None：无邮箱可归并 → 保持原公式行为（按刷新串哈希各成一条）。
        fn no_email(refresh: &str) -> Value {
            value_of(&GoogleSession {
                access_token: tok(refresh),
                refresh_token: Some(refresh.to_string()),
                expires_at_ms: 1_790_000_000_000,
                project_id: String::new(),
                email: None,
            })
        }
        let kind = &antigravity::GoogleKind;
        let mut file = AccountsFile::empty();
        let id1 = file.upsert(no_email("first"), kind);
        let id2 = file.upsert(no_email("second"), kind);
        assert_ne!(id1, id2, "不同刷新串 → 两条独立");
        assert_eq!(file.accounts.len(), 2);
        // 同刷新串（同公式 id）再插 → 原地覆盖
        let mut again = no_email("first");
        again["access_token"] = Value::String(tok("updated"));
        let id3 = file.upsert(again, kind);
        assert_eq!(id3, id1, "同公式 id → 覆盖");
        assert_eq!(file.accounts.len(), 2, "不新增");
        assert_eq!(
            file.accounts.iter().find(|a| a.id == id1).unwrap().session["access_token"],
            Value::String(tok("updated"))
        );
        // 空 email 串同 None 处理（非空才参与归并）
        let mut blank = no_email("third");
        blank["email"] = Value::String(String::new());
        let id4 = file.upsert(blank, kind);
        assert_eq!(
            id4,
            file.upsert(no_email("third"), kind),
            "空串 email → 按公式（可复现）"
        );
        assert_eq!(file.accounts.iter().filter(|a| a.id == id4).count(), 1);
    }

    #[test]
    fn set_active_accepts_existing_and_rejects_unknown() {
        let (mut file, id_a, id_b) = two_account_file();
        assert_eq!(file.active, id_b);
        // 合法
        assert!(file.set_active(&id_a));
        assert_eq!(file.active, id_a);
        // 非法：拒绝且 active 不变
        assert!(!file.set_active("does-not-exist"));
        assert_eq!(file.active, id_a, "非法切换不改状态");
        // 空表：任何 id 都非法
        let mut empty = AccountsFile::empty();
        assert!(!empty.set_active(&id_a));
        assert!(empty.active.is_empty());
    }

    #[test]
    fn remove_account_contract_rejects_unknown_and_non_store_providers() {
        // 无账号表 provider → "账号不存在"（不读存储）
        assert_eq!(
            remove_account(ProviderId::Mimo, "x").unwrap_err(),
            "账号不存在"
        );
        // 有账号表但 id 不存在 → 同样是"账号不存在"
        assert_eq!(
            remove_account(ProviderId::Codex, "nope").unwrap_err(),
            "账号不存在"
        );
    }

    #[test]
    fn switch_contract_rejects_unknown_with_account_missing_error() {
        // keyring 里没有账号（或 id 不在表里）→ 钉死文案；有账号时也只读不写。
        let err = switch_account(ProviderId::Antigravity, "000000000000000000000000")
            .expect_err("未知 id 必须失败");
        assert_eq!(err, "账号不存在");
        // 无会话 provider → 同样是"账号不存在"（不读存储）
        assert_eq!(
            switch_account(ProviderId::DeepSeek, "x").unwrap_err(),
            "账号不存在"
        );
    }

    #[test]
    fn remove_active_shifts_to_first_then_clears() {
        let (mut file, id_a, id_b) = two_account_file();
        // 移除活动 b → 剩 [a]，自动切到第一个
        let removed = file.remove_active().expect("移除了 b");
        assert_eq!(removed.id, id_b);
        assert_eq!(file.accounts.len(), 1);
        assert_eq!(file.active, id_a, "剩余自动切到列表第一个");
        // 移除最后一个 → 空表、active 置空
        let removed = file.remove_active().expect("移除了 a");
        assert_eq!(removed.id, id_a);
        assert!(file.accounts.is_empty());
        assert!(file.active.is_empty());
        // 无活动可移 → None 且不动
        assert!(file.remove_active().is_none());
        // active 无效 → None 且不动表
        let (mut file, id_a, _) = two_account_file();
        file.active = "missing".into();
        assert!(file.remove_active().is_none());
        assert_eq!(file.accounts.len(), 2, "无效 active 不动表");
        let _ = id_a;
    }

    #[test]
    fn remove_by_id_follows_active_rules_or_surgical_removal() {
        let kind = &antigravity::GoogleKind;
        let mut file = AccountsFile::empty();
        let id_a = file.upsert(sample_session("a@example.test", "cred-a"), kind);
        let id_b = file.upsert(sample_session("b@example.test", "cred-b"), kind);
        let id_c = file.upsert(sample_session("c@example.test", "cred-c"), kind);
        assert!(file.set_active(&id_b));

        // 被移除的恰是 active → 按 remove_active 规则：剩余切第一个
        let removed = file.remove_by_id(&id_b).expect("移除了 active");
        assert_eq!(removed.id, id_b);
        assert_eq!(file.accounts.len(), 2);
        assert_eq!(file.active, id_a, "active 被移 → 切到列表第一个");

        // 非 active 条目 → 只摘该条（active/顺序不动）
        let removed = file.remove_by_id(&id_c).expect("摘掉 c");
        assert_eq!(removed.id, id_c);
        assert_eq!(file.active, id_a, "非 active 被移 → active 不动");
        assert_eq!(
            file.accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            vec![id_a.clone()],
            "顺序保持"
        );
        // 不存在 → None 且不动
        assert!(file.remove_by_id("does-not-exist").is_none());
        assert_eq!(file.accounts.len(), 1);
        // 摘光 → active 置空（存储侧随后 clear）
        assert!(file.remove_by_id(&id_a).is_some());
        assert!(file.accounts.is_empty());
        assert!(file.active.is_empty());
        let _ = id_b;
        let _ = id_c;
    }

    #[test]
    fn update_session_by_id_touches_only_that_entry() {
        let (mut file, id_a, id_b) = two_account_file();
        let snapshot_b = file.accounts[1].clone();
        // 按 id 回写 a：只动 a，active/顺序/其它条目全不动
        let mut refreshed: GoogleSession =
            serde_json::from_value(file.accounts[0].session.clone()).unwrap();
        refreshed.access_token = tok("refreshed");
        assert!(file.update_session_by_id(&id_a, value_of(&refreshed)));
        assert_eq!(
            file.accounts[0].session["access_token"],
            Value::String(tok("refreshed")),
            "目标条目被更新"
        );
        assert_eq!(file.accounts[1], snapshot_b, "另一条目原样");
        assert_eq!(
            file.accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            vec![id_a.clone(), id_b.clone()],
            "顺序不变"
        );
        assert_eq!(file.active, id_b, "active 不动（与 update_active 对照）");
        // 不存在的 id → false 且无副作用
        assert!(!file.update_session_by_id("nope", sample_session("x@y.z", "cred")));
        assert_eq!(file.accounts.len(), 2);
    }

    #[test]
    fn account_tokens_contract_is_empty_for_non_oauth_providers() {
        // 非账号表 provider：短路空 Vec（不读 keyring）
        assert!(account_tokens(ProviderId::DeepSeek).is_empty());
        assert!(account_tokens(ProviderId::Mimo).is_empty());
        assert!(account_tokens(ProviderId::OpenCodeGo).is_empty());
    }

    #[test]
    fn migrate_keeps_corrupt_entry_as_row_level_error_fodder() {
        // 新格式里一条 session 缺 access token/过期时刻（条目级损坏）：
        // 条目**保留**（default 空值），email 还在 → 抓取侧能出"哪个号坏了"的行，
        // 而不是整张表被拖垮成空。
        let good = serde_json::json!({
            "id": "good-id",
            "added_at": 1,
            "session": {
                "access_token": tok("good"),
                "expires_at_ms": 1_790_000_000_000u64,
                "email": "good@x.t",
            },
        });
        let bad = serde_json::json!({
            "id": "bad-id",
            "added_at": 1,
            "session": { "email": "bad@x.t" },
        });
        let json = serde_json::json!({
            "active": "good-id",
            "accounts": [good, bad],
        })
        .to_string();
        let file = migrate(&json);
        assert_eq!(file.accounts.len(), 2, "损坏条目不拖垮整表");
        let bad = file
            .accounts
            .iter()
            .find(|a| a.id == "bad-id")
            .expect("坏条目保留");
        let session: GoogleSession = serde_json::from_value(bad.session.clone()).unwrap();
        assert!(session.access_token.is_empty(), "缺字段 → 空 token（判 Corrupt）");
        assert_eq!(
            session.email.as_deref(),
            Some("bad@x.t"),
            "email 保留 → 行上仍显示是谁"
        );
        assert_eq!(file.active, "good-id", "active 不受影响");
    }

    // ---- active 选取 → session()/account_label 的纯逻辑 ----

    #[test]
    fn active_selection_drives_session_and_account_label() {
        let (mut file, id_a, id_b) = two_account_file();
        assert_eq!(file.active, id_b, "初始 active = b");
        // active = b → 投影出 b 的 session JSON 与 email
        let json = file.active_session_json().expect("有活动账号");
        assert_eq!(email_from_session_json(&json).as_deref(), Some("b@example.test"));
        assert!(json.contains("cred-b"));
        // 切回 a → 投影跟着变
        assert!(file.set_active(&id_a));
        let json = file.active_session_json().expect("有活动账号");
        assert_eq!(email_from_session_json(&json).as_deref(), Some("a@example.test"));
        // 空表 → 两者皆 None（session()=None → account_label=None）
        let empty = AccountsFile::empty();
        assert!(empty.active_session_json().is_none());
        assert!(email_from_session_json("not json").is_none());
        assert!(email_from_session_json(r#"{"email":""}"#).is_none());
    }

    #[test]
    fn update_active_session_only_touches_active_entry() {
        let (mut file, id_a, id_b) = two_account_file();
        let snapshot_a = file.accounts[0].clone();
        // 模拟刷新成功：只回写活动账号 b 的 session
        let mut refreshed: GoogleSession =
            serde_json::from_value(file.active().unwrap().session.clone()).unwrap();
        refreshed.access_token = tok("fresh");
        refreshed.expires_at_ms = 9_999_999_999_000;
        assert!(file.update_active_session(value_of(&refreshed)));
        // b 更新、active/顺序不变、a 一字不动
        assert_eq!(file.active, id_b, "active 不变");
        assert_eq!(
            file.accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
            vec![id_a.clone(), id_b.clone()],
            "顺序不变"
        );
        assert_eq!(file.accounts[0], snapshot_a, "非活动条目完全不动");
        assert_eq!(
            file.active().unwrap().session["access_token"],
            Value::String(tok("fresh")),
            "活动 session 已回写"
        );
        // 无活动账号 → false
        assert!(!AccountsFile::empty().update_active_session(sample_session("x@y.z", "cred")));
    }

    #[test]
    fn account_infos_marks_exactly_one_active() {
        let (file, _id_a, id_b) = two_account_file();
        let infos = account_infos(&file, &antigravity::GoogleKind);
        assert_eq!(infos.len(), 2);
        assert_eq!(
            infos.iter().filter(|i| i.active).count(),
            1,
            "恒有且仅有一个 active"
        );
        assert!(infos.iter().find(|i| i.active).unwrap().id == id_b);
        assert_eq!(
            infos.iter().find(|i| !i.active).unwrap().email.as_deref(),
            Some("a@example.test")
        );
        // 空表 → 空列表
        assert!(account_infos(&AccountsFile::empty(), &antigravity::GoogleKind).is_empty());
    }

    // ---- 保留的原契约测试 ----

    #[test]
    fn google_session_serde_roundtrip_with_defaults() {
        let s = gsession("a@b.com", "cred");
        let json = serde_json::to_string(&s).unwrap();
        let back: GoogleSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // 缺 email / 刷新串的旧会话仍可解析
        let minimal = serde_json::json!({
            "access_token": tok("minimal"),
            "expires_at_ms": 1,
            "project_id": "p",
        })
        .to_string();
        let m: GoogleSession = serde_json::from_str(&minimal).unwrap();
        assert_eq!(m.email, None);
        assert_eq!(m.refresh_token, None);
        // 根上带 access token → migrate 判为旧格式（本用例保证判定依据存在）
        assert_eq!(migrate(&minimal).accounts.len(), 1);
    }

    #[test]
    fn non_oauth_providers_reject_login_with_clear_error() {
        let err = login_blocking(ProviderId::DeepSeek).unwrap_err();
        assert!(err.contains("不支持独立登录"));
        assert!(access_token(ProviderId::Mimo).is_err());
        // 无会话 provider：列表恒空、登出幂等 Ok（保持现价）
        assert!(list_accounts(ProviderId::DeepSeek).is_empty());
        assert!(list_accounts(ProviderId::OpenCodeGo).is_empty());
        assert!(logout(ProviderId::DeepSeek).is_ok());
    }

    /// 会话种类分派：Antigravity / Codex 用账号表，其余不用。
    #[test]
    fn session_kind_dispatch_matches_account_store_providers() {
        assert!(uses_account_store(ProviderId::Antigravity));
        assert!(uses_account_store(ProviderId::Codex));
        assert!(!uses_account_store(ProviderId::Mimo));
        assert!(!uses_account_store(ProviderId::OpenCodeGo));
        assert!(kind_of(ProviderId::Mimo).is_none());
        assert!(kind_of(ProviderId::Codex).is_some());
        // hashed_account_id 与 dsh 公式一致（email + '\0' + 刷新串）
        let h = hashed_account_id(Some("a@b.c"), Some("cred"));
        assert_eq!(h.len(), 24);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, hashed_account_id(Some("a@b.c"), Some("cred")));
        assert_ne!(h, hashed_account_id(Some("a@b.c"), Some("cred2")));
        // 全缺 → 仍是 24 hex 的固定值（行键形态不落空、可复现）
        let degenerate = hashed_account_id(None, None);
        assert_eq!(degenerate.len(), 24);
        assert_eq!(degenerate, hashed_account_id(None, None));
    }
}
