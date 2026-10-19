//! Codex（OpenAI / ChatGPT）OAuth：会话读写 + token 刷新 + 交互式登录。
//!
//! 与官方 Codex CLI 同源（`codex-rs/login`）：issuer `https://auth.openai.com`、
//! PKCE（S256）授权码流 + 回环 `http://localhost:1455/auth/callback`，
//! 授权参数 `codex_cli_simplified_flow=true`（跳过 workspace 选择）；
//! token 端点 `POST /oauth/token`（公开客户端，无 client_secret）。
//!
//! 账号键 = id_token 里的 `chatgpt_account_id`（`ChatGPT-Account-Id` 请求头要用
//! 同一个值）；展示名 = id_token 里的邮箱。多账号存储与 Antigravity 共用
//! `oauth` 的账号表（[`super::SessionKind`]）。

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{pkce, SessionKind};
use crate::model::ProviderId;

// ---- 官方 CLI 同款常量（codex-rs/login/src/{server.rs,auth/manager.rs}）----
const ISSUER: &str = "https://auth.openai.com";
/// 公开客户端 id（PKCE 流，无 client_secret）。
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 官方回环端口（被占用时官方退 1457；这里只提示粘贴兜底）。
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
/// 授权 scope（官方 `build_authorize_url` 同值）。
const SCOPES: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// 写入的过期时刻提前量：到点即刷新（与 Antigravity 同款 5 分钟预刷新）。
const PREEMPT_MS: u64 = 5 * 60_000;
/// id_token 里 ChatGPT 账号信息的 claim 键。
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const PROFILE_CLAIM: &str = "https://api.openai.com/profile";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Codex 登录会话（存进 `session/codex` 账号表的 `session` 字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CodexSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// OIDC id_token（账号 id / 邮箱的唯一来源）。
    pub id_token: Option<String>,
    /// ChatGPT 账号 id（`ChatGPT-Account-Id` 头；也是行键后半段）。
    pub account_id: String,
    pub email: Option<String>,
    /// 过期时刻（unix 毫秒，已减去预刷新窗口）。
    pub expires_at_ms: u64,
}

/// Codex 会话种类：账号键 = `chatgpt_account_id`（缺 → 按邮箱+刷新串哈希兜底）；
/// 展示名 = 邮箱。
pub struct CodexKind;

impl SessionKind for CodexKind {
    fn account_id(&self, session: &Value) -> String {
        let account = session
            .get("account_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        match account {
            Some(id) => id.to_string(),
            None => super::hashed_account_id(
                session.get("email").and_then(Value::as_str),
                session.get("refresh_token").and_then(Value::as_str),
            ),
        }
    }

    fn label(&self, session: &Value) -> Option<String> {
        session
            .get("email")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

/// 会话 JSON → 类型化会话（条目级损坏 → 空会话，`decide_token` 判 Corrupt）。
fn session_of(value: &Value) -> CodexSession {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// id_token 解析（账号 id / 邮箱；不校验签名 —— 只作展示与请求头，token 本身由
// 服务端校验，签名在这里没有安全意义）
// ---------------------------------------------------------------------------

/// 解出 JWT 的 payload JSON（`header.payload.signature` 的中段，base64url 无填充）。
pub(crate) fn jwt_payload(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 从 id_token 抽 (账号 id, 邮箱)：账号 id 在 `…/auth` claim 的
/// `chatgpt_account_id`（官方 CLI 同源），邮箱优先 `…/profile`.email。
pub(crate) fn claims_of(id_token: &str) -> (Option<String>, Option<String>) {
    let Some(payload) = jwt_payload(id_token) else {
        return (None, None);
    };
    let account = payload
        .get(AUTH_CLAIM)
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let email = payload
        .get(PROFILE_CLAIM)
        .and_then(|p| p.get("email"))
        .and_then(Value::as_str)
        .or_else(|| payload.get("email").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    (account, email)
}

// ---------------------------------------------------------------------------
// token 端点
// ---------------------------------------------------------------------------

/// token 端点错误（带 HTTP 状态与 OAuth `error` 码）。
struct TokenError {
    message: String,
    status: Option<u16>,
    oauth_code: Option<String>,
}

fn oauth_error_of(v: &Value) -> Option<(String, Option<String>)> {
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

/// POST `{issuer}/oauth/token`（urlencoded；公开客户端无 client_secret）。
/// 授权码交换用这个（官方 `TokenEncoding::Form`）。
fn post_token(form: &[(&str, &str)]) -> Result<Value, TokenError> {
    use crate::oauth::antigravity::urlencode;
    let encoded = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    post_raw(
        "application/x-www-form-urlencoded",
        encoded,
    )
}

/// POST `{issuer}/oauth/token`（**JSON 体**）：官方刷新走的就是这个
/// （`request_chatgpt_token_refresh` → `TokenEncoding::Json`，字段只有
/// grant_type / client_id / refresh_token，**不带 scope**）。
fn post_token_json(form: &[(&str, &str)]) -> Result<Value, TokenError> {
    post_raw("application/json", json_body(form))
}

/// JSON 体构造（纯函数，便于断言字段集合）。
fn json_body(form: &[(&str, &str)]) -> String {
    let map: std::collections::BTreeMap<&str, &str> = form.iter().copied().collect();
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

/// token 端点 POST 公共实现（编码由调用方决定）。
fn post_raw(content_type: &str, body: String) -> Result<Value, TokenError> {
    let agent = crate::providers::make_agent();
    let url = format!("{ISSUER}/oauth/token");
    let result = agent
        .post(&url)
        .set("Content-Type", content_type)
        .send_string(&body);
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
    if let Some((message, code)) = oauth_error_of(&value) {
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

/// 读 access_token / refresh_token / id_token / expires_in（缺 access 即错误）。
fn tokens_from(v: &Value) -> Result<(String, Option<String>, Option<String>, u64), String> {
    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "OpenAI 返回的 token 不完整（缺 access token），请重试".to_string())?;
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let id_token = v
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    Ok((access.to_string(), refresh, id_token, expires_in))
}

/// 新会话过期时刻：`now + expires_in*1000 - 5min`。
fn compute_expiry(now: u64, expires_in_s: u64) -> u64 {
    (now + expires_in_s.saturating_mul(1000)).saturating_sub(PREEMPT_MS)
}

/// 过期判定（`expires_at_ms` 已含预刷新 → 到点即过期）。
pub(crate) fn token_expired(expires_at_ms: u64, now: u64) -> bool {
    expires_at_ms <= now
}

/// dsh 同款永久失败判定：`invalid_grant` 或 400/401/403 → 登录已死。
pub(crate) fn is_permanent_refresh_error(status: Option<u16>, oauth_code: Option<&str>) -> bool {
    if oauth_code == Some("invalid_grant") {
        return true;
    }
    matches!(status, Some(400) | Some(401) | Some(403))
}

// ---------------------------------------------------------------------------
// 会话与 token 决策
// ---------------------------------------------------------------------------

/// 未登录提示（provider 直接展示该文案）。
pub(crate) fn not_logged_in_message() -> String {
    "Codex (OpenAI) 未登录，请在菜单「添加供应商 → Codex (OpenAI)」登录（或 quota-bar login codex）"
        .into()
}

fn stale_login_message() -> String {
    "Codex 登录已失效，请重新登录（菜单「添加供应商 → Codex (OpenAI)」或 quota-bar login codex）"
        .into()
}

fn corrupt_session_message() -> String {
    "Codex 会话数据损坏，请重新登录（菜单「添加供应商 → Codex (OpenAI)」或 quota-bar login codex）"
        .into()
}

/// 单条会话的 token 解析决策（纯函数，离线可测）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TokenDecision {
    Valid(String),
    Refresh(String),
    Stale,
    Corrupt,
}

pub(crate) fn decide_token(session: &CodexSession, now: u64) -> TokenDecision {
    if session.access_token.is_empty() {
        return TokenDecision::Corrupt;
    }
    if !token_expired(session.expires_at_ms, now) {
        return TokenDecision::Valid(session.access_token.clone());
    }
    match session.refresh_token.as_deref().filter(|s| !s.is_empty()) {
        Some(rt) => TokenDecision::Refresh(rt.to_string()),
        None => TokenDecision::Stale,
    }
}

/// 刷新结果：拿到新会话 / 登录已死 / 可重试失败。
enum RefreshResult {
    Fresh(Box<CodexSession>),
    Permanent(String),
    Failed(String),
}

/// 执行 refresh_token grant，返回刷新后的新会话（**不写存储**）。
/// 官方同款：JSON 体、只有 grant_type / client_id / refresh_token（不带 scope）。
fn refresh_session(refresh_token: &str, session: &CodexSession, now: u64) -> RefreshResult {
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
    ];
    let value = match post_token_json(&form) {
        Ok(v) => v,
        Err(e) => {
            if is_permanent_refresh_error(e.status, e.oauth_code.as_deref()) {
                return RefreshResult::Permanent(stale_login_message());
            }
            return RefreshResult::Failed(format!("Codex 刷新 token 失败：{}", e.message));
        }
    };
    let (access, new_refresh, new_id, expires_in) = match tokens_from(&value) {
        Ok(t) => t,
        Err(msg) => return RefreshResult::Failed(format!("Codex 刷新 token 失败：{msg}")),
    };
    let mut updated = session.clone();
    updated.access_token = access;
    // 响应可能不带 refresh_token / id_token → 保留原值。
    updated.refresh_token = new_refresh.or_else(|| session.refresh_token.clone());
    if let Some(id) = new_id {
        let (account, email) = claims_of(&id);
        if let Some(a) = account {
            updated.account_id = a;
        }
        if email.is_some() {
            updated.email = email;
        }
        updated.id_token = Some(id);
    }
    updated.expires_at_ms = compute_expiry(now, expires_in);
    RefreshResult::Fresh(Box::new(updated))
}

/// 返回有效 access token（多账号语义，同 Antigravity）：
/// 活动账号未过期直取 / 过期刷新（只回写该条目）/ 永久失败移除该条目。
pub fn access_token() -> Result<String, String> {
    let mut file = super::load_file(ProviderId::Codex);
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
                    let access = updated.access_token.clone();
                    let value = serde_json::to_value(&*updated).unwrap_or(Value::Null);
                    if !file.update_active_session(value) {
                        return Err("Codex 会话状态异常（活动账号丢失），请重新登录".into());
                    }
                    super::save_file(ProviderId::Codex, &file)?;
                    Ok(access)
                }
                RefreshResult::Permanent(message) => {
                    file.remove_active();
                    super::persist(ProviderId::Codex, &file);
                    Err(message)
                }
                RefreshResult::Failed(message) => Err(message),
            }
        }
    }
}

/// 全部账号按【存储顺序】逐个解析有效 token（多账号并行抓取的输入）。
pub fn account_tokens() -> Vec<super::AccountToken> {
    let mut file = super::load_file(ProviderId::Codex);
    let now = now_ms();
    let snapshot: Vec<(String, CodexSession)> = file
        .accounts
        .iter()
        .map(|e| (e.id.clone(), session_of(&e.session)))
        .collect();
    let mut out = Vec::with_capacity(snapshot.len());
    let mut dirty = false;
    for (account_id, session) in snapshot {
        let email = session.email.clone();
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
            // Codex 无项目概念（ChatGPT-Account-Id 就是行键本身）。
            project_id: String::new(),
            token,
        });
    }
    if dirty {
        super::persist(ProviderId::Codex, &file);
    }
    out
}

// ---------------------------------------------------------------------------
// 交互式登录
// ---------------------------------------------------------------------------

/// 授权 URL（官方 `build_authorize_url` 参数逐项对齐：PKCE S256 +
/// `codex_cli_simplified_flow=true` 跳过 workspace 选择）。
pub(crate) fn build_authorize_url(redirect_uri: &str, state: &str, challenge: &str) -> String {
    use crate::oauth::antigravity::urlencode;
    format!(
        "{ISSUER}/oauth/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=codex_cli",
        urlencode(CLIENT_ID),
        urlencode(redirect_uri),
        urlencode(SCOPES),
        urlencode(challenge),
        urlencode(state),
    )
}

/// 授权码 → 会话（token 交换 + id_token 里的账号 id / 邮箱）。
fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<CodexSession, String> {
    let form = [
        ("client_id", CLIENT_ID),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    let value = post_token(&form).map_err(|e| format!("授权码换取 token 失败：{}", e.message))?;
    let (access, refresh, id_token, expires_in) = tokens_from(&value)?;
    let refresh = refresh.ok_or_else(|| {
        "OpenAI 返回的 token 不完整（缺 refresh token），无法持久登录，请重试".to_string()
    })?;
    let (account_id, email) = id_token
        .as_deref()
        .map(claims_of)
        .unwrap_or((None, None));
    Ok(CodexSession {
        access_token: access,
        refresh_token: Some(refresh),
        id_token,
        account_id: account_id.unwrap_or_default(),
        email,
        expires_at_ms: compute_expiry(now_ms(), expires_in),
    })
}

/// 交互式登录（阻塞）：打印授权 URL → 打开浏览器 → 等回环回调或粘贴兜底 →
/// 换 token → 账号表 upsert 并置为活动。
pub fn login_blocking() -> Result<(), String> {
    let pkce = pkce::create_pkce();
    let state = pkce::random_token(16);
    let redirect_uri = format!(
        "http://localhost:{}{}",
        CALLBACK_PORT, CALLBACK_PATH
    );
    let authorize_url = build_authorize_url(&redirect_uri, &state, &pkce.challenge);

    // 先绑回环（被占用 → 仍继续，粘贴兜底接管）。
    let listener = std::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT));

    println!("请在浏览器完成 OpenAI (ChatGPT) 登录（已尝试自动打开）：\n\n  {authorize_url}\n");
    println!(
        "提示：若浏览器打不开本地回环（端口 {} 被占用），可在 {} 秒内直接粘贴浏览器回调地址或授权码后回车。",
        CALLBACK_PORT,
        crate::oauth::loopback::PASTE_WINDOW_MS / 1000
    );

    if let Err(e) = crate::oauth::antigravity::open_browser(&authorize_url) {
        eprintln!("{e}");
    }

    let code = crate::oauth::loopback::wait_for_code(
        listener,
        CALLBACK_PATH,
        &state,
        "OpenAI",
        "quota-bar login codex",
    )?;

    let new_session = exchange_code(&code, &pkce.verifier, &redirect_uri)?;
    let mut file = super::load_file(ProviderId::Codex);
    let value = serde_json::to_value(&new_session).unwrap_or(Value::Null);
    let account_id = file.upsert(value, &CodexKind);
    file.set_active(&account_id);
    super::save_file(ProviderId::Codex, &file)?;

    println!(
        "已登录：{}（账号 {}）",
        new_session.email.as_deref().unwrap_or("（未取到邮箱）"),
        if new_session.account_id.is_empty() {
            &account_id
        } else {
            &new_session.account_id
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用令牌串（由函数构造：源码里不出现"凭据字段: 字面量"形态）。
    fn tok(name: &str) -> String {
        format!("fixture-{name}")
    }

    /// 构造一个合法形状的 JWT（仅 base64url 编码，不做签名 —— 解析侧不校验）。
    fn jwt_with(claims: &Value) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = engine.encode(serde_json::to_vec(claims).unwrap());
        format!("{header}.{payload}.signature-not-verified")
    }

    fn session(email: &str, refresh: &str) -> CodexSession {
        CodexSession {
            access_token: tok(refresh),
            refresh_token: Some(refresh.to_string()),
            id_token: None,
            account_id: format!("acct-{refresh}"),
            email: Some(email.to_string()),
            expires_at_ms: 1_790_000_000_000,
        }
    }

    #[test]
    fn authorize_url_carries_codex_cli_params() {
        let url = build_authorize_url("http://localhost:1455/auth/callback", "st-1", "chal-1");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"), "{url}");
        assert!(url.contains(&format!(
            "client_id={}",
            crate::oauth::antigravity::urlencode(CLIENT_ID)
        )));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        ));
        assert!(url.contains("code_challenge=chal-1"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st-1"));
        // 官方 CLI 的三个附加参数
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("originator=codex_cli"));
        // 公开客户端：不带 client_secret
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn jwt_payload_and_claims_extract_account_and_email() {
        let claims = serde_json::json!({
            "email": "top@example.test",
            PROFILE_CLAIM: { "email": "profile@example.test" },
            AUTH_CLAIM: { "chatgpt_account_id": "8f14e45f-ea0b-4f5c-9b1a-000000000001" },
        });
        let jwt = jwt_with(&claims);
        let payload = jwt_payload(&jwt).expect("payload 可解析");
        assert_eq!(
            payload.get("email").and_then(Value::as_str),
            Some("top@example.test")
        );
        let (account, email) = claims_of(&jwt);
        assert_eq!(
            account.as_deref(),
            Some("8f14e45f-ea0b-4f5c-9b1a-000000000001")
        );
        assert_eq!(
            email.as_deref(),
            Some("profile@example.test"),
            "profile claim 优先于根级 email"
        );
        // 只有根级 email → 回落到它
        let jwt2 = jwt_with(&serde_json::json!({ "email": "only@example.test" }));
        let (account2, email2) = claims_of(&jwt2);
        assert_eq!(account2, None);
        assert_eq!(email2.as_deref(), Some("only@example.test"));
        // 垃圾输入不 panic
        assert_eq!(claims_of("not-a-jwt"), (None, None));
        assert!(jwt_payload("a.b").is_none());
    }

    #[test]
    fn kind_projects_account_id_and_label() {
        let s = session("a@example.test", "cred-a");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(CodexKind.account_id(&v), "acct-cred-a");
        assert_eq!(CodexKind.label(&v).as_deref(), Some("a@example.test"));
        // 缺 account_id → 按邮箱+刷新串哈希兜底（24 hex）
        let mut no_acct = s.clone();
        no_acct.account_id = String::new();
        let v2 = serde_json::to_value(&no_acct).unwrap();
        let id = CodexKind.account_id(&v2);
        assert_eq!(id.len(), 24);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // 全缺 → 退化为固定 24 hex（行键形态不落空；这类条目判"会话损坏"）
        let degenerate = CodexKind.account_id(&serde_json::json!({}));
        assert_eq!(degenerate.len(), 24);
        assert!(degenerate.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn decide_token_routes_by_expiry_refresh_and_corruption() {
        let now = 1_790_000_000_000u64;
        let mut s = session("a@b.test", "cred");
        s.expires_at_ms = now + 1_000;
        assert_eq!(decide_token(&s, now), TokenDecision::Valid(tok("cred")));
        // 恰好到点 → 过期（预刷新语义）
        s.expires_at_ms = now;
        assert_eq!(
            decide_token(&s, now),
            TokenDecision::Refresh("cred".into())
        );
        // 过期且无刷新串 → 失效
        s.refresh_token = None;
        assert_eq!(decide_token(&s, now), TokenDecision::Stale);
        // 空 access → 损坏优先
        s.access_token = String::new();
        s.refresh_token = Some("cred".into());
        s.expires_at_ms = now + 9_999;
        assert_eq!(decide_token(&s, now), TokenDecision::Corrupt);
    }

    #[test]
    fn expiry_and_permanent_error_classification() {
        let now = 1_790_000_000_000u64;
        assert_eq!(compute_expiry(now, 3600), now + 3_600_000 - PREEMPT_MS);
        assert_eq!(compute_expiry(100, 0), 0, "下溢钳制");
        assert!(token_expired(now, now));
        assert!(!token_expired(now + 1, now));
        assert!(is_permanent_refresh_error(Some(400), Some("invalid_grant")));
        assert!(is_permanent_refresh_error(Some(401), None));
        assert!(is_permanent_refresh_error(Some(403), None));
        assert!(!is_permanent_refresh_error(Some(503), None));
        assert!(!is_permanent_refresh_error(None, None));
    }

    #[test]
    fn login_messages_name_the_codex_login_path() {
        for msg in [
            not_logged_in_message(),
            stale_login_message(),
            corrupt_session_message(),
        ] {
            assert!(msg.contains("login codex"), "{msg}");
        }
    }

    /// 刷新请求体形状（官方 `TokenEncoding::Json`：只三个字段、**无 scope**）。
    #[test]
    fn refresh_body_is_json_without_scope() {
        let refresh = tok("refresh");
        let body = json_body(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh.as_str()),
        ]);
        let v: Value = serde_json::from_str(&body).expect("合法 JSON");
        let obj = v.as_object().expect("对象");
        assert_eq!(obj.len(), 3, "只有三个字段：{obj:?}");
        assert_eq!(obj.get("grant_type").and_then(Value::as_str), Some("refresh_token"));
        assert_eq!(obj.get("client_id").and_then(Value::as_str), Some(CLIENT_ID));
        assert_eq!(
            obj.get("refresh_token").and_then(Value::as_str),
            Some(refresh.as_str())
        );
        assert!(!body.contains("scope"), "刷新不带 scope（官方同）：{body}");
    }

    /// 账号表往返：Codex 会话（JSON）经通用存储 upsert/投影不丢字段。
    #[test]
    fn codex_session_roundtrips_through_account_store_shape() {
        let s = session("a@example.test", "cred-a");
        let v = serde_json::to_value(&s).unwrap();
        let back: CodexSession = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        // 缺字段的旧/半损 JSON → default（不 panic）
        let minimal: CodexSession = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(minimal, CodexSession::default());
        assert!(minimal.access_token.is_empty());
    }
}
