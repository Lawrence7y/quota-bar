//! Kimi Code 套餐额度（dsh `src/plans/kimi-code.ts` 移植）：
//! - 主端点：`GET https://api.kimi.com/coding/v1/usages`
//!   （`Bearer` API key、`User-Agent: KimiCLI/1.6`）：
//!   `usage` → 周窗、`limits[]` 中 300-MINUTE 窗（回退第一条）→ 5 小时窗、
//!   `user.membership.level` → 档位。响应本身没有月窗。
//! - 可选月窗（逆向 web 接口，尽力而为）：`POST
//!   www.kimi.com/.../MembershipService/GetSubscriptionStats`，
//!   需要 `kimi-auth` cookie —— env `KIMI_AUTH_COOKIE`
//!   （dsh 语义：插件配置 `kimiCodeCookie` / 凭据 `KIMI_AUTH_TOKEN`，quota-bar 用 env 承接）。
//!   cookie 缺失或失败只丢月窗，周/5h 数据照常返回。
//! - 凭据 env（dsh `source.refs`）：`KIMI_CODE_API_KEY` / `KIMI_API_KEY`
//!   （`model.rs` 已覆盖 `KIMI_API_KEY`/`MOONSHOT_API_KEY`，registry 用 `env_fallback` 补 KIMI_CODE 前缀名）。
//! - 窗口解析 `detailWindow`：`limit<=0` 或 used/remaining 都缺 → `None`（缺一侧绝不造 0）；
//!   percent 取整（dsh `Math.round`）；`resetTime` ISO 串 → unix 秒。

use serde_json::Value;

use crate::model::{ProviderId, ProviderSnapshot, UsageWindow};
use crate::providers::QuotaProvider;

const USAGES_URL: &str = "https://api.kimi.com/coding/v1/usages";
const STATS_URL: &str =
    "https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats";
const UA_KIMI: &str = "KimiCLI/1.6";
/// dsh `KIMI_WEB_HEADERS`：web 月窗端点要求的浏览器头。
const UA_WEB: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

pub struct KimiCodeProvider {
    api_key: Option<String>,
    /// `kimi-auth` web 会话 cookie（可选月窗增强）。
    cookie: Option<String>,
}

impl KimiCodeProvider {
    pub fn new(api_key: Option<String>, cookie: Option<String>) -> Self {
        Self { api_key, cookie }
    }
}

/// dsh `refs` 补充候选（`resolve_api_key` 未命中时由 registry 兜底调用）。
pub fn env_fallback() -> Option<String> {
    ["KIMI_CODE_API_KEY", "KIMI_API_KEY"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

impl QuotaProvider for KimiCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::KimiCode
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        const ID: ProviderId = ProviderId::KimiCode;
        let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            return ProviderSnapshot::error(
                ID,
                "no-key",
                "未配置 Kimi Code API Key（quota-bar setkey kimi-code <key> 或环境变量 KIMI_CODE_API_KEY / KIMI_API_KEY）",
            );
        };
        // 1) 官方 usages：5 小时 + 周窗 + 档位。
        let auth = format!("Bearer {key}");
        let req = http
            .get(USAGES_URL)
            .set("Authorization", &auth)
            .set("User-Agent", UA_KIMI)
            .set("Accept", "application/json");
        let mut snap = match finish(req.call()) {
            Ok((status, body)) => snapshot_from_usages(ID, status, &body),
            Err((code, message)) => return ProviderSnapshot::error(ID, &code, message),
        };
        // 2) 可选 cookie → 月窗。任何失败只丢月窗（dsh best-effort 语义）。
        if let Some(cookie) = self
            .cookie
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            let cookie_header = format!("kimi-auth={cookie}");
            let bearer = format!("Bearer {cookie}");
            let post = http
                .post(STATS_URL)
                .set("Authorization", &bearer)
                .set("Cookie", &cookie_header)
                .set("User-Agent", UA_WEB)
                .set("Origin", "https://www.kimi.com")
                .set("Referer", "https://www.kimi.com/code/console")
                .set("x-msh-platform", "web")
                .set("connect-protocol-version", "1")
                .set("Content-Type", "application/json")
                .set("Accept", "application/json");
            if let Ok((status, body)) = finish(post.send_string("{}")) {
                if let Ok(value) = interpret(status, &body) {
                    if let Some(monthly) = norm_monthly(&value) {
                        snap.monthly = Some(monthly);
                    }
                }
            }
        }
        snap
    }
}

/// /usages 响应体 → 快照（测试直接喂 fixture，不碰网络）。
pub(crate) fn snapshot_from_usages(
    id: ProviderId,
    status: u16,
    body: &str,
) -> ProviderSnapshot {
    match interpret(status, body) {
        Ok(value) => {
            let (rolling, weekly) = norm_usages(&value);
            let mut snap = ProviderSnapshot::new(id);
            snap.rolling = rolling;
            snap.weekly = weekly;
            snap.level = level_of(&value);
            snap
        }
        Err((code, message)) => ProviderSnapshot::error(id, &code, message),
    }
}

fn finish(
    resp: Result<ureq::Response, ureq::Error>,
) -> Result<(u16, String), (String, String)> {
    let (status, resp) = match resp {
        Ok(resp) => (resp.status(), resp),
        Err(ureq::Error::Status(status, resp)) => (status, resp),
        Err(ureq::Error::Transport(t)) => {
            return Err(("exec".into(), format!("upstream request failed: {t}")));
        }
    };
    match resp.into_string() {
        Ok(body) => Ok((status, body)),
        Err(_) if status == 200 => Err((
            "exec".into(),
            "failed to read upstream response".to_string(),
        )),
        Err(_) => Ok((status, String::new())),
    }
}

/// dsh `curlJson` 同款语义：信封 → 状态码 → 判空；body 非 JSON 时非 200 报 http。
fn interpret(status: u16, body: &str) -> Result<Value, (String, String)> {
    let value = match serde_json::from_str::<Value>(body) {
        Ok(value) => value,
        Err(_) if status == 200 => {
            return Err(("parse".into(), "invalid upstream response".into()));
        }
        Err(_) => return Err(("http".into(), format!("upstream HTTP {status}"))),
    };
    if let Some(e) = value.get("error").filter(|e| e.is_object()) {
        let code = e
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "api".to_string());
        let message = e
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "API error".to_string());
        return Err((code, message));
    }
    if let Some(num) = value.get("code").and_then(Value::as_f64) {
        if num != 200.0 {
            let message = value
                .get("msg")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("API error (code {})", num as i64));
            return Err(("api".into(), message));
        }
    }
    if status != 200 {
        return Err(("http".into(), format!("upstream HTTP {status}")));
    }
    if !value.is_object() && !value.is_array() {
        return Err(("empty".into(), "empty response".into()));
    }
    Ok(value)
}

/// dsh `normKimi`：`usage` → 周窗；`limits[]` 里 300-MINUTE 窗（回退第一条）→ 5 小时窗。
fn norm_usages(value: &Value) -> (Option<UsageWindow>, Option<UsageWindow>) {
    let Some(obj) = value.as_object() else {
        return (None, None);
    };
    let mut weekly = None;
    if let Some(usage) = obj.get("usage") {
        if usage.is_object() {
            weekly = detail_window(usage);
        }
    }
    let mut rolling = None;
    if let Some(limits) = obj.get("limits").and_then(Value::as_array) {
        let mut five_hour: Option<&Value> = None;
        for raw in limits {
            if !raw.is_object() {
                continue;
            }
            let win = raw.get("window");
            let matched = win
                .filter(|w| w.is_object())
                .map(|w| {
                    let duration = w.get("duration").and_then(Value::as_f64);
                    let time_unit = w.get("timeUnit").and_then(Value::as_str).unwrap_or("");
                    duration == Some(300.0) && time_unit.contains("MINUTE")
                })
                .unwrap_or(false);
            if matched {
                five_hour = Some(raw);
                break;
            }
        }
        if five_hour.is_none() {
            five_hour = limits.first();
        }
        if let Some(item) = five_hour {
            if let Some(detail) = item.get("detail") {
                if detail.is_object() {
                    rolling = detail_window(detail);
                }
            }
        }
    }
    (rolling, weekly)
}

/// dsh `detailWindow`：`{limit, used, remaining, resetTime}` → 整数百分比窗口。
fn detail_window(d: &Value) -> Option<UsageWindow> {
    if !d.is_object() {
        return None;
    }
    let limit = to_num(d.get("limit"));
    let mut used = to_num(d.get("used"));
    if used.is_none() {
        if let (Some(limit_val), Some(remaining)) = (limit, to_num(d.get("remaining"))) {
            used = Some(limit_val - remaining);
        }
    }
    let limit = limit?;
    if limit <= 0.0 {
        return None;
    }
    let used = used?;
    let percent = ((used.clamp(0.0, limit) / limit) * 100.0).round();
    let resets_at = d
        .get("resetTime")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(epoch_from_str);
    Some(UsageWindow {
        percent: Some(percent),
        resets_at,
        reset_in_sec: None,
        status: None,
    })
}

/// dsh `user.membership.level` → 档位。
fn level_of(value: &Value) -> Option<String> {
    value
        .get("user")?
        .get("membership")?
        .get("level")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// dsh `normKimiMonthly`：`subscriptionBalance.{amountUsedRatio|kimiCodeUsedRatio}` → 月窗。
fn norm_monthly(value: &Value) -> Option<UsageWindow> {
    let bal = value.get("subscriptionBalance")?;
    if !bal.is_object() {
        return None;
    }
    let ratio = to_num(bal.get("amountUsedRatio")).or_else(|| to_num(bal.get("kimiCodeUsedRatio")))?;
    if !ratio.is_finite() {
        return None;
    }
    let percent = if (0.0..=1.0).contains(&ratio) {
        ratio * 100.0
    } else {
        ratio
    };
    let percent = percent.clamp(0.0, 100.0).round();
    let resets_at = bal
        .get("expireTime")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(epoch_from_str);
    Some(UsageWindow {
        percent: Some(percent),
        resets_at,
        reset_in_sec: None,
        status: None,
    })
}

/// 数字或数字字符串（dsh `toNum`）。
fn to_num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

/// 复位时间串 → unix 秒：纯数字（秒/毫秒 epoch）或 ISO-8601。
fn epoch_from_str(s: &str) -> Option<i64> {
    let t = s.trim();
    if let Ok(n) = t.parse::<f64>() {
        if n > 1e9 {
            return Some(if n >= 1e12 {
                (n / 1000.0) as i64
            } else {
                n as i64
            });
        }
    }
    iso_to_epoch_sec(t)
}

/// 最小 ISO-8601 解析（`2030-01-01T00:00:00Z` / `…+08:00` / 带小数秒）→ unix 秒。
fn iso_to_epoch_sec(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    fn digit(b: &[u8], i: usize) -> Option<i64> {
        let c = *b.get(i)?;
        c.is_ascii_digit().then(|| (c - b'0') as i64)
    }
    fn two(b: &[u8], i: usize) -> Option<i64> {
        Some(digit(b, i)? * 10 + digit(b, i + 1)?)
    }
    fn four(b: &[u8], i: usize) -> Option<i64> {
        Some(digit(b, i)? * 1000 + digit(b, i + 1)? * 100 + digit(b, i + 2)? * 10 + digit(b, i + 3)?)
    }
    let year = four(b, 0)?;
    (b[4] == b'-').then_some(())?;
    let month = two(b, 5)?;
    (b[7] == b'-').then_some(())?;
    let day = two(b, 8)?;
    let sep = b[10];
    if sep != b'T' && sep != b't' && sep != b' ' {
        return None;
    }
    let hour = two(b, 11)?;
    (b[13] == b':').then_some(())?;
    let minute = two(b, 14)?;
    (b[16] == b':').then_some(())?;
    let second = two(b, 17)?;
    let mut idx = 19;
    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
    }
    let offset_min: i64 = match b.get(idx) {
        Some(b'Z') | Some(b'z') => 0,
        Some(b'+') | Some(b'-') => {
            let sign = if b[idx] == b'-' { -1 } else { 1 };
            if idx + 6 > b.len() || b[idx + 3] != b':' || idx + 6 != b.len() {
                return None;
            }
            let oh = two(b, idx + 1)?;
            let om = two(b, idx + 4)?;
            sign * (oh * 60 + om)
        }
        _ => return None,
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset_min * 60)
}

/// days since 1970-01-01（Howard Hinnant `days_from_civil`）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(path),
        )
        .unwrap_or_else(|e| panic!("读取 fixture {path} 失败：{e}"))
    }

    #[test]
    fn ext_kimi_parses_rolling_weekly_and_level_from_fixture() {
        let body = fixture("ext_kimi_usages.json");
        let snap = snapshot_from_usages(ProviderId::KimiCode, 200, &body);
        assert!(snap.is_ok());
        // 5 小时窗来自 300-MINUTE 条目的 detail：250/1000 → 25%
        let rolling = snap.rolling.as_ref().expect("5小时窗口");
        assert_eq!(rolling.percent, Some(25.0));
        assert_eq!(rolling.resets_at, Some(1893542400)); // 2030-01-02T00:00:00Z
        // 周窗来自顶层 usage：500000/2000000 → 25%
        let weekly = snap.weekly.as_ref().expect("周窗口");
        assert_eq!(weekly.percent, Some(25.0));
        assert_eq!(weekly.resets_at, Some(1893456000)); // 2030-01-01T00:00:00Z
        // 月窗只可能来自 cookie 端点，这里没有 → None
        assert_eq!(snap.monthly, None);
        assert_eq!(snap.level.as_deref(), Some("pro"));
        assert!(snap.has_data());
    }

    #[test]
    fn ext_kimi_missing_detail_side_creates_no_window() {
        // usage 只有 limit、没有 used/remaining → 周窗 None（缺一侧绝不造 0）。
        let snap = snapshot_from_usages(
            ProviderId::KimiCode,
            200,
            r#"{"usage":{"limit":100},"limits":[]}"#,
        );
        assert!(snap.is_ok());
        assert_eq!(snap.weekly, None);
        assert_eq!(snap.rolling, None);
        assert!(!snap.has_data());
        // limit<=0 → 拒绝造窗。
        let snap = snapshot_from_usages(
            ProviderId::KimiCode,
            200,
            r#"{"limits":[{"window":{"duration":300,"timeUnit":"MINUTE"},"detail":{"limit":0,"used":0}}]}"#,
        );
        assert_eq!(snap.rolling, None);
        // used 缺失但 remaining 在 → used = limit-remaining（dsh 推导路径）。
        let snap = snapshot_from_usages(
            ProviderId::KimiCode,
            200,
            r#"{"usage":{"limit":100,"remaining":75},"limits":[{"detail":{"limit":100,"remaining":75,"resetTime":"1893456000"}}]}"#,
        );
        assert_eq!(snap.weekly.as_ref().unwrap().percent, Some(25.0));
        assert_eq!(snap.rolling.as_ref().unwrap().percent, Some(25.0));
        assert_eq!(snap.rolling.as_ref().unwrap().resets_at, Some(1893456000));
    }

    #[test]
    fn ext_kimi_monthly_comes_from_subscription_stats_fixture() {
        let body = fixture("ext_kimi_stats.json");
        let monthly = norm_monthly(&serde_json::from_str::<Value>(&body).unwrap())
            .expect("月窗");
        // amountUsedRatio 0.63 → 63%（优先于 kimiCodeUsedRatio）
        assert_eq!(monthly.percent, Some(63.0));
        assert_eq!(monthly.resets_at, Some(1896134400)); // 2030-02-01T00:00:00Z
        // 比率 >1 → 当作百分比并钳到 100。
        let v: Value = serde_json::from_str(r#"{"subscriptionBalance":{"kimiCodeUsedRatio":150}}"#).unwrap();
        assert_eq!(norm_monthly(&v).unwrap().percent, Some(100.0));
        // 缺比率 / 缺 subscriptionBalance → None（不造 0）。
        let v: Value = serde_json::from_str(r#"{"subscriptionBalance":{}}"#).unwrap();
        assert_eq!(norm_monthly(&v), None);
        assert_eq!(norm_monthly(&serde_json::from_str("{}").unwrap()), None);
    }

    #[test]
    fn ext_kimi_error_envelope_and_http_status() {
        let snap = snapshot_from_usages(
            ProviderId::KimiCode,
            200,
            r#"{"error":{"message":"quota exceeded"}}"#,
        );
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert_eq!(snap.message.as_deref(), Some("quota exceeded"));
        // dsh 注释里的典型场景：Kimi 401/403 → http + 状态码。
        let snap = snapshot_from_usages(ProviderId::KimiCode, 401, "");
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 401"));
        let snap = snapshot_from_usages(ProviderId::KimiCode, 200, "<html>");
        assert_eq!(snap.error.as_deref(), Some("parse"));
    }

    #[test]
    fn ext_kimi_no_key_before_network() {
        let http = crate::providers::make_agent();
        let snap = KimiCodeProvider::new(None, None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        assert!(snap.message.as_deref().unwrap().contains("setkey kimi-code"));
        // 空白 key 同样视为未配置。
        let snap = KimiCodeProvider::new(Some("  ".into()), None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
    }

    #[test]
    fn ext_kimi_iso_parser_handles_offsets_and_numeric_epochs() {
        assert_eq!(iso_to_epoch_sec("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_epoch_sec("1970-01-01T08:00:00+08:00"), Some(0));
        assert_eq!(iso_to_epoch_sec("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_to_epoch_sec("2030-01-01T00:00:00Z"), Some(1893456000));
        assert_eq!(iso_to_epoch_sec("not-a-date"), None);
        // 纯数字串按 epoch 归一（毫秒自动降秒）。
        assert_eq!(epoch_from_str("1893456000"), Some(1893456000));
        assert_eq!(epoch_from_str("1893456000000"), Some(1893456000));
    }
}
