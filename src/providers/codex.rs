//! Codex（OpenAI / ChatGPT）套餐额度：`GET https://chatgpt.com/backend-api/wham/usage`
//! （官方 CLI `backend-client` 的 `rate_limit_status_url` 同源；Bearer + 账号头）。
//!
//! 响应形状（`codex-backend-openapi-models` 同构）：
//! ```json
//! { "plan_type": "plus",
//!   "rate_limit": { "allowed": true, "limit_reached": false,
//!     "primary_window":   { "used_percent": 12, "limit_window_seconds": 18000,
//!                           "reset_after_seconds": 3600, "reset_at": 1790000000 },
//!     "secondary_window": { "used_percent": 40, "limit_window_seconds": 604800, … } },
//!   "credits": { "has_credits": true, "unlimited": false, "balance": "12.5" } }
//! ```
//! 窗口按 `limit_window_seconds` 分槽（18000→5小时 / 604800→周 / 2592000→月），
//! 秒数认不出时按官方语义回落 primary→5小时、secondary→周。缺失值一律 None。

use serde_json::{Map, Value};

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow, WindowSlot};
use crate::providers::mimo::{clamp_pct, js_round, parse_iso8601, pick, pick_num, secs_until, to_num};
use crate::providers::QuotaProvider;

/// 官方后端基址（`https://chatgpt.com` 自动补 `/backend-api`）。
const BASE_URL: &str = "https://chatgpt.com/backend-api";
const USAGE_PATH: &str = "/wham/usage";
/// 官方 CLI 的请求头 UA（Cloudflare 侧按它放行）。
const CODEX_UA: &str = "codex-cli";

fn now_ms() -> u64 {
    crate::providers::mimo::now_ms()
}

/// 窗口秒数 → 槽位（官方三档）。
fn slot_from_seconds(secs: f64) -> Option<WindowSlot> {
    match secs as i64 {
        18000 => Some(WindowSlot::Rolling),
        604800 => Some(WindowSlot::Weekly),
        2592000 => Some(WindowSlot::Monthly),
        _ => None,
    }
}

/// `{used_percent, limit_window_seconds, reset_after_seconds, reset_at}` → 窗口。
/// `used_percent` 缺失 → None（绝不造 0）；`limit_reached` → status `rate-limited`。
fn window_of(v: &Value, limit_reached: bool) -> Option<UsageWindow> {
    let obj = v.as_object()?;
    let pct = pick_num(obj, &["used_percent", "usedPercent"])?;
    let resets_at = pick(obj, &["reset_at", "resets_at"])
        .and_then(to_num)
        .filter(|t| *t > 1e9) // 秒级时间戳（毫秒则折算）
        .map(|t| if t > 1e12 { (t / 1000.0) as i64 } else { t as i64 })
        .or_else(|| {
            pick(obj, &["reset_at", "resets_at"])
                .and_then(Value::as_str)
                .and_then(parse_iso8601)
        });
    let reset_in_sec = pick_num(obj, &["reset_after_seconds", "resetAfterSeconds"])
        .filter(|s| *s >= 0.0)
        .or_else(|| resets_at.map(|t| secs_until(t, now_ms())));
    Some(UsageWindow {
        percent: Some(js_round(clamp_pct(pct))),
        resets_at,
        reset_in_sec,
        status: if limit_reached {
            Some("rate-limited".into())
        } else {
            None
        },
    })
}

/// 解析结果：三窗口 + 档位 + credits 余额。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CodexUsage {
    pub rolling: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    pub monthly: Option<UsageWindow>,
    pub level: Option<String>,
    pub credits: Option<Money>,
}

/// 取速率限制对象：兼容 `rate_limit` / `rate_limits` / `data.*` 三种包裹。
fn rate_limit_of(payload: &Value) -> Option<&Map<String, Value>> {
    let root = payload.as_object()?;
    for key in ["rate_limit", "rate_limits"] {
        if let Some(obj) = root.get(key).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    let data = root.get("data").and_then(Value::as_object)?;
    for key in ["rate_limit", "rate_limits"] {
        if let Some(obj) = data.get(key).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    None
}

/// 顶层（含 `data` 包裹）查找对象字段。
fn field<'a>(payload: &'a Value, key: &str) -> Option<&'a Value> {
    payload
        .get(key)
        .filter(|v| !v.is_null())
        .or_else(|| payload.get("data").and_then(|d| d.get(key)).filter(|v| !v.is_null()))
}

/// 归一：三窗口（按秒数分槽，秒数缺失回落 primary/secondary）+ 档位 + credits。
pub(crate) fn parse_usage(payload: &Value) -> CodexUsage {
    let mut out = CodexUsage::default();
    if let Some(rl) = rate_limit_of(payload) {
        let limit_reached = rl
            .get("limit_reached")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 主/副窗口：先按秒数分槽，秒数认不出再按官方语义回落。
        let primary = rl.get("primary_window").filter(|v| !v.is_null());
        let secondary = rl.get("secondary_window").filter(|v| !v.is_null());
        for (v, fallback) in [(primary, WindowSlot::Rolling), (secondary, WindowSlot::Weekly)] {
            let Some(v) = v else { continue };
            let Some(win) = window_of(v, limit_reached) else {
                continue;
            };
            let slot = pick_num(v.as_object().unwrap_or(&Map::new()), &["limit_window_seconds"])
                .and_then(slot_from_seconds)
                .unwrap_or(fallback);
            let target = match slot {
                WindowSlot::Rolling => &mut out.rolling,
                WindowSlot::Weekly => &mut out.weekly,
                WindowSlot::Monthly => &mut out.monthly,
            };
            if target.is_none() {
                *target = Some(win);
            }
        }
    }
    // 档位：`plan_type`（plus / pro / team / free …）。
    out.level = field(payload, "plan_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // credits：`{has_credits, unlimited, balance}`；balance 是字符串数字。
    if let Some(c) = field(payload, "credits").and_then(Value::as_object) {
        let unlimited = c.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
        if !unlimited {
            out.credits = pick_num(c, &["balance", "remaining", "credits"]).map(|amount| Money {
                amount,
                unit: MoneyUnit::Credits,
            });
        }
    }
    out
}

/// 未登录/失效行（坏账号也占一行，带邮箱便于定位）。
fn auth_error_row(account_id: &str, email: Option<&str>, message: String) -> ProviderSnapshot {
    let mut snap = ProviderSnapshot::error(ProviderId::Codex, "auth", message);
    snap.row_key = format!("{}:{}", ProviderId::Codex.as_str(), account_id);
    snap.account_label = email.map(|e| e.to_string());
    snap
}

/// 单账号抓取：请求 + 解析（token/账号 id 由调用方给定）。
fn fetch_with(
    http: &ureq::Agent,
    token: &str,
    account_id: &str,
    email: Option<&str>,
) -> ProviderSnapshot {
    let url = format!("{BASE_URL}{USAGE_PATH}");
    let mut req = http
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .set("User-Agent", CODEX_UA);
    if !account_id.is_empty() {
        req = req.set("ChatGPT-Account-Id", account_id);
    }
    let result = req.call();
    let (status, body) = match result {
        Ok(resp) => (200u16, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Transport(t)) => {
            return row_error(
                account_id,
                email,
                "http",
                format!("Codex 请求失败：{t}"),
            );
        }
    };
    let value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return row_error(
                account_id,
                email,
                "parse",
                format!("Codex 返回无法解析（HTTP {status}）"),
            );
        }
    };
    if status == 401 || status == 403 {
        return auth_error_row(
            account_id,
            email,
            "Codex 登录已失效，请重新登录（菜单「添加供应商 → ChatGPT / Codex (OpenAI)」或 quota-bar login codex）"
                .into(),
        );
    }
    if status != 200 {
        let detail = value
            .get("detail")
            .or_else(|| value.get("error"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("HTTP {status}"));
        return row_error(account_id, email, "http", format!("Codex 上游错误：{detail}"));
    }
    let usage = parse_usage(&value);
    let mut snap = ProviderSnapshot::new(ProviderId::Codex);
    snap.row_key = format!("{}:{}", ProviderId::Codex.as_str(), account_id);
    snap.account_label = email.map(|e| e.to_string());
    snap.level = usage.level;
    snap.rolling = usage.rolling;
    snap.weekly = usage.weekly;
    snap.monthly = usage.monthly;
    snap.balance = usage.credits;
    snap
}

fn row_error(account_id: &str, email: Option<&str>, code: &str, message: String) -> ProviderSnapshot {
    let mut snap = ProviderSnapshot::error(ProviderId::Codex, code, message);
    snap.row_key = format!("{}:{}", ProviderId::Codex.as_str(), account_id);
    snap.account_label = email.map(|e| e.to_string());
    snap
}

/// 无账号表的退化路径（活动账号）：CLI `json` / 未登录提示共用。
fn fetch_active(http: &ureq::Agent) -> ProviderSnapshot {
    let mut snap = ProviderSnapshot::error(
        ProviderId::Codex,
        "auth",
        crate::oauth::codex::not_logged_in_message(),
    );
    match crate::oauth::access_token(ProviderId::Codex) {
        Ok(token) => {
            let (account_id, email) = crate::oauth::list_accounts(ProviderId::Codex)
                .into_iter()
                .find(|a| a.active)
                .map(|a| (a.id, a.email))
                .unwrap_or_default();
            snap = fetch_with(http, &token, &account_id, email.as_deref());
        }
        Err(e) => {
            snap.message = Some(e);
        }
    }
    snap
}

pub struct CodexProvider;

impl QuotaProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        fetch_active(http)
    }

    /// 每账号一行（行序 = 账号表顺序；坏账号也占一行）。
    fn fetch_multi(&self, http: &ureq::Agent) -> Vec<ProviderSnapshot> {
        let accounts = crate::oauth::account_tokens(ProviderId::Codex);
        if accounts.is_empty() {
            return vec![ProviderSnapshot::error(
                ProviderId::Codex,
                "auth",
                crate::oauth::codex::not_logged_in_message(),
            )];
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = accounts
                .iter()
                .map(|acct| {
                    let agent = http.clone();
                    let account_id = acct.account_id.clone();
                    let email = acct.email.clone();
                    let token = acct.token.clone();
                    scope.spawn(move || match token {
                        Err(e) => auth_error_row(&account_id, email.as_deref(), e),
                        Ok(t) => fetch_with(&agent, &t, &account_id, email.as_deref()),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        ProviderSnapshot::error(ProviderId::Codex, "panic", "抓取线程异常")
                    })
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Value {
        serde_json::from_str(json).expect("fixture 是合法 JSON")
    }

    fn pct(w: &Option<UsageWindow>) -> Option<f64> {
        w.as_ref().and_then(|w| w.percent)
    }

    #[test]
    fn primary_secondary_map_to_rolling_and_weekly_by_seconds() {
        let v = parse(
            r#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false,
                "primary_window":{"used_percent":12,"limit_window_seconds":18000,
                                  "reset_after_seconds":3600,"reset_at":1790000000},
                "secondary_window":{"used_percent":40,"limit_window_seconds":604800,
                                    "reset_after_seconds":86400,"reset_at":1790500000}}}"#,
        );
        let u = parse_usage(&v);
        assert_eq!(pct(&u.rolling), Some(12.0));
        assert_eq!(pct(&u.weekly), Some(40.0));
        assert_eq!(u.monthly, None);
        assert_eq!(u.level.as_deref(), Some("plus"));
        let r = u.rolling.unwrap();
        assert_eq!(r.resets_at, Some(1_790_000_000));
        assert_eq!(r.reset_in_sec, Some(3600.0), "reset_after_seconds 直用");
        assert_eq!(r.status, None);
    }

    #[test]
    fn monthly_window_slots_by_seconds_and_limit_reached_sets_status() {
        let v = parse(
            r#"{"rate_limit":{"limit_reached":true,
                "primary_window":{"used_percent":100,"limit_window_seconds":2592000,"reset_after_seconds":10}}}"#,
        );
        let u = parse_usage(&v);
        assert_eq!(pct(&u.monthly), Some(100.0), "2592000 → 月窗");
        assert!(u.rolling.is_none());
        assert_eq!(
            u.monthly.unwrap().status.as_deref(),
            Some("rate-limited")
        );
    }

    #[test]
    fn unknown_window_seconds_fall_back_to_official_primary_secondary() {
        // 秒数认不出（缺字段）→ primary=5小时、secondary=周
        let v = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":5},"secondary_window":{"used_percent":7}}}"#,
        );
        let u = parse_usage(&v);
        assert_eq!(pct(&u.rolling), Some(5.0));
        assert_eq!(pct(&u.weekly), Some(7.0));
        // reset 缺失 → None（不造 0）
        let r = u.rolling.expect("primary → rolling");
        assert_eq!(r.resets_at, None);
        assert_eq!(r.reset_in_sec, None);
    }

    #[test]
    fn credits_and_plan_parsed_and_missing_stays_none() {
        let v = parse(r#"{"plan_type":"pro","credits":{"has_credits":true,"unlimited":false,"balance":"12.5"}}"#);
        let u = parse_usage(&v);
        assert_eq!(
            u.credits,
            Some(Money {
                amount: 12.5,
                unit: MoneyUnit::Credits
            })
        );
        assert_eq!(u.level.as_deref(), Some("pro"));
        // unlimited → 不显示金额；缺字段 → None
        let v2 = parse(r#"{"credits":{"unlimited":true,"balance":"999"}}"#);
        assert_eq!(parse_usage(&v2).credits, None);
        assert_eq!(parse_usage(&parse("{}")), CodexUsage::default());
        assert_eq!(parse_usage(&parse("[1,2,3]")), CodexUsage::default());
        // 缺 used_percent 的窗口 → 整窗 None（绝不造 0）
        let v3 = parse(r#"{"rate_limit":{"primary_window":{"limit_window_seconds":18000}}}"#);
        assert_eq!(parse_usage(&v3).rolling, None);
    }

    #[test]
    fn data_wrapped_and_rate_limits_plural_are_accepted() {
        let v = parse(
            r#"{"data":{"rate_limits":{"primary_window":{"used_percent":33,"limit_window_seconds":18000}}}}"#,
        );
        assert_eq!(pct(&parse_usage(&v).rolling), Some(33.0));
        let v2 = parse(
            r#"{"data":{"plan_type":"team","rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000}}}}"#,
        );
        let u = parse_usage(&v2);
        assert_eq!(pct(&u.rolling), Some(1.0));
        assert_eq!(u.level.as_deref(), Some("team"), "data 包裹的档位同样读");
    }

    #[test]
    fn out_of_range_percent_is_clamped() {
        let v = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":350,"limit_window_seconds":18000}}}"#,
        );
        assert_eq!(pct(&parse_usage(&v).rolling), Some(100.0));
        let v2 = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":-4,"limit_window_seconds":18000}}}"#,
        );
        assert_eq!(pct(&parse_usage(&v2).rolling), Some(0.0));
    }

    #[test]
    fn error_rows_carry_account_key_and_label() {
        let row = auth_error_row("acct-1", Some("a@b.test"), "失效".into());
        assert_eq!(row.error.as_deref(), Some("auth"));
        assert_eq!(row.row_key, "codex:acct-1");
        assert_eq!(row.row_account_id(), Some("acct-1"));
        assert_eq!(row.account_label.as_deref(), Some("a@b.test"));
        let row2 = row_error("acct-2", None, "http", "boom".into());
        assert_eq!(row2.row_key, "codex:acct-2");
        assert_eq!(row2.account_label, None);
    }
}
