//! OpenRouter key 额度：`GET https://openrouter.ai/api/v1/auth/key`。
//! Bearer `OPENROUTER_API_KEY`；解析移植自 win `openrouter.rs`（含其严格缺失字段测试）：
//! - `data.{usage,limit}` 两侧都有且 `limit > 0` → `Money{Usd, amount: limit-usage}`（余额）
//!   + 派生百分比窗口（`usage/limit*100` 钳 0..100）。
//! - 缺任一侧 / `limit <= 0` → 不造余额也不造百分比（绝不默认 0）：
//!   win 的 `remaining=None` 在 quota-bar 的余额槽同样显示不出来 —— 缺失即 None。
//! - 两侧都缺失 → 错误（win `metrics_for_key` 直接 `Err`，不是空数据）。
//! - `is_free_tier: true` → `level = "免费层"`（win 用它拼免费层标签）。
//! - 派生百分比没有时间语义（win `WindowKind::None`），quota-bar 必须落一个槽 ——
//!   取 Monthly（"月"标签的重置暗示最弱；resets_at 保持 None，不会显示假倒计时）。

use serde_json::Value;

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow};
use crate::providers::QuotaProvider;

const URL: &str = "https://openrouter.ai/api/v1/auth/key";

pub struct OpenRouterProvider {
    api_key: Option<String>,
}

impl OpenRouterProvider {
    pub fn new(api_key: Option<String>) -> Self {
        Self { api_key }
    }
}

impl QuotaProvider for OpenRouterProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenRouter
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        const ID: ProviderId = ProviderId::OpenRouter;
        let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            return ProviderSnapshot::error(
                ID,
                "no-key",
                "未配置 OpenRouter API Key（quota-bar setkey openrouter <key> 或环境变量 OPENROUTER_API_KEY）",
            );
        };
        let auth = format!("Bearer {key}");
        let headers = [
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
        ];
        match http_get(http, URL, &headers) {
            Ok((status, body)) => snapshot_from_body(ID, status, &body),
            Err((code, message)) => ProviderSnapshot::error(ID, &code, message),
        }
    }
}

/// 响应体 → 快照（测试直接喂 fixture / 手写 JSON，不碰网络）。
pub(crate) fn snapshot_from_body(id: ProviderId, status: u16, body: &str) -> ProviderSnapshot {
    match interpret(status, body) {
        Ok(value) => parse_key(id, &value),
        Err((code, message)) => ProviderSnapshot::error(id, &code, message),
    }
}

fn http_get(
    http: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, String), (String, String)> {
    let mut req = http.get(url);
    for (name, value) in headers {
        req = req.set(name, value);
    }
    let (status, resp) = match req.call() {
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

/// win `metrics_for_key` 的四分支映射到 (余额, 派生百分比窗口)。
fn parse_key(id: ProviderId, value: &Value) -> ProviderSnapshot {
    // win：`res.get("data").unwrap_or(&res)` —— 缺 data 时退回顶层。
    let data = value.get("data").filter(|d| !d.is_null()).unwrap_or(value);
    let usage = num(data.get("usage"));
    let limit = num(data.get("limit"));
    let is_free_tier = data
        .get("is_free_tier")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut snap = ProviderSnapshot::new(id);
    if is_free_tier {
        snap.level = Some("免费层".to_string());
    }
    match (usage, limit) {
        // 两侧齐备且限额有效 → 余额 = 剩余，百分比 = 已用占比（win 同款钳位）。
        (Some(usage_val), Some(limit_val)) if limit_val > 0.0 => {
            let remaining = (limit_val - usage_val).max(0.0);
            // 先乘后除：对 25.5/100 这类常见值保持精确（win 是先除后乘，仅差 ulp）。
            let percent = ((usage_val * 100.0) / limit_val).clamp(0.0, 100.0);
            snap.balance = Some(Money {
                amount: remaining,
                unit: MoneyUnit::Usd,
            });
            snap.monthly = Some(UsageWindow {
                percent: Some(percent),
                resets_at: None,
                reset_in_sec: None,
                status: None,
            });
            snap
        }
        // usage-only（免费层/无限额/limit<=0）：win 的 remaining=None —— 不造余额、不造百分比。
        (Some(_), _) => snap,
        // limit-only：用量未知，win 拒绝伪造"剩余全额" → 余额/窗口都 None。
        (None, Some(_)) => snap,
        // 两侧都缺失：win 直接报错，不展示假数据。
        (None, None) => ProviderSnapshot::error(
            id,
            "api",
            "OpenRouter 未返回可用额度字段（usage/limit 均缺失）",
        ),
    }
}

/// 数字或数字字符串（win 只收 f64；防御性兼容上游偶发字符串）。
fn num(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
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
    fn ext_openrouter_derives_money_and_percent_when_both_sides_present() {
        let body = fixture("ext_openrouter_key.json");
        let snap = snapshot_from_body(ProviderId::OpenRouter, 200, &body);
        assert!(snap.is_ok());
        // 余额 = limit - usage = 100 - 25.5
        let balance = snap.balance.expect("余额");
        assert_eq!(balance.amount, 74.5);
        assert_eq!(balance.unit, MoneyUnit::Usd);
        // 派生百分比 = 25.5%（win metrics[1] percent → Monthly 槽）
        let monthly = snap.monthly.as_ref().expect("派生百分比窗口");
        assert_eq!(monthly.percent, Some(25.5));
        assert_eq!(monthly.resets_at, None);
        assert_eq!(snap.rolling, None);
        assert_eq!(snap.weekly, None);
        assert_eq!(snap.level, None);
        // 数字字符串兼容：usage "10.5" / limit "40" → 余额 29.5、26.25%
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"data":{"usage":"10.5","limit":"40"}}"#,
        );
        assert_eq!(snap.balance.unwrap().amount, 29.5);
        assert_eq!(snap.monthly.unwrap().percent, Some(26.25));
    }

    #[test]
    fn ext_openrouter_missing_side_fakes_no_zero() {
        // 只有 limit（用量未知）→ 不造"剩余全额"，不造百分比。
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"data":{"limit":100,"is_free_tier":false}}"#,
        );
        assert!(snap.is_ok());
        assert_eq!(snap.balance, None);
        assert_eq!(snap.monthly, None);
        // 只有 usage（免费层）→ 不造余额、不造百分比；level 标记免费层。
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"data":{"usage":10.0,"limit":null,"is_free_tier":true}}"#,
        );
        assert!(snap.is_ok());
        assert_eq!(snap.balance, None);
        assert_eq!(snap.monthly, None);
        assert_eq!(snap.level.as_deref(), Some("免费层"));
        // limit <= 0 视为无效限额 → 同 usage-only，不计算百分比。
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"data":{"usage":5.0,"limit":0}}"#,
        );
        assert!(snap.is_ok());
        assert_eq!(snap.balance, None);
        assert_eq!(snap.monthly, None);
        // usage 超限 → 余额钳 0（上游真实观测），百分比钳 100。
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"data":{"usage":130.0,"limit":100}}"#,
        );
        assert_eq!(snap.balance.unwrap().amount, 0.0);
        assert_eq!(snap.monthly.unwrap().percent, Some(100.0));
    }

    #[test]
    fn ext_openrouter_both_missing_is_error_not_empty() {
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"data":{"is_free_tier":false}}"#,
        );
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert!(snap
            .message
            .as_deref()
            .unwrap()
            .contains("usage/limit 均缺失"));
        assert!(!snap.has_data());
    }

    #[test]
    fn ext_openrouter_error_envelope_and_http_status() {
        let snap = snapshot_from_body(
            ProviderId::OpenRouter,
            200,
            r#"{"error":{"message":"Invalid API key"}}"#,
        );
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert_eq!(snap.message.as_deref(), Some("Invalid API key"));
        let snap = snapshot_from_body(ProviderId::OpenRouter, 401, "Unauthorized");
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 401"));
        let snap = snapshot_from_body(ProviderId::OpenRouter, 200, "<html>");
        assert_eq!(snap.error.as_deref(), Some("parse"));
    }

    #[test]
    fn ext_openrouter_no_key_before_network() {
        let http = crate::providers::make_agent();
        let snap = OpenRouterProvider::new(None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        assert!(snap
            .message
            .as_deref()
            .unwrap()
            .contains("OPENROUTER_API_KEY"));
    }
}
