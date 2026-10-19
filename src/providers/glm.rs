//! GLM Coding Plan 额度抓取**共用实现**（Z.AI / 智谱两个渠道只差 origin 与凭证）。
//!
//! - 端点：`GET {origin}/api/monitor/usage/quota/limit`（无 query 参数）。
//! - 鉴权：`Authorization: <key>` **裸值、不带 Bearer**（dsh `glm.ts` `bearer:false`、
//!   win `zhipu.rs` 同款实战接口）。
//! - origin 白名单：只许 `*.z.ai` / `*.bigmodel.cn`（win `origin()` 收紧版；
//!   构造器里 origin 是硬编码常量，白名单是防御性闸门，`fetch` 先过闸再发请求）。
//! - 解析以 win `parse_limits` 为准绳、槽位对照 dsh `normGlm`：
//!   - `TOKENS_LIMIT`：`unit/number` (3,5)→5小时、(6,1)→周（dsh）；
//!     type 含 `WEEK`/`7`→周、其余→5小时（win else 分支）。
//!   - `TIME_LIMIT`：→ 月窗口（dsh 注释：MCP 工具的 one-month window；
//!     win 的 else 分支把它兜底进 Rolling，这里取 dsh 的月窗语义）。
//!   - `CREDIT_LIMIT`：→ `Money{Credits}`，amount = `100 - percentage`
//!     （win balance 展示语义：取该 metric 的 `remaining`）。
//!   - `percentage` 缺失或越界（∉0..=100）→ 该条目整体跳过，缺失即 `None`，绝不造 0。
//! - 复位字段：`resetAt`（win，数字，≥1e12 视为毫秒归一到秒）→ `resets_at`；
//!   `nextResetTime`（dsh，毫秒）→ `reset_in_sec = max(0, (t - now_ms)/1000)`。
//! - 错误信封（dsh `curlJson` 同款，先信封后状态码）：
//!   `{error:{...}}` → (`e.type` 或 `api`, `e.message`)；
//!   `{code: ≠200, msg}` → (`api`, `msg`)；HTTP 非 200 → (`http`, `upstream HTTP {n}`)。

use serde_json::Value;

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow, WindowSlot};

/// 抓一轮 GLM 额度（两个渠道共用；`zhipu.rs` 的 provider 结构体调这里）。
pub fn fetch(
    http: &ureq::Agent,
    id: ProviderId,
    origin: &str,
    key: Option<&str>,
) -> ProviderSnapshot {
    let Some(key) = key.map(str::trim).filter(|k| !k.is_empty()) else {
        return ProviderSnapshot::error(id, "no-key", no_key_message(id));
    };
    // 防御闸门：origin 必须落在智谱/z.ai 白名单内才发请求（win origin() 规则）。
    if !origin_allowed(origin) {
        return ProviderSnapshot::error(
            id,
            "endpoint",
            format!("智谱适配器拒绝未知 origin：{origin}"),
        );
    }
    let url = format!("{origin}/api/monitor/usage/quota/limit");
    let headers = [
        ("Authorization", key),
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8"),
    ];
    match http_get(http, &url, &headers) {
        Ok((status, body)) => snapshot_from_body(id, status, &body),
        Err((code, message)) => ProviderSnapshot::error(id, &code, message),
    }
}

fn no_key_message(id: ProviderId) -> String {
    match id {
        ProviderId::GlmZhipu => {
            "未配置 GLM 智谱 API Key（quota-bar setkey glm-zhipu <key> 或环境变量 ZHIPU_API_KEY / GLM_API_KEY）"
                .to_string()
        }
        _ => {
            "未配置 GLM Z.AI API Key（quota-bar setkey glm-zai <key> 或环境变量 ZAI_API_KEY / GLM_API_KEY）"
                .to_string()
        }
    }
}

/// origin 白名单：win `zhipu.rs::origin` —— 只放行 `api.z.ai`/`*.z.ai`、
/// `open.bigmodel.cn`/`*.bigmodel.cn`/`dev.bigmodel.cn`，且必须是 https。
pub fn origin_allowed(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("https://") else {
        return false;
    };
    let Some(host) = rest.split('/').next() else {
        return false;
    };
    if host.is_empty() {
        return false;
    }
    host == "api.z.ai"
        || host.ends_with(".z.ai")
        || host == "open.bigmodel.cn"
        || host.ends_with(".bigmodel.cn")
        || host == "dev.bigmodel.cn"
}

/// GET 一次并返回 `(status, body)`。非 2xx 也带 body 返回（信封可能在错误体里）；
/// 只有传输层失败才走 `Err(("exec", …))`。
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
        // 读不出错误体就交给状态码分支兜底成 http 错误。
        Err(_) => Ok((status, String::new())),
    }
}

/// 响应体 → 快照。测试直接喂 fixture 字符串（正常/缺失/信封/HTTP 状态全覆盖，不碰网络）。
pub(crate) fn snapshot_from_body(id: ProviderId, status: u16, body: &str) -> ProviderSnapshot {
    match interpret(status, body) {
        Ok(value) => parse_limits(id, &value),
        Err((code, message)) => ProviderSnapshot::error(id, &code, message),
    }
}

/// dsh `curlJson` 语义：JSON 解析 → 错误信封 → 状态码 → 非对象判空。
/// body 不是合法 JSON 时，非 200 优先报 `http`（任务约定），200 才报 `parse`。
fn interpret(status: u16, body: &str) -> Result<Value, (String, String)> {
    let value = match serde_json::from_str::<Value>(body) {
        Ok(value) => value,
        Err(_) if status == 200 => {
            return Err(("parse".into(), "invalid upstream response".into()));
        }
        Err(_) => return Err(("http".into(), format!("upstream HTTP {status}"))),
    };
    // `{error:{…}}`（OpenCode/通用 OpenAI 风格；GLM 也可能是 {error:{code,message}}）。
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
    // GLM 信封：`{code: <非200数字>, msg}`。
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

/// win `parse_limits` + dsh `normGlm` 槽位 → quota-bar 三窗口 + Credits 余额 + level。
fn parse_limits(id: ProviderId, value: &Value) -> ProviderSnapshot {
    let mut snap = ProviderSnapshot::new(id);
    let inner = value.get("data").filter(|d| !d.is_null()).unwrap_or(value);
    if let Some(level) = inner
        .get("level")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        snap.level = Some(level.to_string());
    }
    // dsh：缺 data.limits → 各窗口保持 null（不是错误、更不是 0）。
    let Some(limits) = inner.get("limits").and_then(Value::as_array) else {
        return snap;
    };
    for raw in limits {
        let Some(item) = raw.as_object() else {
            continue;
        };
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        // win 准绳：percentage 缺失或越界 → 整条跳过（缺失 ≠ 0）。
        let Some(percent) = item
            .get("percentage")
            .and_then(Value::as_f64)
            .filter(|p| (0.0..=100.0).contains(p))
        else {
            continue;
        };
        match kind {
            "TOKENS_LIMIT" => {
                let window = limit_window(item, percent);
                match token_slot(kind, item) {
                    WindowSlot::Rolling => snap.rolling = Some(window),
                    WindowSlot::Weekly => snap.weekly = Some(window),
                    WindowSlot::Monthly => snap.monthly = Some(window),
                }
            }
            // TIME_LIMIT = MCP 工具月窗（dsh）。
            "TIME_LIMIT" => snap.monthly = Some(limit_window(item, percent)),
            // CREDIT_LIMIT → 余额（win balance 语义取 remaining；首条生效，同 win 的 find-first）。
            "CREDIT_LIMIT" => {
                if snap.balance.is_none() {
                    snap.balance = Some(Money {
                        amount: 100.0 - percent,
                        unit: MoneyUnit::Credits,
                    });
                }
            }
            // 未识别的 type：dsh 直接忽略，win 的"未知配额"标签 quota-bar 无槽位 —— 不造数据。
            _ => {}
        }
    }
    snap
}

/// `TOKENS_LIMIT` 槽位：先 dsh 的 unit/number 判定，再 win 的 type 字符串判定，兜底 5 小时。
fn token_slot(kind: &str, item: &serde_json::Map<String, Value>) -> WindowSlot {
    let unit = item.get("unit").and_then(Value::as_f64);
    let number = item.get("number").and_then(Value::as_f64);
    if unit == Some(3.0) && number == Some(5.0) {
        return WindowSlot::Rolling;
    }
    if unit == Some(6.0) && number == Some(1.0) {
        return WindowSlot::Weekly;
    }
    if kind.contains("WEEK") || kind.contains('7') {
        return WindowSlot::Weekly;
    }
    WindowSlot::Rolling
}

fn limit_window(item: &serde_json::Map<String, Value>, percent: f64) -> UsageWindow {
    UsageWindow {
        percent: Some(percent),
        resets_at: reset_at(item),
        reset_in_sec: reset_in_sec(item),
        status: item
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

/// `resetAt`（win）→ unix 秒；≥1e12 视为毫秒归一。
fn reset_at(item: &serde_json::Map<String, Value>) -> Option<i64> {
    let raw = item.get("resetAt").and_then(Value::as_f64)?;
    if raw <= 0.0 {
        return None;
    }
    Some(if raw >= 1e12 { (raw / 1000.0) as i64 } else { raw as i64 })
}

/// `nextResetTime`（dsh，毫秒 epoch）→ 距重置秒数。
fn reset_in_sec(item: &serde_json::Map<String, Value>) -> Option<f64> {
    let raw = item.get("nextResetTime").and_then(Value::as_f64)?;
    if raw <= 0.0 {
        return None;
    }
    let now_ms = crate::model::now_sec() as f64 * 1000.0;
    Some(((raw - now_ms) / 1000.0).max(0.0))
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
    fn ext_glm_parses_windows_credits_and_level_from_fixture() {
        let body = fixture("ext_glm_quota.json");
        let snap = snapshot_from_body(ProviderId::GlmZai, 200, &body);
        assert!(snap.is_ok());
        assert_eq!(snap.level.as_deref(), Some("flagship"));
        // TOKENS_LIMIT (3,5) → 5 小时
        let rolling = snap.rolling.as_ref().expect("5小时窗口");
        assert_eq!(rolling.percent, Some(42.5));
        assert_eq!(rolling.resets_at, Some(1893456000));
        assert!(rolling.reset_in_sec.is_some());
        // TOKENS_LIMIT (6,1) → 周
        assert_eq!(snap.weekly.as_ref().unwrap().percent, Some(12.0));
        // TIME_LIMIT → 月
        assert_eq!(snap.monthly.as_ref().unwrap().percent, Some(63.0));
        // CREDIT_LIMIT percentage=30 → 余额 70 credits（win remaining 语义）
        let balance = snap.balance.expect("Credits 余额");
        assert_eq!(balance.amount, 70.0);
        assert_eq!(balance.unit, MoneyUnit::Credits);
        assert!(snap.has_data());
    }

    #[test]
    fn ext_glm_missing_percentage_creates_no_window_and_no_money() {
        // 缺 percentage 的条目整条跳过：窗口/余额都保持 None，绝不默认 0。
        let body = r#"{"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5},{"type":"CREDIT_LIMIT"}]}}"#;
        let snap = snapshot_from_body(ProviderId::GlmZhipu, 200, body);
        assert!(snap.is_ok());
        assert_eq!(snap.rolling, None);
        assert_eq!(snap.balance, None);
        assert!(!snap.has_data());
        // 连 limits 都没有 → 三个窗口全 None，仍然不是错误。
        let snap = snapshot_from_body(ProviderId::GlmZhipu, 200, r#"{"data":{"level":"std"}}"#);
        assert!(snap.is_ok());
        assert!(!snap.has_data());
        assert_eq!(snap.level.as_deref(), Some("std"));
        // 越界 percentage 同样不采纳。
        let snap = snapshot_from_body(
            ProviderId::GlmZhipu,
            200,
            r#"{"data":{"limits":[{"type":"TOKENS_LIMIT","percentage":120}]}}"#,
        );
        assert_eq!(snap.rolling, None);
        assert!(!snap.has_data());
    }

    #[test]
    fn ext_glm_code_envelope_maps_to_api_error() {
        let body = fixture("ext_glm_error.json");
        let snap = snapshot_from_body(ProviderId::GlmZai, 200, &body);
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert_eq!(snap.message.as_deref(), Some("无效的访问令牌"));
        assert!(!snap.has_data());
        // {error:{message}} 信封（type 缺失 → code 兜底 "api"）。
        let snap = snapshot_from_body(
            ProviderId::GlmZai,
            200,
            r#"{"error":{"code":"1231","message":"请求体格式错误"}}"#,
        );
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert_eq!(snap.message.as_deref(), Some("请求体格式错误"));
    }

    #[test]
    fn ext_glm_http_and_transport_failures_report_status() {
        // 非 200 且 body 不可解析 → http + 状态码。
        let snap = snapshot_from_body(ProviderId::GlmZai, 503, "Service Unavailable");
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 503"));
        // 非 200 但 body 是可解析 JSON、无信封 → 仍按状态码报 http。
        let snap = snapshot_from_body(ProviderId::GlmZai, 404, r#"{"detail":"gone"}"#);
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 404"));
        // 200 但 body 不是 JSON → parse。
        let snap = snapshot_from_body(ProviderId::GlmZai, 200, "<html>");
        assert_eq!(snap.error.as_deref(), Some("parse"));
        // 200 的 JSON 标量 → empty。
        let snap = snapshot_from_body(ProviderId::GlmZai, 200, "null");
        assert_eq!(snap.error.as_deref(), Some("empty"));
    }

    #[test]
    fn ext_glm_origin_allowlist_matches_win_rules() {
        assert!(origin_allowed("https://api.z.ai"));
        assert!(origin_allowed("https://open.bigmodel.cn"));
        assert!(origin_allowed("https://dev.bigmodel.cn"));
        assert!(origin_allowed("https://api.example.z.ai"));
        assert!(!origin_allowed("http://open.bigmodel.cn"));
        assert!(!origin_allowed("https://evil.example.com"));
        assert!(!origin_allowed("https://z.ai.evil.com"));
        assert!(!origin_allowed(""));
    }
}
