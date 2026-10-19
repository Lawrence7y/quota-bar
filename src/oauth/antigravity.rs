//! Antigravity 的 Google OAuth：会话读写 + token 刷新 + 交互式登录。
//!
//! 移植 dsh `src/auth/pkce.ts` + `src/auth/oauth-flow.ts` + `src/providers/antigravity.ts`
//! 的 OAuth 半部（MIT）：
//! - 授权：`accounts.google.com/o/oauth2/v2/auth` + PKCE，回环
//!   `http://127.0.0.1:51121/oauth-callback`（dsh 同款 client_id / scope /
//!   `access_type=offline&prompt=consent`），含"粘贴回调 URL / 裸授权码"兜底；
//! - 交换/刷新：`POST oauth2.googleapis.com/token`（预刷新 5 分钟）；
//! - `client_secret` 在 dsh 源码里以 base64 串书写、**解码后使用**
//!   （`Buffer.from(..., 'base64').toString('utf-8')`），这里直接存解码值。

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{account_label, loopback, pkce, GoogleSession, SessionKind};
use crate::model::ProviderId;

/// Google 会话种类：账号键 = `sha256(email + '\0' + 刷新串)` 前 24 hex
/// （凭据不进明文）；展示名 = 邮箱。
pub struct GoogleKind;

impl SessionKind for GoogleKind {
    fn account_id(&self, session: &Value) -> String {
        let email = session.get("email").and_then(Value::as_str);
        let refresh = session.get("refresh_token").and_then(Value::as_str);
        super::hashed_account_id(email, refresh)
    }

    fn label(&self, session: &Value) -> Option<String> {
        session
            .get("email")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

/// 会话 JSON → 类型化会话（条目级损坏时按空会话处置：`decide_token` 判 Corrupt）。
fn session_of(value: &Value) -> GoogleSession {
    serde_json::from_value(value.clone()).unwrap_or(GoogleSession {
        access_token: String::new(),
        refresh_token: None,
        expires_at_ms: 0,
        project_id: String::new(),
        email: None,
    })
}

// ---- dsh antigravity.ts :45-70 的 OAuth 常量 ----
//
// 凭据不再硬编码：构建时通过环境变量注入，避免把第三方 OAuth 客户端密钥
// 带进源码仓库。缺失时回退为空串，相关登录流程会明确报错而不是静默使用占位值。
//
//   $env:ANTIGRAVITY_CLIENT_ID = "<google oauth client id>"
//   $env:ANTIGRAVITY_CLIENT_SECRET = "<google oauth client secret>"
//
// 上游常量（dsh 源码 antigravity.ts）：
//   client_id     1071006060591-...（见项目部署文档）
//   client_secret dsh 以 base64 串书写、解码后使用
const CLIENT_ID: &str = match option_env!("ANTIGRAVITY_CLIENT_ID") {
    Some(v) => v,
    None => "",
};
const CLIENT_SECRET: &str = match option_env!("ANTIGRAVITY_CLIENT_SECRET") {
    Some(v) => v,
    None => "",
};
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
/// dsh `ANTIGRAVITY_SCOPES`（空格连接，授权 URL 里整体编码）。
const SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/aicode ",
    "https://www.googleapis.com/auth/cloud-platform ",
    "https://www.googleapis.com/auth/userinfo.email ",
    "https://www.googleapis.com/auth/userinfo.profile ",
    "https://www.googleapis.com/auth/cclog ",
    "https://www.googleapis.com/auth/experimentsandconfigs",
);
/// dsh `ANTIGRAVITY_PREEMPT_MS`：写入的过期时刻已减 5 分钟 → 到点即刷新。
const PREEMPT_MS: u64 = 5 * 60_000;

// 项目发现（dsh `discoverProjectId` :367-404 的两个 host，与 provider 同源）。
const HOST_PRIMARY: &str = "https://daily-cloudcode-pa.googleapis.com";
const HOST_FALLBACK: &str = "https://cloudcode-pa.googleapis.com";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 会话过期判定（**注入时间**，便于测试边界）：`expires_at_ms` 已含 5 分钟
/// 预刷新，故 `now == expires_at_ms` 即视为过期、立即刷新。
pub(crate) fn token_expired(expires_at_ms: u64, now_ms: u64) -> bool {
    expires_at_ms <= now_ms
}

/// 新会话过期时刻：`now + expires_in*1000 - 5min`（dsh exchange/refresh 同式）。
pub(crate) fn compute_expiry(now_ms: u64, expires_in_s: u64) -> u64 {
    (now_ms + expires_in_s.saturating_mul(1000)).saturating_sub(PREEMPT_MS)
}

/// dsh `isPermanentRefreshError(invalid_grant, [400,401,403])`（:495-507）：
/// 刷新被拒且是这类结果 → 登录已死，删会话重新登录。
pub(crate) fn is_permanent_refresh_error(status: Option<u16>, oauth_code: Option<&str>) -> bool {
    if oauth_code == Some("invalid_grant") {
        return true;
    }
    matches!(status, Some(400) | Some(401) | Some(403))
}

/// RFC 3986 query 组件编码（`URLSearchParams` 同款：仅保留非保留字符）。
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 授权 URL（dsh `antigravityFlow.buildAuthorizeUrl` :283-295 参数逐项对齐：
/// client_id / response_type=code / redirect_uri / scope / code_challenge /
/// code_challenge_method=S256 / state / access_type=offline / prompt=consent ——
/// Antigravity 不带 nonce）。
pub(crate) fn build_authorize_url(redirect_uri: &str, state: &str, challenge: &str) -> String {
    format!(
        "{AUTH_URL}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        urlencode(CLIENT_ID),
        urlencode(redirect_uri),
        urlencode(SCOPES),
        urlencode(challenge),
        urlencode(state),
    )
}

// ---- token 端点 ----

/// token 端点错误：带 HTTP 状态与 OAuth `error` 码（如 `invalid_grant`）。
struct TokenError {
    message: String,
    status: Option<u16>,
    oauth_code: Option<String>,
}

/// Google 错误体 `{error, error_description}` → (展示消息, error 码)。
fn google_error_of(v: &Value) -> Option<(String, Option<String>)> {
    let code = v.get("error").and_then(Value::as_str).map(|s| s.to_string());
    let desc = v
        .get("error_description")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    match (code, desc) {
        (Some(c), Some(d)) => Some((d, Some(c))),
        (Some(c), None) => Some((c.clone(), Some(c))),
        (None, Some(d)) => Some((d, None)),
        (None, None) => None,
    }
}

/// POST `oauth2.googleapis.com/token`（urlencoded）。
fn post_token(form: &[(&str, &str)]) -> Result<Value, TokenError> {
    let encoded = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let agent = crate::providers::make_agent();
    let result = agent
        .post(TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&encoded);
    let (status, text) = match result {
        Ok(resp) => (200u16, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Transport(t)) => {
            return Err(TokenError {
                message: format!("token 端点请求失败：{t}"),
                status: None,
                oauth_code: None,
            });
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return Err(TokenError {
                message: format!("token 端点返回无法解析（HTTP {status}）"),
                status: Some(status),
                oauth_code: None,
            });
        }
    };
    if let Some((message, code)) = google_error_of(&value) {
        return Err(TokenError {
            message,
            status: Some(status),
            oauth_code: code,
        });
    }
    if status != 200 {
        return Err(TokenError {
            message: format!("token 端点 HTTP {status}"),
            status: Some(status),
            oauth_code: None,
        });
    }
    Ok(value)
}

/// 读 access_token / refresh_token / expires_in（缺 token 即错误）。
fn tokens_from(v: &Value) -> Result<(String, Option<String>, u64), String> {
    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Google 返回的 token 不完整（缺 access_token），请重试".to_string())?;
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let expires_in = v
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    Ok((access.to_string(), refresh, expires_in))
}

// ---- 会话 ----

/// 未登录提示（含重新登录指引 —— provider 直接展示该文案）。
pub(crate) fn not_logged_in_message() -> String {
    "Google Antigravity 未登录，请执行 quota-bar login antigravity 登录".into()
}

/// 登录失效提示（invalid_grant 删会话后返回）。
fn stale_login_message() -> String {
    "Antigravity 登录已失效，请执行 quota-bar login antigravity 重新登录".into()
}

/// 条目损坏提示（session 缺关键字段，仍返回该账号让条上能看到是谁坏了）。
fn corrupt_session_message() -> String {
    "Antigravity 会话数据损坏，请执行 quota-bar login antigravity 重新登录".into()
}

/// 单条会话的 token 解析决策（**纯函数**：读哪条、怎么判 —— 离线可测；
/// 网络部分（刷新）由调用方按 `Refresh` 携带的 refresh_token 执行）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TokenDecision {
    /// 未过期 → 直接用该 access token。
    Valid(String),
    /// 过期且有 refresh_token → 用它刷新。
    Refresh(String),
    /// 过期且无 refresh_token → 登录失效。
    Stale,
    /// access_token 为空（条目级损坏）→ 会话数据损坏。
    Corrupt,
}

/// 判定一条 session 的 token 状态（时间参数注入，便于测试边界）。
pub(crate) fn decide_token(session: &GoogleSession, now_ms_val: u64) -> TokenDecision {
    if session.access_token.is_empty() {
        return TokenDecision::Corrupt;
    }
    if !token_expired(session.expires_at_ms, now_ms_val) {
        return TokenDecision::Valid(session.access_token.clone());
    }
    match session
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(rt) => TokenDecision::Refresh(rt.to_string()),
        None => TokenDecision::Stale,
    }
}

/// 刷新结果：拿到新 session / 登录已死（调用方移除该条目）/ 可重试失败。
enum RefreshResult {
    Fresh(Box<GoogleSession>),
    Permanent(String),
    Failed(String),
}

/// 返回有效 access token（多账号语义）：
/// - 活动账号未过期 → 直接返回；
/// - 过期且有 refresh_token → 刷新，**只回写该 entry.session**
///   （active / 顺序 / 其它条目全不动）并保存；
/// - `invalid_grant` 或 400/401/403 → 移除该失效条目（剩余自动切换 /
///   空则清存储）并提示重新登录；其它失败 → 保留会话、带上游信息返回。
pub fn access_token() -> Result<String, String> {
    let mut file = super::load_file(ProviderId::Antigravity);
    let now = now_ms();
    let session = file
        .active()
        .map(|e| session_of(&e.session))
        .ok_or_else(not_logged_in_message)?;
    match decide_token(&session, now) {
        TokenDecision::Valid(token) => Ok(token),
        TokenDecision::Corrupt => Err(corrupt_session_message()),
        TokenDecision::Stale => Err(stale_login_message()),
        TokenDecision::Refresh(refresh_token) => {
            match refresh_session(&refresh_token, &session, now) {
                RefreshResult::Fresh(updated) => {
                    let updated = *updated;
                    let access = updated.access_token.clone();
                    let value = serde_json::to_value(&updated).unwrap_or(Value::Null);
                    if !file.update_active_session(value) {
                        // 单线程下刷新前刚取到活动账号，正常到不了这里；防御性报错。
                        return Err("Antigravity 会话状态异常（活动账号丢失），请重新登录".into());
                    }
                    super::save_file(ProviderId::Antigravity, &file)?;
                    Ok(access)
                }
                RefreshResult::Permanent(message) => {
                    // 登录已死：移除该活动条目（避免每轮轮询都白打一次刷新）。
                    file.remove_active();
                    if file.accounts.is_empty() {
                        let _ = super::clear_session(ProviderId::Antigravity);
                    } else {
                        let _ = super::save_file(ProviderId::Antigravity, &file);
                    }
                    Err(message)
                }
                RefreshResult::Failed(message) => Err(message),
            }
        }
    }
}

/// 全部账号按【存储顺序】逐个解析有效 token（`account_tokens` 的实现）：
/// - 未过期 → `Ok(access)`；过期有 refresh → 刷新，成功**只写回该条目**
///   （`update_session_by_id`，active/顺序/其它不动）；
/// - 永久失败 → 移除【该条目】（是 active → `remove_active` 收尾规则；
///   否则只摘该条）——当次仍返回其 Err 行，便于条上定位是哪个号失效；
/// - 条目损坏 → `Err("会话数据损坏…")` 且**保留条目**；
/// - 本轮有写（刷新回写/移除）才落盘一次；移除到空 → 清存储。
pub fn account_tokens() -> Vec<super::AccountToken> {
    let mut file = super::load_file(ProviderId::Antigravity);
    let now = now_ms();
    // 先按存储顺序快照条目：中途的条目级移除不会打乱遍历（回写按 id 寻址）。
    let snapshot: Vec<(String, GoogleSession)> = file
        .accounts
        .iter()
        .map(|e| (e.id.clone(), session_of(&e.session)))
        .collect();
    let mut out = Vec::with_capacity(snapshot.len());
    let mut dirty = false;
    for (account_id, session) in snapshot {
        let email = session.email.clone();
        let project_id = session.project_id.clone();
        let token = match decide_token(&session, now) {
            TokenDecision::Valid(token) => Ok(token),
            TokenDecision::Corrupt => Err(corrupt_session_message()),
            TokenDecision::Stale => Err(stale_login_message()),
            TokenDecision::Refresh(refresh_token) => {
                match refresh_session(&refresh_token, &session, now) {
                    RefreshResult::Fresh(updated) => {
                        let access = updated.access_token.clone();
                        let value = serde_json::to_value(&*updated).unwrap_or(Value::Null);
                        if file.update_session_by_id(&account_id, value) {
                            dirty = true;
                        }
                        Ok(access)
                    }
                    RefreshResult::Permanent(message) => {
                        if file.remove_by_id(&account_id).is_some() {
                            dirty = true;
                        }
                        Err(message)
                    }
                    RefreshResult::Failed(message) => Err(message),
                }
            }
        };
        out.push(super::AccountToken {
            account_id,
            email,
            project_id,
            token,
        });
    }
    if dirty {
        if file.accounts.is_empty() {
            let _ = super::clear_session(ProviderId::Antigravity);
        } else {
            let _ = super::save_file(ProviderId::Antigravity, &file);
        }
    }
    out
}

/// 执行 refresh_token grant，返回刷新后的新 session（**不写存储** ——
/// 回写范围由调用方限定为活动账号条目）。
fn refresh_session(
    refresh_token: &str,
    session: &GoogleSession,
    now: u64,
) -> RefreshResult {
    let form = [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    let value = match post_token(&form) {
        Ok(v) => v,
        Err(e) => {
            if is_permanent_refresh_error(e.status, e.oauth_code.as_deref()) {
                return RefreshResult::Permanent(stale_login_message());
            }
            return RefreshResult::Failed(format!("Antigravity 刷新 token 失败：{}", e.message));
        }
    };
    let (access, new_refresh, expires_in) = match tokens_from(&value) {
        Ok(t) => t,
        Err(msg) => return RefreshResult::Failed(format!("Antigravity 刷新 token 失败：{msg}")),
    };
    let mut updated = session.clone();
    updated.access_token = access;
    // 响应可能不带 refresh_token → 保留原值。
    updated.refresh_token = match new_refresh {
        Some(t) => Some(t),
        None => session.refresh_token.clone(),
    };
    updated.expires_at_ms = compute_expiry(now, expires_in);
    RefreshResult::Fresh(Box::new(updated))
}

// ---- 登录期的辅助探测（失败静默，dsh 同）----

/// Google userinfo 邮箱（dsh `getUserEmail` :309-320）。
fn fetch_email(token: &str) -> Option<String> {
    let agent = crate::providers::make_agent();
    let resp = agent
        .get(USERINFO_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?;
    let v: Value = serde_json::from_str(&resp.into_string().ok()?).ok()?;
    v.get("email")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// dsh `extractProjectId`（:338-364）：多键直取 + 嵌套 id + 项目数组递归。
fn extract_project_id(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    for key in [
        "antigravityProjectId",
        "projectId",
        "backendProjectId",
        "userDefinedCloudaicompanionProject",
        "cloudaicompanionProject",
        "project",
    ] {
        if let Some(direct) = obj.get(key).filter(|x| !x.is_null()) {
            if let Some(s) = direct.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            if let Some(nested) = direct.as_object().and_then(|d| d.get("id")).and_then(Value::as_str)
            {
                if !nested.is_empty() {
                    return Some(nested.to_string());
                }
            }
        }
    }
    for key in ["projects", "projectIds", "cloudaicompanionProjects"] {
        if let Some(Value::Array(items)) = obj.get(key) {
            for item in items {
                if let Some(found) = extract_project_id(item) {
                    return Some(found);
                }
                if let Some(s) = item.as_str() {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

/// dsh `discoverProjectId`（:367-404）：两 host ×（loadCodeAssist →
/// listCloudAICompanionProjects）；全失败 → None（fetch 侧用默认项目）。
fn discover_project(token: &str) -> Option<String> {
    for host in [HOST_PRIMARY, HOST_FALLBACK] {
        let probes = [
            (
                "/v1internal:loadCodeAssist",
                "{\"metadata\":{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"PLATFORM_UNSPECIFIED\",\"pluginType\":\"GEMINI\"}}",
            ),
            ("/v1internal:listCloudAICompanionProjects", "{}"),
        ];
        for (path, body) in probes {
            if let Some(found) = probe_project(host, path, body, token) {
                return Some(found);
            }
        }
    }
    None
}

fn probe_project(host: &str, path: &str, body: &str, token: &str) -> Option<String> {
    let agent = crate::providers::make_agent();
    let url = format!("{host}{path}");
    let req = crate::providers::antigravity::with_headers(agent.post(&url), token);
    let resp = req.send_string(body).ok()?;
    let v: Value = serde_json::from_str(&resp.into_string().ok()?).ok()?;
    extract_project_id(&v)
}

// ---- 交互式登录 ----

/// 用系统默认浏览器打开授权 URL（`ShellExecuteW`，windows crate Win32_UI_Shell）。
/// Codex 登录（`oauth::codex`）复用同一实现。
pub(crate) fn open_browser(url: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let instance = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 约定：返回值 > 32 才是成功。
    let code = instance.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!(
            "无法打开默认浏览器（ShellExecuteW 返回 {code}）；请手动打开上面的授权链接"
        ))
    }
}

/// 授权码 → 会话（dsh `exchangeAntigravityCode` :407-455）：
/// token 交换 + userinfo 邮箱 + 项目发现；`expires_at_ms` 含 5 分钟预刷新。
fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<GoogleSession, String> {
    let form = [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    let value = post_token(&form)
        .map_err(|e| format!("授权码换取 token 失败：{}", e.message))?;
    let (access, refresh, expires_in) = tokens_from(&value)?;
    let refresh = refresh.ok_or_else(|| {
        "Google 返回的 token 不完整（缺 refresh_token），无法持久登录，请重试".to_string()
    })?;
    let now = now_ms();
    let email = fetch_email(&access);
    // dsh 用 stableProjectId(sha1) 兜底；Rust 侧无 sha1 依赖 —— 取不到就留空，
    // 抓取时按 DEFAULT_PROJECT（aicode-consumers）发请求，400/403 再退空 body。
    let project_id = discover_project(&access).unwrap_or_default();
    Ok(GoogleSession {
        access_token: access,
        refresh_token: Some(refresh),
        expires_at_ms: compute_expiry(now, expires_in),
        project_id,
        email,
    })
}

/// 交互式登录（阻塞）：打印授权 URL → 打开浏览器 → 等回环回调或粘贴兜底 →
/// 换 token → `save_session`。
pub fn login_blocking() -> Result<(), String> {
    let pkce = pkce::create_pkce();
    let state = pkce::random_token(16);
    let redirect_uri = format!(
        "http://{}:{}{}",
        loopback::CALLBACK_HOST,
        loopback::CALLBACK_PORT,
        loopback::CALLBACK_PATH
    );
    let authorize_url = build_authorize_url(&redirect_uri, &state, &pkce.challenge);

    // 先绑回环（被占用 → 仍继续，粘贴兜底接管）。
    let listener = std::net::TcpListener::bind((
        loopback::CALLBACK_HOST,
        loopback::CALLBACK_PORT,
    ));

    println!("请在浏览器完成 Google 登录（已尝试自动打开）：\n\n  {authorize_url}\n");
    println!(
        "提示：若浏览器打不开本地回环（端口 {} 被占用），可在 {} 秒内直接粘贴浏览器回调地址或授权码后回车。",
        loopback::CALLBACK_PORT,
        loopback::PASTE_WINDOW_MS / 1000
    );

    if let Err(e) = open_browser(&authorize_url) {
        eprintln!("{e}");
    }

    // 等 code：回环 180s / 粘贴 30s（内部含 state 校验与中文错误）。
    let code = loopback::wait_for_code(
        listener,
        loopback::CALLBACK_PATH,
        &state,
        "Google",
        "quota-bar login antigravity",
    )?;

    // 换 token → 【添加/更新该账号并切换为活动】（login_blocking 的多账号语义）。
    let new_session = exchange_code(&code, &pkce.verifier, &redirect_uri)?;
    let mut file = super::load_file(ProviderId::Antigravity);
    let value = serde_json::to_value(&new_session).unwrap_or(Value::Null);
    let account_id = file.upsert(value, &GoogleKind);
    file.set_active(&account_id);
    super::save_file(ProviderId::Antigravity, &file)?;

    let label = account_label(ProviderId::Antigravity).unwrap_or_else(|| "（未取到邮箱）".into());
    let project = new_session.project_id.clone();
    // 项目为空是合法状态：抓取侧只发 `{}` body（不注入默认项目）。
    println!(
        "已登录：{label}（账号 {account_id}，项目 {}）",
        if project.is_empty() { "（未发现，按空 body 请求）" } else { &project }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_computes_with_five_minute_preempt() {
        let now = 1_790_000_000_000u64;
        // 3600s 的 token：写入时刻已减 5 分钟
        assert_eq!(
            compute_expiry(now, 3600),
            now + 3_600_000 - 300_000
        );
        // expires_in 缺失时按 3600（tokens_from 默认）；这里直接测 0 → 立即过期
        assert_eq!(compute_expiry(now, 0), now.saturating_sub(300_000));
        // 下溢钳制不 panic
        assert_eq!(compute_expiry(100, 0), 0);
    }

    #[test]
    fn preloaded_expiry_boundary_is_exactly_at_deadline() {
        let now = 1_790_000_000_000u64;
        let expires = compute_expiry(now, 3600);
        // 有效期内（now < expires）
        assert!(!token_expired(expires, now));
        assert!(!token_expired(expires, expires - 1));
        // 恰好到点 → 过期（预刷新语义：expires_at_ms 本身已是提前 5 分钟的时刻）
        assert!(token_expired(expires, expires));
        assert!(token_expired(expires, expires + 1));
    }

    #[test]
    fn decide_token_routes_by_expiry_refresh_and_corruption() {
        let now = 1_790_000_000_000u64;
        let mut session = GoogleSession {
            access_token: "at-valid".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms: now + 1_000,
            project_id: "proj".into(),
            email: Some("a@b.test".into()),
        };
        // 未过期 → 直接用
        assert_eq!(
            decide_token(&session, now),
            TokenDecision::Valid("at-valid".into())
        );
        // 恰好到点（预刷新边界）→ 过期但有 refresh → 走刷新
        session.expires_at_ms = now;
        assert_eq!(decide_token(&session, now), TokenDecision::Refresh("rt-1".into()));
        assert_eq!(decide_token(&session, now - 1), TokenDecision::Valid("at-valid".into()));
        // 过期且无 refresh → 失效
        session.refresh_token = None;
        assert_eq!(decide_token(&session, now), TokenDecision::Stale);
        // 空 refresh 串同"没有"
        session.refresh_token = Some(String::new());
        assert_eq!(decide_token(&session, now), TokenDecision::Stale);
        // access_token 为空（条目损坏）→ Corrupt，即使未过期也优先判
        session.access_token = String::new();
        session.refresh_token = Some("rt-1".into());
        session.expires_at_ms = now + 9_999;
        assert_eq!(decide_token(&session, now), TokenDecision::Corrupt);
        // 三种错误文案都带重新登录指引
        assert!(corrupt_session_message().contains("quota-bar login antigravity"));
        assert!(stale_login_message().contains("quota-bar login antigravity"));
        assert!(not_logged_in_message().contains("quota-bar login antigravity"));
    }

    #[test]
    fn permanent_refresh_error_classification() {
        // dsh：invalid_grant 或 400/401/403 → 登录已死
        assert!(is_permanent_refresh_error(Some(400), Some("invalid_grant")));
        assert!(is_permanent_refresh_error(Some(400), None));
        assert!(is_permanent_refresh_error(Some(401), None));
        assert!(is_permanent_refresh_error(Some(403), None));
        assert!(is_permanent_refresh_error(None, Some("invalid_grant")));
        // 上游 5xx / 网络错误不是登出理由
        assert!(!is_permanent_refresh_error(Some(503), None));
        assert!(!is_permanent_refresh_error(None, None));
    }

    #[test]
    fn authorize_url_carries_exact_dsh_params() {
        let url = build_authorize_url(
            "http://127.0.0.1:51121/oauth-callback",
            "st-xyz",
            "challenge-abc",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"), "{url}");
        assert!(url.contains(&format!("client_id={}", urlencode(CLIENT_ID))));
        assert!(url.contains("response_type=code"));
        assert!(url
            .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A51121%2Foauth-callback"));
        assert!(url.contains("code_challenge=challenge-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st-xyz"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        // scope 六个、空格分隔后整体编码（%20）
        let scope_encoded = urlencode(SCOPES);
        assert!(url.contains(&format!("scope={scope_encoded}")));
        assert_eq!(SCOPES.split(' ').count(), 6);
        // Antigravity 不带 nonce（dsh buildAuthorizeUrl 无此参数）
        assert!(!url.contains("nonce="));
        // client_secret 是解码后的 Google 形态（GOCSPX- 前缀）
        // 凭据由构建期环境变量注入；未注入时跳过凭据相关断言。
        if !CLIENT_SECRET.is_empty() {
            assert!(CLIENT_SECRET.starts_with("GOCSPX-"));
        }
    }

    #[test]
    fn not_logged_in_message_names_the_login_command() {
        let msg = not_logged_in_message();
        assert!(msg.contains("quota-bar login antigravity"), "{msg}");
        assert!(stale_login_message().contains("quota-bar login antigravity"));
    }

    #[test]
    fn extract_project_id_covers_wire_variants() {
        // 直接键
        assert_eq!(
            extract_project_id(&serde_json::json!({"projectId": "p1"})).as_deref(),
            Some("p1")
        );
        assert_eq!(
            extract_project_id(&serde_json::json!({"project": "p2"})).as_deref(),
            Some("p2")
        );
        // 嵌套 {id}
        assert_eq!(
            extract_project_id(&serde_json::json!({"project": {"id": "p3"}})).as_deref(),
            Some("p3")
        );
        // 项目数组递归
        assert_eq!(
            extract_project_id(
                &serde_json::json!({"projects": [{"name": "x"}, {"projectId": "p4"}]})
            )
            .as_deref(),
            Some("p4")
        );
        assert_eq!(
            extract_project_id(&serde_json::json!({"projects": ["p5"]})).as_deref(),
            Some("p5")
        );
        // 空对象 → None
        assert_eq!(extract_project_id(&serde_json::json!({})), None);
    }

    #[test]
    fn urlencode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(urlencode("abc-123_~."), "abc-123_~.");
        assert_eq!(urlencode("a b/c?d=e"), "a%20b%2Fc%3Fd%3De");
        assert_eq!(urlencode("中"), "%E4%B8%AD");
    }
}
