//! 阶跃星辰 StepFun 套餐额度：控制台 Dashboard RPC（Connect 风格，JSON body）。
//!
//! 三个方法（`POST https://platform.stepfun.com/api/step.openapi.devcenter.Dashboard/<Method>`）：
//! - `QueryStepPlanRateLimit` → 5 小时 / 每周剩余率 + 套餐 Credit 月池剩余率与重置时刻；
//! - `GetStepPlanStatus` → 档位名（`subscription.name`）与到期时刻；
//! - `QueryAccountBalance` → 账户余额（**单位「分」**，¥15.00 = `"1500"`）。
//!
//! 认证 = 浏览器会话 cookie **`Oasis-Token`**（会话段 + 设备段两段 JWT 拼接，共 8 个
//! dot 分段）；必需头 `oasis-appid: 10300` / `oasis-platform: web` /
//! `oasis-webid: <设备 id>`（设备 id 从第二段 JWT 的 `device_id` claim 里取，用户
//! 只需粘贴 cookie）。**API key 查不了额度**（服务端 403 `api key not permitted
//! for this method`），所以本 provider 是 Cookie 类。
//!
//! 协议要点（逆向记录，见 stepfun-cli `docs/protocol.md`）：
//! - cookie **只能含 `Oasis-Token` 一个字段**（多带任何字段服务端必 401）；
//! - 会话段 30 分钟、设备段 30 天，两段都得活着；会话段满 ~15 分钟后
//!   `PassportService/RefreshToken`（body `{}`、无需密码）才肯换发新会话段 ——
//!   本 provider 在临近过期与 401 时各续一次，并把新 cookie **写回凭据管理器**，
//!   于是只要程序每隔半小时内跑过一轮，会话就能一直续下去；
//! - 设备段超过 30 天后 `RefreshToken` 不报错、静默签发**匿名** token（`oasis_id`
//!   变化）→ 用 `oasis_id` 比对识别会话已死；
//! - proto3 JSON：int64 以字符串下发、double 以数字下发，字段名 camelCase 与
//!   snake_case 都要认；**滚动配额返回 `0` 表示"套餐不含该配额"**（不是"已用完"），
//!   这类整窗隐藏。

use serde_json::{Map, Value};

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow};
use crate::providers::mimo::{clamp_pct, js_round, now_ms, pick_num, secs_until, to_num};
use crate::providers::QuotaProvider;
use crate::secrets;

/// 业务接口前缀（Connect 风格：`POST <prefix>/<Method>`）。
const DASHBOARD: &str = "https://platform.stepfun.com/api/step.openapi.devcenter.Dashboard";
/// 会话续期前缀（passport 挂在 `/passport` 下，**不是** `/api`）。
const PASSPORT: &str =
    "https://platform.stepfun.com/passport/proto.api.passport.v1.PassportService";
/// 国内站 appid（国际站 `.ai` 用 20700，本项目只做国内站）。
const OASIS_APPID: &str = "10300";
const OASIS_PLATFORM: &str = "web";
/// 会话段临近过期就续（秒）。续期门槛约 15 分钟，5 分钟提前量足够稳过门槛。
const REFRESH_MARGIN_SECS: i64 = 5 * 60;

// ---------------------------------------------------------------------------
// token（cookie）处理
// ---------------------------------------------------------------------------

/// 用户粘贴内容的归一化：支持
/// - 裸 token（`xxx.yyy.zzz...aaa.bbb.ccc`）；
/// - `Oasis-Token=xxx…`；
/// - 整条 Cookie 请求头（从中挑出 `Oasis-Token`，其余字段**丢弃** —— 多带必 401）。
pub(crate) fn normalize_token(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"').trim();
    let candidate = if trimmed.contains('=') {
        trimmed
            .split(';')
            .find_map(|part| {
                let (k, v) = part.split_once('=')?;
                k.trim()
                    .eq_ignore_ascii_case("oasis-token")
                    .then(|| v.trim().to_string())
            })
            .unwrap_or_else(|| trimmed.to_string())
    } else {
        trimmed.to_string()
    };
    candidate.trim().trim_matches('"').trim().to_string()
}

/// base64url（无填充/有填充都试）解出 JWT 段（或裸段）的 payload JSON。
fn b64_json(seg: &str) -> Option<Value> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(seg)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(seg))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 拆 cookie：`access.raw + "..." + refresh.raw` → 8 个 dot 分段
/// （`[0..3]` 会话段 JWT、`[3]`/`[4]` 空段、`[5..8]` 设备段 JWT）。
fn segments(token: &str) -> Vec<&str> {
    token.split('.').collect()
}

/// 会话段 payload（`[1]`）。
fn session_claims(token: &str) -> Option<Value> {
    b64_json(segments(token).get(1)?)
}

/// 设备段 payload（`[6]`）。
fn device_claims(token: &str) -> Option<Value> {
    b64_json(segments(token).get(6)?)
}

/// 设备 id（`oasis-webid` 头要用；取自 token 里带 `device_id` 的那段 JWT）。
///
/// **必须非空且与 token 一致**：实测（匿名 token 打真实服务端）
/// - 业务接口：webid 空/垃圾 → `oasis-token is embezzled`；正确 → `not a logined oasis account`；
/// - 续期接口：webid 空 → `invalid_argument: oasis header is invalid`；垃圾反而 200。
/// 所以这里逐段扫（不写死第 6 段）以容忍点分段数差异；解不出就由调用方报"粘贴内容不对"。
pub(crate) fn device_id_of(token: &str) -> Option<String> {
    // 先试约定的设备段（[6]），再逐段兜底。
    let segs = segments(token);
    let ordered = segs
        .iter()
        .skip(6)
        .chain(segs.iter().skip(1).take(6))
        .copied();
    for seg in ordered {
        let Some(v) = b64_json(seg) else { continue };
        for key in ["device_id", "deviceId", "deviceID"] {
            if let Some(s) = v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 粘贴内容的形态诊断（解不出设备 id 时给用户的**可操作**说明）。
pub(crate) fn token_shape_hint(token: &str) -> String {
    let segs = segments(token).len();
    if segs < 3 {
        return format!(
            "粘贴的内容不是 Oasis-Token（只有 {segs} 段、不是 JWT 串）：请在 platform.stepfun.com \
             按 F12 → Application → Cookies → 复制 **Oasis-Token** 的完整值"
        );
    }
    format!(
        "解不出 Oasis-Token 里的设备 id（当前 {segs} 段，正常应为 8 段、约 650 字符）：\
         可能复制到了别的字段或被截断 —— 请在 DevTools → Application → Cookies → \
         platform.stepfun.com 复制 **Oasis-Token** 的完整值"
    )
}

/// 会话段过期时刻（unix 秒）。
pub(crate) fn session_exp_of(token: &str) -> Option<i64> {
    let v = session_claims(token)?;
    pick_num(&v.as_object()?.clone(), &["exp"]).map(|e| e as i64)
}

/// `oasis_id`（会话身份）：续期前后比对，变了 = 会话已死（服务端改发匿名 token）。
/// 真实 token 里是**数字**（`414286556075503616`），也兼容字符串形态。
pub(crate) fn oasis_id_of(token: &str) -> Option<String> {
    for claims in [session_claims(token), device_claims(token)] {
        let Some(v) = claims.as_ref().and_then(|v| {
            v.get("oasis_id")
                .or_else(|| v.get("oasisId"))
                .filter(|x| !x.is_null())
        }) else {
            continue;
        };
        let id = match v {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        return Some(id);
    }
    None
}

/// 会话段是否需要续期：**只在能解出 exp 且确实临近/已过期时**才续。
/// 解不出 exp 说明 token 形态不对，此时主动续期只会换来一条看不懂的服务端错误
/// （实测空 webid → `oasis header is invalid`）；交给"鉴权失败 → 被动续期一次"兜底。
pub(crate) fn needs_refresh(token: &str, now_sec: i64) -> bool {
    match session_exp_of(token) {
        Some(exp) => exp - now_sec <= REFRESH_MARGIN_SECS,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// 业务 RPC：`POST {DASHBOARD}/{method}`，body `{}`。
fn rpc(
    http: &ureq::Agent,
    method: &str,
    token: &str,
    webid: &str,
) -> Result<Value, (String, String)> {
    let url = format!("{DASHBOARD}/{method}");
    let result = http
        .post(&url)
        .set("Content-Type", "application/json")
        // cookie 只能含这一个字段（多带必 401）
        .set("Cookie", &format!("Oasis-Token={token}"))
        .set("oasis-appid", OASIS_APPID)
        .set("oasis-platform", OASIS_PLATFORM)
        .set("oasis-webid", webid)
        .send_string("{}");
    let (status, body) = match result {
        Ok(resp) => (200u16, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Transport(t)) => {
            return Err(("http".into(), format!("StepFun 请求失败：{t}")));
        }
    };
    let value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return Err((
                "parse".into(),
                format!("StepFun 返回无法解析（HTTP {status}）"),
            ));
        }
    };
    if let Some(err) = rpc_error(&value, status) {
        return Err(err);
    }
    Ok(value)
}

/// 错误信封：`{"code":"unauthenticated","message":"auth failed: token is expired"}`
/// （proto3 JSON 里 code 是**枚举名字符串**）+ 非 200 的兜底。
fn rpc_error(v: &Value, status: u16) -> Option<(String, String)> {
    let obj = v.as_object()?;
    let message = obj
        .get("message")
        .or_else(|| obj.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(code) = obj.get("code").and_then(Value::as_str) {
        if !code.is_empty() && code != "ok" && code != "OK" && code != "0" {
            let is_auth = matches!(code, "unauthenticated" | "permission_denied")
                || message.contains("token")
                || message.contains("auth");
            return Some((
                if is_auth { "auth" } else { "api" }.into(),
                if message.is_empty() {
                    format!("StepFun 返回 {code}")
                } else {
                    message
                },
            ));
        }
    }
    if status == 401 || status == 403 {
        return Some(("auth".into(), format!("StepFun 鉴权失败（HTTP {status}）")));
    }
    if status != 200 {
        return Some((
            "http".into(),
            if message.is_empty() {
                format!("StepFun 上游 HTTP {status}")
            } else {
                message
            },
        ));
    }
    None
}

/// 会话续期：`POST {PASSPORT}/RefreshToken`（body `{}`，无需密码）。
/// 返回 `(新 cookie, 身份是否未变)`；身份变化（`oasis_id` 不同）= 设备段已死。
fn refresh_token(
    http: &ureq::Agent,
    token: &str,
    webid: &str,
) -> Result<(String, bool), String> {
    let url = format!("{PASSPORT}/RefreshToken");
    let result = http
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Cookie", &format!("Oasis-Token={token}"))
        .set("oasis-appid", OASIS_APPID)
        .set("oasis-platform", OASIS_PLATFORM)
        .set("oasis-webid", webid)
        .send_string("{}");
    let (status, header_token, body) = match result {
        Ok(resp) => {
            let header = resp
                .header("oasis-token")
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty());
            (200u16, header, resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) => (code, None, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Transport(t)) => return Err(format!("续期请求失败：{t}")),
    };
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if let Some((_, message)) = rpc_error(&value, status) {
        return Err(format!("续期失败：{message}"));
    }
    // 优先 body 的两段 raw（`accessToken.raw` + `refreshToken.raw`），
    // 其次响应头 `oasis-token`。
    let access = value
        .get("accessToken")
        .or_else(|| value.get("access_token"))
        .and_then(|a| a.get("raw"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let refresh = value
        .get("refreshToken")
        .or_else(|| value.get("refresh_token"))
        .and_then(|r| r.get("raw"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let new_token = match (access, refresh) {
        (Some(a), Some(r)) => [a, r].join("..."),
        _ => header_token.ok_or_else(|| "续期响应里没有新 token".to_string())?,
    };
    let same = match (oasis_id_of(token), oasis_id_of(&new_token)) {
        (Some(before), Some(after)) => before == after,
        _ => true, // 解不出身份就不做判断（不误报会话已死）
    };
    Ok((new_token, same))
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 响应体解包：Connect RPC 直接返回消息对象；容忍 `{"result": {...}}` 包裹。
fn payload(v: &Value) -> &Map<String, Value> {
    if let Some(inner) = v.get("result").and_then(Value::as_object) {
        return inner;
    }
    v.as_object().unwrap_or_else(|| {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        EMPTY.get_or_init(Map::new)
    })
}

/// 剩余率（0..1）→ 已用百分比窗口。
///
/// `left` 为 `None` → 无此窗口；`left == 0` **且** 无重置时刻 → 上游语义是
/// "套餐不含该配额"（实测 Plus 套餐 5h/周字段恒 0）→ 隐藏整窗，绝不显示成 100% 已用。
fn window_from_left(
    left: Option<f64>,
    reset_at: Option<i64>,
    now: u64,
) -> Option<UsageWindow> {
    let left = left?;
    let has_reset = reset_at.is_some_and(|t| t > 0);
    if left <= 0.0 && !has_reset {
        return None;
    }
    let used = clamp_pct(js_round((1.0 - left.clamp(0.0, 1.0)) * 100.0));
    Some(UsageWindow {
        percent: Some(used),
        resets_at: reset_at.filter(|t| *t > 0),
        reset_in_sec: reset_at
            .filter(|t| *t > 0)
            .map(|t| secs_until(t, now)),
        status: None,
    })
}

/// 时间戳归一（秒 / 毫秒 / 字符串都认；0 → None）。
fn reset_secs(v: Option<&Value>) -> Option<i64> {
    let raw = v.and_then(to_num)?;
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    Some(if raw > 1e12 { (raw / 1000.0) as i64 } else { raw as i64 })
}

/// 顶层键（camelCase / snake_case 两种拼写都认）。
fn get2<'a>(obj: &'a Map<String, Value>, snake: &str, camel: &str) -> Option<&'a Value> {
    obj.get(snake)
        .filter(|v| !v.is_null())
        .or_else(|| obj.get(camel).filter(|v| !v.is_null()))
}

/// `QueryStepPlanRateLimit` → 三窗口（5 小时 / 周 / 套餐 Credit 月池）。
pub(crate) fn parse_rate_limit(v: &Value) -> (Option<UsageWindow>, Option<UsageWindow>, Option<UsageWindow>) {
    let now = now_ms();
    let root = payload(v);
    let rolling = window_from_left(
        get2(root, "five_hour_usage_left_rate", "fiveHourUsageLeftRate").and_then(to_num),
        reset_secs(get2(
            root,
            "five_hour_usage_reset_time",
            "fiveHourUsageResetTime",
        )),
        now,
    );
    let weekly = window_from_left(
        get2(root, "weekly_usage_left_rate", "weeklyUsageLeftRate").and_then(to_num),
        reset_secs(get2(root, "weekly_usage_reset_time", "weeklyUsageResetTime")),
        now,
    );
    // 套餐 Credit 月池：嵌套在 plan_credit_rate_limit 里。
    let credit = get2(root, "plan_credit_rate_limit", "planCreditRateLimit")
        .and_then(Value::as_object);
    let monthly = credit.and_then(|c| {
        window_from_left(
            get2(c, "subscription_credit_left_rate", "subscriptionCreditLeftRate")
                .and_then(to_num),
            reset_secs(get2(
                c,
                "subscription_credit_reset_time",
                "subscriptionCreditResetTime",
            )),
            now,
        )
    });
    (rolling, weekly, monthly)
}

/// `GetStepPlanStatus` → 档位（`subscription.name`，过期缀 `· 已过期`）。
pub(crate) fn parse_plan_status(v: &Value) -> Option<String> {
    let root = payload(v);
    let sub = get2(root, "subscription", "subscription")?.as_object()?;
    let name = sub
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(if plan_expired(v) {
        format!("{name} · 已过期")
    } else {
        name.to_string()
    })
}

/// `subscription.expired_at` 已过（显示层据此隐藏整行）。
pub(crate) fn plan_expired(v: &Value) -> bool {
    let root = payload(v);
    get2(root, "subscription", "subscription")
        .and_then(Value::as_object)
        .and_then(|sub| reset_secs(get2(sub, "expired_at", "expiredAt")))
        .is_some_and(|end| end * 1000 <= now_ms() as i64)
}

/// `QueryAccountBalance` → 余额（**上游单位是分** → 元）。
pub(crate) fn parse_balance(v: &Value) -> Option<Money> {
    let root = payload(v);
    let fen = pick_num(root, &["balance", "payment"])?;
    Some(Money {
        amount: fen / 100.0,
        unit: MoneyUnit::Cny,
    })
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// An auth rejection can precede the local expiry time. Refresh once and keep
/// the renewed token for subsequent status and balance requests.
fn query_rate_with_retry(
    token: &mut String,
    mut query: impl FnMut(&str) -> Result<Value, (String, String)>,
    mut refresh: impl FnMut(&str) -> Option<String>,
) -> Result<Value, (String, String)> {
    match query(token) {
        Err((code, message)) if code == "auth" => {
            let Some(fresh) = refresh(token) else { return Err((code, message)); };
            *token = fresh;
            query(token)
        }
        result => result,
    }
}

pub struct StepFunProvider {
    token: Option<String>,
    /// keyring 引用：会话续期后把新 cookie 写回（`None` = 环境变量兜底，不写回）。
    secret_ref: Option<String>,
}

impl StepFunProvider {
    pub fn new(token: Option<String>, secret_ref: Option<String>) -> Self {
        Self {
            token: token.map(|t| normalize_token(&t)).filter(|t| !t.is_empty()),
            secret_ref,
        }
    }

    /// 缺 Cookie 错误（含菜单路径与 CLI 双指引）。
    fn no_token_error(&self) -> ProviderSnapshot {
        ProviderSnapshot::error(
            ProviderId::StepFun,
            "no-cookie",
            "阶跃星辰 StepFun 未配置：右键菜单 →「供应商管理 → 阶跃星辰 StepFun → 添加账号…」\
             粘贴 Oasis-Token（platform.stepfun.com 登录后 F12 → Application → Cookies → \
             复制 Oasis-Token 的值），或执行 quota-bar setcookie stepfun <token>",
        )
    }

    /// 鉴权失败（会话段过期 / 被踢 / 设备段超 30 天）。
    fn auth_error(message: &str) -> ProviderSnapshot {
        ProviderSnapshot::error(
            ProviderId::StepFun,
            "auth",
            format!(
                "StepFun 会话已失效（{message}）：请重新登录 platform.stepfun.com 后，\
                 在「供应商管理 → 阶跃星辰 StepFun → 添加账号…」粘贴新的 Oasis-Token"
            ),
        )
    }

    /// 需要时续期会话并把新 cookie 写回凭据管理器（失败不致命：照旧用原 token）。
    fn maybe_refresh(&self, http: &ureq::Agent, token: &str, webid: &str, force: bool) -> Option<String> {
        let now_sec = (now_ms() / 1000) as i64;
        if !force && !needs_refresh(token, now_sec) {
            return None;
        }
        match refresh_token(http, token, webid) {
            Ok((new_token, same)) => {
                if !same {
                    crate::dblog::log("stepfun: refresh returned a different oasis_id（会话已死）");
                    return None;
                }
                if let Some(r) = self.secret_ref.as_deref() {
                    if secrets::put(r, &new_token).is_ok() {
                        crate::dblog::log("stepfun: session refreshed + persisted");
                    }
                }
                Some(new_token)
            }
            Err(e) => {
                crate::dblog::log(&format!("stepfun: refresh failed ({e})"));
                None
            }
        }
    }
}

impl QuotaProvider for StepFunProvider {
    fn id(&self) -> ProviderId {
        ProviderId::StepFun
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        let Some(token) = self.token.as_deref() else {
            return self.no_token_error();
        };
        // `oasis-webid` 必须与 token 内的设备 id 一致（空值会被上游判 `embezzled`
        // / `oasis header is invalid`）——解不出就是粘贴内容不对，直接给出说明，
        // 不发注定失败的请求。
        let Some(webid) = device_id_of(token) else {
            return ProviderSnapshot::error(
                ProviderId::StepFun,
                "no-cookie",
                format!("StepFun 凭据无效：{}", token_shape_hint(token)),
            );
        };
        // 主动续期（临近过期才打；对新鲜 token 是空操作，不会白刷）。
        let mut token = token.to_string();
        if let Some(fresh) = self.maybe_refresh(http, &token, &webid, false) {
            token = fresh;
        }
        // 1) 额度窗口（核心）；鉴权失败 → 被动续期一次再试。
        let rate = match query_rate_with_retry(
            &mut token,
            |current| rpc(http, "QueryStepPlanRateLimit", current, &webid),
            |current| self.maybe_refresh(http, current, &webid, true),
        ) {
            Ok(v) => v,
            Err((code, message)) => {
                crate::dblog::log(&format!("stepfun: QueryStepPlanRateLimit failed [{code}] {message}"));
                let mut snap = if code == "auth" { Self::auth_error(&message) }
                    else { ProviderSnapshot::error(ProviderId::StepFun, &code, message) };
                snap.balance = rpc(http, "QueryAccountBalance", &token, &webid)
                    .ok().and_then(|v| parse_balance(&v));
                return snap;
            }
        };
        let (rolling, weekly, monthly) = parse_rate_limit(&rate);
        let mut snap = ProviderSnapshot::new(ProviderId::StepFun);
        snap.rolling = rolling;
        snap.weekly = weekly;
        snap.monthly = monthly;

        // 2) 档位 3) 余额：尽力而为，失败静默（不影响窗口）。
        let status = rpc(http, "GetStepPlanStatus", &token, &webid).ok();
        snap.level = status.as_ref().and_then(parse_plan_status);
        snap.plan_expired = status.as_ref().is_some_and(plan_expired);
        snap.balance = rpc(http, "QueryAccountBalance", &token, &webid)
            .ok()
            .and_then(|v| parse_balance(&v));
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_retry_keeps_renewed_token_for_later_requests() {
        let mut token = "old-session".to_string();
        let mut calls = Vec::new();
        let result = query_rate_with_retry(&mut token, |t| {
            calls.push(t.to_string());
            if t == "old-session" { Err(("auth".into(), "expired".into())) }
            else { Ok(serde_json::json!({"remaining": 50})) }
        }, |_| Some("renewed-session".into())).unwrap();
        assert_eq!(calls, ["old-session", "renewed-session"]);
        assert_eq!(token, "renewed-session");
        assert_eq!(result["remaining"], 50);
    }

    #[test]
    fn auth_retry_is_bounded_and_other_errors_do_not_refresh() {
        let mut token = "session".to_string();
        let mut calls = 0;
        let result = query_rate_with_retry(&mut token, |_| {
            calls += 1;
            Err(("auth".into(), "rejected".into()))
        }, |_| Some("renewed".into()));
        assert!(result.is_err());
        assert_eq!(calls, 2);
        let result = query_rate_with_retry(&mut token,
            |_| Err(("http".into(), "unavailable".into())),
            |_| panic!("network errors must not refresh"));
        assert_eq!(result.unwrap_err().0, "http");
    }

    /// 造一段 JWT（仅 base64url 编码 payload，签名不校验）。
    fn jwt_with(claims: &Value) -> String {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.encode(br#"{"alg":"HS256"}"#);
        let payload = engine.encode(serde_json::to_vec(claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    /// 造一条完整 cookie：`会话段 + "..." + 设备段`（8 个 dot 分段）。
    fn cookie(session: &Value, device: &Value) -> String {
        [jwt_with(session), jwt_with(device)].join("...")
    }

    fn session_claims_fixture(exp: i64, oasis_id: &str) -> Value {
        serde_json::json!({"activated": true, "exp": exp, "mode": 1, "oasis_id": oasis_id})
    }

    fn device_claims_fixture(device_id: &str, oasis_id: &str) -> Value {
        serde_json::json!({"app_id": 10300, "device_id": device_id, "platform": "web", "oasis_id": oasis_id})
    }

    fn parse(json: &str) -> Value {
        serde_json::from_str(json).expect("fixture 是合法 JSON")
    }

    // ---- token / cookie ----

    #[test]
    fn normalize_token_accepts_bare_prefixed_and_full_cookie() {
        let bare = cookie(
            &session_claims_fixture(1_800_000_000, "oasis-1"),
            &device_claims_fixture("dev-1", "oasis-1"),
        );
        assert_eq!(normalize_token(&bare), bare, "裸 token 原样");
        assert_eq!(normalize_token(&format!("Oasis-Token={bare}")), bare);
        assert_eq!(normalize_token(&format!("  \"{bare}\"  ")), bare, "去空白与引号");
        // 整条 Cookie 头：只挑 Oasis-Token，其它字段丢弃（多带必 401）
        let full = format!("INGRESSCOOKIE=x; Oasis-Token={bare}; _wafdytokenv1=y");
        assert_eq!(normalize_token(&full), bare);
        assert_eq!(normalize_token(""), "");
    }

    #[test]
    fn token_segments_yield_device_id_exp_and_oasis_id() {
        let ck = cookie(
            &session_claims_fixture(1_790_000_000, "oasis-42"),
            &device_claims_fixture("dev-abc", "oasis-42"),
        );
        assert_eq!(segments(&ck).len(), 8, "两段 JWT + 中间三个点");
        assert_eq!(device_id_of(&ck).as_deref(), Some("dev-abc"));
        assert_eq!(session_exp_of(&ck), Some(1_790_000_000));
        assert_eq!(oasis_id_of(&ck).as_deref(), Some("oasis-42"));
        // 垃圾输入不 panic
        assert_eq!(device_id_of("nope"), None);
        assert_eq!(session_exp_of("a.b.c"), None);
        assert_eq!(oasis_id_of(""), None);
    }

    /// 设备 id 逐段扫：点分段数不同（不是 8 段）也能找到 `device_id`。
    #[test]
    fn device_id_is_found_even_when_segment_count_differs() {
        // 只把设备段放在前面（模拟点分形态变化）
        let odd = [jwt_with(&device_claims_fixture("dev-x", "o")), jwt_with(&session_claims_fixture(1, "o"))].join("..");
        assert_eq!(device_id_of(&odd).as_deref(), Some("dev-x"));
        // 真 token 里 oasis_id 是**数字**（匿名 RegisterDevice 实测形态）
        let numeric = serde_json::json!({"device_id":"dev-n","oasis_id":414286556075503616i64});
        let ck = cookie(
            &serde_json::json!({"activated":false,"exp":1_790_228_700i64,"oasis_id":414286556075503616i64}),
            &numeric,
        );
        assert_eq!(oasis_id_of(&ck).as_deref(), Some("414286556075503616"));
    }

    #[test]
    fn token_shape_hint_explains_bad_pastes() {
        let short = token_shape_hint("abc.def");
        assert!(short.contains("不是 Oasis-Token"), "{short}");
        let weird = token_shape_hint(&["a", "b", "c", "d"].join("."));
        assert!(weird.contains("设备 id"), "{weird}");
        assert!(weird.contains("8 段"), "{weird}");
    }

    #[test]
    fn needs_refresh_only_near_or_after_expiry() {
        let exp = 1_790_000_000i64;
        let ck = cookie(
            &session_claims_fixture(exp, "o"),
            &device_claims_fixture("d", "o"),
        );
        assert!(!needs_refresh(&ck, exp - 3600), "还有 1 小时 → 不续");
        assert!(!needs_refresh(&ck, exp - REFRESH_MARGIN_SECS - 1), "差 1 秒到门槛 → 不续");
        assert!(needs_refresh(&ck, exp - REFRESH_MARGIN_SECS), "到门槛 → 续");
        assert!(needs_refresh(&ck, exp + 10), "已过期 → 续");
        // 解不出 exp（token 形态不对）→ **不**主动续：实测空 webid 续期只会拿到
        // `oasis header is invalid` 这种看不懂的错误，交给被动路径兜底。
        assert!(!needs_refresh("junk", 0));
    }

    // ---- 额度窗口 ----

    #[test]
    fn rate_limit_maps_left_rate_to_used_and_hides_absent_quotas() {
        // 实测形态：5h/周恒 0（套餐不含该配额）+ 月池 0.875 剩余
        let v = parse(
            r#"{"status":"1","desc":"",
                "fiveHourUsageLeftRate":"0","fiveHourUsageResetTime":"0",
                "weeklyUsageLeftRate":0,"weeklyUsageResetTime":0,
                "planCreditRateLimit":{"subscriptionCreditLeftRate":0.875,
                                       "subscriptionCreditResetTime":1790000000}}"#,
        );
        let (r, w, m) = parse_rate_limit(&v);
        assert!(r.is_none(), "0 且无重置时刻 = 不含该配额 → 整窗隐藏");
        assert!(w.is_none());
        let m = m.expect("月池应有窗口");
        assert_eq!(m.percent, Some(13.0), "剩余 0.875 → 已用 12.5% → 四舍五入 13（全局同款 js_round）");
        assert_eq!(m.resets_at, Some(1_790_000_000));
        assert!(m.reset_in_sec.is_some(), "由 resets_at 推倒计时");
    }

    #[test]
    fn rate_limit_keeps_zero_left_when_reset_present() {
        // 剩余 0 **且有**重置时刻 = 真的用完了（不是"没有配额"）
        let v = parse(
            r#"{"five_hour_usage_left_rate":0,"five_hour_usage_reset_time":1790000000}"#,
        );
        let (r, _, _) = parse_rate_limit(&v);
        let r = r.expect("用完的窗口要显示");
        assert_eq!(r.percent, Some(100.0));
        assert_eq!(r.resets_at, Some(1_790_000_000));
        // 剩余 1 → 0% 已用
        let v2 = parse(r#"{"weekly_usage_left_rate":1,"weekly_usage_reset_time":1790000000}"#);
        assert_eq!(parse_rate_limit(&v2).1.expect("周窗").percent, Some(0.0));
    }

    #[test]
    fn rate_limit_accepts_snake_and_camel_and_ms_reset() {
        // camelCase 拼写 + 毫秒重置时刻
        let v = parse(
            r#"{"fiveHourUsageLeftRate":0.5,"fiveHourUsageResetTime":1790000000000,
                "weeklyUsageLeftRate":0.25,"weeklyUsageResetTime":"1790000000",
                "planCreditRateLimit":{"subscriptionCreditLeftRate":0.5,
                                       "subscriptionCreditResetTime":"1790000000"}}"#,
        );
        let (r, w, m) = parse_rate_limit(&v);
        let r = r.expect("5h 窗口");
        assert_eq!(r.percent, Some(50.0));
        assert_eq!(r.resets_at, Some(1_790_000_000), "毫秒折算成秒");
        assert_eq!(w.expect("周").percent, Some(75.0));
        assert_eq!(m.expect("月").percent, Some(50.0), "字符串数字同样认");
        // 缺字段 / 空对象 → 三窗全 None，不 panic
        assert_eq!(parse_rate_limit(&parse("{}")), (None, None, None));
        assert_eq!(parse_rate_limit(&parse("[1,2]")), (None, None, None));
        // result 包裹也认
        let wrapped = parse(r#"{"result":{"weekly_usage_left_rate":0.9,"weekly_usage_reset_time":1790000000}}"#);
        assert_eq!(parse_rate_limit(&wrapped).1.expect("周").percent, Some(10.0));
    }

    // ---- 档位 / 余额 ----

    #[test]
    fn plan_status_reads_name_and_expiry() {
        let future = (now_ms() / 1000 + 86_400) as i64;
        let v = parse(&format!(
            r#"{{"status":1,"subscription":{{"name":"Plus","expired_at":{future},"auto_renew":true}}}}"#
        ));
        assert_eq!(parse_plan_status(&v).as_deref(), Some("Plus"));
        assert!(!plan_expired(&v), "未到期");
        // 已过期 → 缀「· 已过期」+ 过期标志置位（显示层据此隐藏整行）
        let past = (now_ms() / 1000 - 86_400) as i64;
        let v2 = parse(&format!(
            r#"{{"subscription":{{"name":"Pro","expiredAt":{past}}}}}"#
        ));
        assert_eq!(parse_plan_status(&v2).as_deref(), Some("Pro · 已过期"));
        assert!(plan_expired(&v2));
        // 无套餐 / 无名 → None（不冒充档位）；缺到期时间 → 不算过期（宁显示不隐藏）
        assert_eq!(parse_plan_status(&parse(r#"{"subscription":{}}"#)), None);
        assert_eq!(parse_plan_status(&parse(r#"{}"#)), None);
        assert!(!plan_expired(&parse(r#"{"subscription":{"name":"Plus"}}"#)));
        assert!(!plan_expired(&parse("{}")));
    }

    #[test]
    fn balance_is_fen_to_yuan() {
        // 上游单位是「分」：1500 → ¥15.00
        let v = parse(r#"{"balance":"1500","payment":"1500","voucher":"0","cost_month":"0"}"#);
        assert_eq!(
            parse_balance(&v),
            Some(Money {
                amount: 15.0,
                unit: MoneyUnit::Cny
            })
        );
        // 数字形态同样认；缺字段 → None
        assert_eq!(parse_balance(&parse(r#"{"balance":12345}"#)).unwrap().amount, 123.45);
        assert_eq!(parse_balance(&parse(r#"{"cost_month":"100"}"#)), None);
        assert_eq!(parse_balance(&parse("{}")), None);
    }

    // ---- 错误信封 ----

    #[test]
    fn rpc_error_classifies_auth_and_other_codes() {
        // 实测形态：code 是枚举名字符串
        let (code, msg) = rpc_error(
            &parse(r#"{"code":"unauthenticated","message":"auth failed: token is expired"}"#),
            200,
        )
        .expect("应识别鉴权失败");
        assert_eq!(code, "auth");
        assert!(msg.contains("token is expired"), "{msg}");
        // 业务错误
        let (code, msg) =
            rpc_error(&parse(r#"{"code":"invalid_argument","message":"bad body"}"#), 200).unwrap();
        assert_eq!(code, "api");
        assert_eq!(msg, "bad body");
        // 正常响应无错误
        assert_eq!(rpc_error(&parse(r#"{"status":"1","desc":""}"#), 200), None);
        assert_eq!(rpc_error(&parse(r#"{"code":"ok"}"#), 200), None);
        // HTTP 兜底
        assert_eq!(rpc_error(&parse("{}"), 401).unwrap().0, "auth");
        assert_eq!(rpc_error(&parse("{}"), 403).unwrap().0, "auth");
        assert_eq!(rpc_error(&parse("{}"), 500).unwrap().0, "http");
        assert_eq!(rpc_error(&parse("{}"), 200), None);
    }

    #[test]
    fn missing_token_gives_menu_and_cli_guidance_without_network() {
        let p = StepFunProvider::new(None, None);
        let snap = p.fetch(&crate::providers::make_agent());
        assert_eq!(snap.error.as_deref(), Some("no-cookie"));
        let msg = snap.message.as_deref().unwrap();
        assert!(msg.contains("供应商管理"), "{msg}");
        assert!(msg.contains("setcookie stepfun"), "{msg}");
        // 空串 / 只有前缀 → 同样按未配置处置
        assert!(StepFunProvider::new(Some("   ".into()), None).token.is_none());
        assert!(StepFunProvider::new(Some("Oasis-Token=".into()), None).token.is_none());
    }

    /// 粘贴内容解不出设备 id → **不发请求**，直接给出可操作说明
    /// （实测：空 `oasis-webid` 会被上游判 `embezzled` / `oasis header is invalid`）。
    #[test]
    fn unparsable_token_reports_shape_hint_without_network() {
        let p = StepFunProvider::new(Some("not-a-jwt".into()), None);
        let snap = p.fetch(&crate::providers::make_agent());
        assert_eq!(snap.error.as_deref(), Some("no-cookie"));
        let msg = snap.message.as_deref().unwrap();
        assert!(msg.contains("不是 Oasis-Token"), "{msg}");
        // 有 JWT 形态但缺 device_id → 提示设备 id 解不出
        let no_dev = [jwt_with(&session_claims_fixture(1_790_000_000, "o")), "x".into(), "y".into()].join(".");
        let snap2 = StepFunProvider::new(Some(no_dev), None).fetch(&crate::providers::make_agent());
        assert!(snap2.message.as_deref().unwrap().contains("设备 id"), "{:?}", snap2.message);
    }

    #[test]
    fn auth_error_message_points_at_the_menu() {
        let snap = StepFunProvider::auth_error("auth failed: token is expired");
        assert_eq!(snap.error.as_deref(), Some("auth"));
        let msg = snap.message.as_deref().unwrap();
        assert!(msg.contains("添加账号"), "{msg}");
        assert!(msg.contains("token is expired"), "{msg}");
    }
}
