//! DeepSeek 余额（**金额-only** provider）：`GET https://api.deepseek.com/user/balance`。
//! Bearer `DEEPSEEK_API_KEY`；解析移植自 win `newapi.rs` 的 DeepSeek 分支：
//! `balance_infos[0].{currency,total_balance}` → `Money{Cny|Usd}`（currency 判断）。
//! 没有时间窗口（rolling/weekly/monthly 全 `None`），只有 `balance`；
//! 有余额时 `has_data()==true`，金额也算有效的显示内容。
//! `total_balance` 允许字符串或数字（win 同款）；**字段缺失 → balance None，绝不造 0**
//! （win 的 `unwrap_or(0.0)` 在此收紧为 None）。

use serde_json::Value;

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot};
use crate::providers::QuotaProvider;

pub struct DeepSeekProvider {
    api_key: Option<String>,
}

impl DeepSeekProvider {
    pub fn new(api_key: Option<String>) -> Self {
        Self { api_key }
    }
}

impl QuotaProvider for DeepSeekProvider {
    fn id(&self) -> ProviderId {
        ProviderId::DeepSeek
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        const ID: ProviderId = ProviderId::DeepSeek;
        let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            return ProviderSnapshot::error(
                ID,
                "no-key",
                "未配置 DeepSeek API Key（quota-bar setkey deepseek <key> 或环境变量 DEEPSEEK_API_KEY）",
            );
        };
        let url = "https://api.deepseek.com/user/balance";
        let auth = format!("Bearer {key}");
        let headers = [
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
        ];
        match http_get(http, url, &headers) {
            Ok((status, body)) => snapshot_from_body(ID, status, &body),
            Err((code, message)) => ProviderSnapshot::error(ID, &code, message),
        }
    }
}

/// 响应体 → 快照（测试直接喂 fixture，不碰网络）。
pub(crate) fn snapshot_from_body(id: ProviderId, status: u16, body: &str) -> ProviderSnapshot {
    match interpret(status, body) {
        Ok(value) => parse_balance(id, &value),
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

/// `balance_infos[0]` → `Money`。窗口恒为 None（DeepSeek 余额接口不给时间窗）。
fn parse_balance(id: ProviderId, value: &Value) -> ProviderSnapshot {
    let mut snap = ProviderSnapshot::new(id);
    let Some(first) = value
        .get("balance_infos")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    else {
        return snap;
    };
    // currency 判断：cny（忽略大小写）→ Cny，其余 → Usd；缺失按 win 兜底 CNY。
    let currency = first.get("currency").and_then(Value::as_str).unwrap_or("CNY");
    let amount = match first.get("total_balance") {
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(other) => other.as_f64(),
        None => None,
    };
    let Some(amount) = amount else {
        // total_balance 缺失/不可解析 → 不造 0（win 的 unwrap_or(0.0) 在此收紧）。
        return snap;
    };
    let unit = if currency.eq_ignore_ascii_case("cny") {
        MoneyUnit::Cny
    } else {
        MoneyUnit::Usd
    };
    snap.balance = Some(Money { amount, unit });
    snap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderId;

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
    fn ext_deepseek_parses_cny_balance_without_windows() {
        let body = fixture("ext_deepseek_balance.json");
        let snap = snapshot_from_body(ProviderId::DeepSeek, 200, &body);
        assert!(snap.is_ok());
        let balance = snap.balance.expect("余额");
        assert_eq!(balance.amount, 110.0);
        assert_eq!(balance.unit, MoneyUnit::Cny);
        assert_eq!(balance.format(), "¥110.00");
        // 金额-only：三窗口全 None，但余额足以构成有效的显示内容。
        assert!(snap.has_data(), "余额也是有效的显示内容");
        assert!(snap.rolling.is_none() && snap.weekly.is_none() && snap.monthly.is_none());
    }

    #[test]
    fn ext_deepseek_missing_balance_stays_none_not_zero() {
        // balance_infos 整个缺失 → balance None（绝不默认 0）。
        let snap = snapshot_from_body(ProviderId::DeepSeek, 200, r#"{"is_available":true}"#);
        assert!(snap.is_ok());
        assert_eq!(snap.balance, None);
        // 条目在但 total_balance 缺失 → 同样 None。
        let snap = snapshot_from_body(
            ProviderId::DeepSeek,
            200,
            r#"{"balance_infos":[{"currency":"CNY"}]}"#,
        );
        assert_eq!(snap.balance, None);
        assert!(!snap.has_data());
        // USD 币种 → Usd 单位。
        let snap = snapshot_from_body(
            ProviderId::DeepSeek,
            200,
            r#"{"balance_infos":[{"currency":"USD","total_balance":12.5}]}"#,
        );
        assert_eq!(snap.balance, Some(Money { amount: 12.5, unit: MoneyUnit::Usd }));
    }

    #[test]
    fn ext_deepseek_error_envelope_and_http_status() {
        let body = fixture("ext_deepseek_error.json");
        let snap = snapshot_from_body(ProviderId::DeepSeek, 200, &body);
        assert_eq!(snap.error.as_deref(), Some("invalid_request_error"));
        assert_eq!(
            snap.message.as_deref(),
            Some("Authentication Fails, API key required")
        );
        // HTTP 非 200 → http + 状态码。
        let snap = snapshot_from_body(ProviderId::DeepSeek, 500, r#"{"detail":"boom"}"#);
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 500"));
        // 非 JSON 错误页（502）→ http。
        let snap = snapshot_from_body(ProviderId::DeepSeek, 502, "<html>bad gateway</html>");
        assert_eq!(snap.error.as_deref(), Some("http"));
    }

    #[test]
    fn ext_deepseek_no_key_before_network() {
        let http = crate::providers::make_agent();
        let snap = DeepSeekProvider::new(None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        assert!(snap
            .message
            .as_deref()
            .unwrap()
            .contains("DEEPSEEK_API_KEY"));
    }
}
