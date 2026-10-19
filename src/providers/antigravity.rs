//! Google Antigravity 套餐额度：`POST daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary`
//! （回退 `cloudcode-pa` → `retrieveUserQuota` → `fetchAvailableModels`）。
//! OAuth Bearer（会话存 Credential Manager，`crate::oauth`）；
//! 解析移植自 dsh `src/providers/antigravity.ts`（MIT）：
//! `usedPercent = (1 - remainingFraction) * 100`，同槽取已用最高的那条。
//!
//! 投影语义对齐 dsh `plans/routes.ts` `projectProviderUsage`（:109-136）：
//! `session` → 5小时、`weekly` → 周、`other` → 月，同槽保留 usedPercent 最高者。

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow, WindowSlot};
use crate::providers::mimo::{clamp_pct, js_round, now_ms, parse_iso8601, secs_until};
use crate::providers::QuotaProvider;

// ---- dsh antigravity.ts :45-70 常量 ----
const HOST_PRIMARY: &str = "https://daily-cloudcode-pa.googleapis.com";
const HOST_FALLBACK: &str = "https://cloudcode-pa.googleapis.com";
const SUMMARY_PATH: &str = "/v1internal:retrieveUserQuotaSummary";
const QUOTA_PATH: &str = "/v1internal:retrieveUserQuota";
const MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
// 注意：不再注入任何"默认项目"。会话 project 为空时只发空 body `{}` ——
// 实测（2026-09-23，本机 curl）`{}` 在两台 host 稳定 200，而共享项目
// `aicode-consumers` 的 `{project}` 组合持续 429/超时（共享池已耗尽）。

const TERMS_DISABLED_MESSAGE: &str = "Google 已暂停此 Antigravity 账号（Terms of Service）；请申诉后重新登录：https://forms.gle/hGzM9MEUv2a";

// ---- dsh antigravityHeaders（:322-336）7 个头的固定值 ----
const AGENT_USER_AGENT: &str =
    "antigravity/hub/2.8.0 (aidev_client; os_type=windows; arch=amd64; cl=963137146)";
const X_GOOG_API_CLIENT: &str = "google-cloud-sdk vscode_cloudshelleditor/0.1";
/// `JSON.stringify({ ideType, platform, pluginType })`（紧凑无空格）。
const CLIENT_METADATA: &str = "{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"WINDOWS\",\"pluginType\":\"GEMINI\"}";
// 不要像 dsh 那样让 attributionHeaders 的 `deepseek-harness/*` UA 覆盖 hub UA：
// 实测（2026-09-23 本机 curl A/B）Google 对该 UA 回 403
// "You do not have a valid license of this product"（落地成 403/429 全挂），
// `antigravity/hub/2.8.0 …` UA 稳定 200 —— 故只发 hub UA（见 with_headers）。

// ---------------------------------------------------------------------------
// 解析（dsh antigravity.ts :532-711）
// ---------------------------------------------------------------------------

/// 归一后的 Antigravity 窗口（dsh UsageWindow 的 slot 化版本）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgyWindow {
    kind: Kind,
    scope: Option<String>,
    used_percent: f64,
    resets_at: Option<i64>,
}

/// dsh window kind：session=5小时、weekly=周、other=月。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Session,
    Weekly,
    Other,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Session => "session",
            Kind::Weekly => "weekly",
            Kind::Other => "other",
        }
    }

    fn slot(self) -> WindowSlot {
        match self {
            Kind::Session => WindowSlot::Rolling,
            Kind::Weekly => WindowSlot::Weekly,
            Kind::Other => WindowSlot::Monthly,
        }
    }
}

/// dsh `parseRemainingFraction`：0..1 直认；(1,100] 视为百分比除以 100。
pub(crate) fn parse_remaining_fraction(v: &Value) -> Option<f64> {
    let f = v.as_f64().filter(|x| x.is_finite())?;
    let fraction = if f > 1.0 && f <= 100.0 { f / 100.0 } else { f };
    if (0.0..=1.0).contains(&fraction) {
        Some(fraction)
    } else {
        None
    }
}

/// dsh `readRemainingFraction`：直接键 → nested `remaining` 对象键。
fn read_remaining_fraction(v: &Value) -> Option<f64> {
    let obj = v.as_object()?;
    for k in ["remainingFraction", "remaining_fraction"] {
        if let Some(p) = obj.get(k).filter(|x| !x.is_null()).and_then(parse_remaining_fraction) {
            return Some(p);
        }
    }
    if let Some(rem) = obj.get("remaining").and_then(|x| x.as_object()) {
        for k in ["remainingFraction", "remaining_fraction", "fraction"] {
            if let Some(p) = rem
                .get(k)
                .filter(|x| !x.is_null())
                .and_then(parse_remaining_fraction)
            {
                return Some(p);
            }
        }
    }
    None
}

/// dsh `parseResetAt`：ISO / epoch 秒或毫秒（阈值 1e10）/ protobuf
/// `{seconds,nanos}` → unix 毫秒（这里折算成 unix 秒）。
pub(crate) fn parse_reset_at(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                parse_iso8601(s)
            }
        }
        Value::Number(n) => {
            let f = n.as_f64().filter(|x| x.is_finite())?;
            if f > 0.0 {
                let ms = if f < 10_000_000_000.0 { f * 1000.0 } else { f };
                Some((ms / 1000.0) as i64)
            } else {
                None
            }
        }
        Value::Object(o) => {
            let seconds = o.get("seconds").or_else(|| o.get("secondsValue")).or_else(|| o.get("sec"));
            let seconds_num = match seconds {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
                _ => None,
            }
            .filter(|x| x.is_finite());
            if let Some(s) = seconds_num {
                let nanos = o
                    .get("nanos")
                    .or_else(|| o.get("nanoseconds"))
                    .and_then(Value::as_f64)
                    .filter(|x| x.is_finite())
                    .unwrap_or(0.0);
                let ms = s * 1000.0 + nanos / 1_000_000.0;
                return Some((ms / 1000.0) as i64);
            }
            // 回落：timestamp 字段递归
            o.get("timestamp")
                .filter(|t| !t.is_null())
                .and_then(parse_reset_at)
        }
        _ => None,
    }
}

/// dsh `readResetAt`：键**逐个尝试**，parse 失败继续下一键（不是 ?? 短路）。
fn read_reset_at(v: &Value) -> Option<i64> {
    let obj = v.as_object()?;
    for k in [
        "resetTime",
        "reset_time",
        "resetAt",
        "reset_at",
        "nextResetTime",
        "next_reset_time",
    ] {
        if let Some(x) = obj.get(k).filter(|x| !x.is_null()) {
            if let Some(sec) = parse_reset_at(x) {
                return Some(sec);
            }
        }
    }
    None
}

/// `5\s*[- ]?hour` 正则的手写匹配（无 regex 依赖）。
fn contains_5hour(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'5' {
            let mut j = i + 1;
            while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'-' || b[j] == b' ') {
                j += 1;
            }
            if t[j.min(t.len())..].starts_with("hour") {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `30\s*[- ]?day` 正则的手写匹配。
fn contains_30day(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'3' && b[i + 1] == b'0' {
            let mut j = i + 2;
            while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b'-' || b[j] == b' ') {
                j += 1;
            }
            if t[j.min(t.len())..].starts_with("day") {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// dsh `quotaWindowKind`（:591-598）：label 正则 → session/weekly/other。
fn quota_window_kind(labels: &[Option<&str>]) -> Kind {
    let text = labels
        .iter()
        .flatten()
        .map(|s| s.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    if contains_5hour(&text)
        || text.contains("5h")
        || text.contains("session")
        || text.contains("rolling")
    {
        Kind::Session
    } else if text.contains("week") {
        Kind::Weekly
    } else if text.contains("month") || contains_30day(&text) {
        Kind::Other
    } else {
        Kind::Other
    }
}

/// dsh `quotaScope`（:600-608）：gemini → `Gemini`；claude|gpt|openai →
/// `Claude + GPT`；其他保留原文。空/非字符串 → None。
fn quota_scope(value: &Value) -> Option<String> {
    let raw = value.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    let text = raw.to_lowercase();
    if text.contains("gemini") {
        return Some("Gemini".to_string());
    }
    if text.contains("claude") || text.contains("gpt") || text.contains("openai") {
        return Some("Claude + GPT".to_string());
    }
    Some(raw.to_string())
}

/// dsh `quotaWindow`（:611-627）：剩余比例 → 已用百分比，带 reset/scope/kind。
fn quota_window(
    bucket: &Map<String, Value>,
    labels: &[Option<&str>],
    scope: Option<&str>,
    default_kind: Option<Kind>,
) -> Option<AgyWindow> {
    let remaining = read_remaining_fraction(&Value::Object(bucket.clone()))?;
    Some(AgyWindow {
        kind: default_kind.unwrap_or_else(|| quota_window_kind(labels)),
        scope: scope.map(|s| s.to_string()),
        used_percent: js_round(clamp_pct((1.0 - remaining) * 100.0)),
        resets_at: read_reset_at(&Value::Object(bucket.clone())),
    })
}

/// dsh `uniqueUsageWindows`（:630-638）：kind:scope:resetsAt 去重、保序。
fn unique_windows(windows: Vec<AgyWindow>) -> Vec<AgyWindow> {
    let mut seen: HashSet<String> = HashSet::new();
    windows
        .into_iter()
        .filter(|w| {
            let key = format!(
                "{}:{}:{}",
                w.kind.as_str(),
                w.scope.clone().unwrap_or_default(),
                w.resets_at.map(|t| t.to_string()).unwrap_or_default()
            );
            seen.insert(key)
        })
        .collect()
}

/// dsh `parseQuotaSummary`（:640-664）：首选 summary 响应 → 窗口列表（已去重）。
pub(crate) fn parse_quota_summary(payload: &Value) -> Vec<AgyWindow> {
    let root = match payload.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let response_obj;
    let body: &Map<String, Value> = match root.get("response").and_then(|v| v.as_object()) {
        Some(b) => {
            response_obj = b;
            response_obj
        }
        None => root,
    };
    let groups = body
        .get("groups")
        .or_else(|| body.get("quotaGroups"))
        .or_else(|| body.get("quota_groups"))
        .and_then(Value::as_array);
    let mut windows = Vec::new();
    for raw_group in groups.into_iter().flatten() {
        let Some(group) = raw_group.as_object() else { continue };
        let group_label = group
            .get("displayName")
            .filter(|v| v.is_string())
            .or_else(|| group.get("name").filter(|v| v.is_string()));
        let scope = group_label.and_then(quota_scope);
        let buckets = group
            .get("buckets")
            .or_else(|| group.get("limits"))
            .and_then(Value::as_array);
        for raw_bucket in buckets.into_iter().flatten() {
            let Some(bucket) = raw_bucket.as_object() else { continue };
            let labels = [
                bucket.get("bucketId").and_then(Value::as_str),
                bucket.get("bucket_id").and_then(Value::as_str),
                bucket.get("window").and_then(Value::as_str),
                bucket.get("displayName").and_then(Value::as_str),
                bucket.get("description").and_then(Value::as_str),
            ];
            if let Some(w) = quota_window(bucket, &labels, scope.as_deref(), None) {
                windows.push(w);
            }
        }
    }
    unique_windows(windows)
}

/// dsh `modelEntries`（:667-683）：对象 map 或数组 → (modelId, info)。
fn model_entries(value: &Value) -> Vec<(String, &Map<String, Value>)> {
    if let Some(obj) = value.as_object() {
        return obj
            .iter()
            .filter_map(|(id, info)| info.as_object().map(|m| (id.clone(), m)))
            .collect();
    }
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let info = item.as_object()?;
            let id = info
                .get("id")
                .or_else(|| info.get("modelId"))
                .or_else(|| info.get("model"))
                .and_then(Value::as_str)?;
            Some((id.to_string(), info))
        })
        .collect()
}

/// dsh `parseModelQuotas`（:685-711）：legacy 逐模型额度 → 两池
/// （按 scope 取最紧张的一条，防止耗尽的模型被平均值藏住）。
pub(crate) fn parse_model_quotas(payload: &Value) -> Vec<AgyWindow> {
    let root = match payload.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let response_obj;
    let body: &Map<String, Value> = match root.get("response").and_then(|v| v.as_object()) {
        Some(b) => {
            response_obj = b;
            response_obj
        }
        None => root,
    };
    let model_map = body
        .get("models")
        .or_else(|| body.get("modelQuotas"))
        .or_else(|| body.get("model_quotas"))
        .or_else(|| body.get("quotas"));
    let mut windows = Vec::new();
    for (model_id, info) in model_entries(model_map.unwrap_or(&Value::Null)) {
        let quota: &Map<String, Value> = info
            .get("quotaInfo")
            .and_then(|v| v.as_object())
            .or_else(|| info.get("quota_info").and_then(|v| v.as_object()))
            .or_else(|| info.get("quota").and_then(|v| v.as_object()))
            .unwrap_or(info);
        let display_name = info
            .get("displayName")
            .filter(|v| v.is_string())
            .and_then(Value::as_str)
            .or_else(|| info.get("label").filter(|v| v.is_string()).and_then(Value::as_str))
            .unwrap_or(&model_id);
        let scope = quota_scope(&Value::String(format!("{model_id} {display_name}")));
        let labels = [
            Some(model_id.as_str()),
            Some(display_name),
            quota.get("window").and_then(Value::as_str),
            quota.get("windowName").and_then(Value::as_str),
            quota.get("bucketId").and_then(Value::as_str),
        ];
        if let Some(w) = quota_window(quota, &labels, scope.as_deref(), Some(Kind::Session)) {
            windows.push(w);
        }
    }
    // 按 scope 分组保留 usedPercent 最高的一条（dsh :704-710，Map 首现序）。
    let mut order: Vec<Option<String>> = Vec::new();
    let mut grouped: Vec<AgyWindow> = Vec::new();
    for w in windows {
        let key = w.scope.clone();
        let pos = order.iter().position(|k| *k == key);
        match pos {
            Some(i) => {
                if w.used_percent > grouped[i].used_percent {
                    grouped[i] = w;
                }
            }
            None => {
                order.push(key);
                grouped.push(w);
            }
        }
    }
    grouped
}

/// dsh `projectProviderUsage`（routes.ts :109-136）的窗口投影：
/// session→5小时、weekly→周、other→月，**同槽取 usedPercent 最高**。
pub(crate) fn project(windows: &[AgyWindow]) -> (Option<UsageWindow>, Option<UsageWindow>, Option<UsageWindow>) {
    let now = now_ms();
    let mut out: [Option<UsageWindow>; 3] = [None, None, None];
    for w in windows {
        let idx = match w.kind.slot() {
            WindowSlot::Rolling => 0,
            WindowSlot::Weekly => 1,
            WindowSlot::Monthly => 2,
        };
        let candidate = UsageWindow {
            status: None,
            percent: Some(w.used_percent),
            resets_at: w.resets_at,
            reset_in_sec: w.resets_at.map(|t| secs_until(t, now)),
        };
        let take = match &out[idx] {
            None => true,
            // dsh: `(candidate.percent ?? -1) > (current.percent ?? -1)`
            Some(cur) => {
                candidate.percent.unwrap_or(-1.0) > cur.percent.unwrap_or(-1.0)
            }
        };
        if take {
            out[idx] = Some(candidate);
        }
    }
    (out[0].take(), out[1].take(), out[2].take())
}

/// Google 整账号停用（TOS）识别（dsh `isAntigravityTermsDisabled` 正则的手写版）。
pub(crate) fn is_terms_disabled(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("tos_violation") {
        return true;
    }
    if let Some(pos) = lower.find("service has been disabled") {
        let rest = &lower[pos..];
        let end = rest.find('\n').unwrap_or(rest.len());
        if rest[..end].contains("terms of service") {
            return true;
        }
    }
    false
}

/// credits 余额嗅探：响应里 credit/credits 相关数值字段 → `Money{Credits}`。
/// 排除 percent/usage/ratio/used/limit 语义的键（如 `creditUsagePercent` 是
/// 已用百分比不是余额）。取不到 → None，绝不造数。
fn sniff_credits(v: &Value) -> Option<Money> {
    fn key_is_credit(k: &str) -> bool {
        k.contains("credit")
            && !k.contains("percent")
            && !k.contains("usage")
            && !k.contains("ratio")
            && !k.contains("used")
            && !k.contains("limit")
    }
    let mut layer: Vec<&Value> = vec![v];
    for _ in 0..5 {
        let mut next: Vec<&Value> = Vec::new();
        for node in layer {
            let Some(obj) = node.as_object() else { continue };
            for (k, val) in obj {
                if !val.is_null() && key_is_credit(&k.to_ascii_lowercase()) {
                    let num = match val {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.trim().parse::<f64>().ok(),
                        _ => None,
                    };
                    if let Some(num) = num.filter(|n| n.is_finite() && *n >= 0.0) {
                        return Some(Money {
                            amount: num,
                            unit: MoneyUnit::Credits,
                        });
                    }
                }
            }
            for child in obj.values() {
                if child.is_object() {
                    next.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        layer = next;
    }
    None
}

// ---------------------------------------------------------------------------
// 抓取
// ---------------------------------------------------------------------------

/// dsh `antigravityHeaders`（:322-336）的头集合，但 **UA 只发 hub 版**
/// （attribution 覆盖版被 Google 403，见文件头注释的 A/B 实测）。
/// `ureq::Request::set` 按值消费，故此函数接收并返回 Request。
/// `pub(crate)`：OAuth 登录期的项目发现复用同款请求头。
pub(crate) fn with_headers(req: ureq::Request, token: &str) -> ureq::Request {
    req.set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .set("User-Agent", AGENT_USER_AGENT)
        .set("X-Goog-Api-Client", X_GOOG_API_CLIENT)
        .set("Client-Metadata", CLIENT_METADATA)
}

/// dsh `fetchAntigravityQuotaJson`（:713-746）：三端点回退的单路径请求。
///
/// 顺序：host（daily → cloudcode）× body（`{project}` → `{}`；project 为空只发
/// `{}`）。HTTP 400/403 才尝试空 body（:722-724），其他状态直接换 host；
/// 网络异常继续下一 body。成功返回解析后的 JSON。
fn fetch_quota_json(
    http: &ureq::Agent,
    path: &str,
    token: &str,
    project: &str,
) -> Result<Value, String> {
    let mut bodies: Vec<String> = Vec::new();
    if !project.is_empty() {
        bodies.push(format!("{{\"project\":{}}}", serde_json::to_string(project).unwrap_or_else(|_| "\"\"".into())));
    }
    bodies.push("{}".to_string());

    let mut last_error: Option<String> = None;
    for host in [HOST_PRIMARY, HOST_FALLBACK] {
        for body in &bodies {
            let url = format!("{host}{path}");
            let req = with_headers(http.post(&url), token);
            match req.send_string(body) {
                Ok(resp) => {
                    let text = resp.into_string().unwrap_or_default();
                    match serde_json::from_str::<Value>(&text) {
                        Ok(v) => return Ok(v),
                        Err(_) => {
                            last_error = Some(format!("invalid upstream response: {text}"));
                            continue; // dsh：.json() 抛错 → 记错继续
                        }
                    }
                }
                Err(ureq::Error::Status(status, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    last_error = Some(format!("upstream HTTP {status}: {text}"));
                    // 400/403：body 形态不合适 → 试下一个 body（dsh :722-724）。
                    // 429/404/503：**同样继续试下一个 body/host** —— dsh 遇非
                    // 400/403 直接 break 会跳过空 body，{project} 一旦限流就全盘
                    // 失败；实测空 body `{}` 可以 200，这里比 dsh 多退一步。
                    // 其余（401 等）换 host 无意义 → break。
                    if !matches!(status, 400 | 403 | 429 | 404 | 503) {
                        break;
                    }
                }
                Err(ureq::Error::Transport(t)) => {
                    last_error = Some(format!("request failed: {t}"));
                    // 网络异常 → 继续下一 body（dsh catch 不 break）
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| format!("Antigravity quota {path} request failed")))
}

/// 会话里的 project_id；登录期项目发现失败时为**空串**（fetch 侧据此只发
/// 空 body `{}`，绝不注入共享默认项目——见文件头注释的实测结论）。
fn project_from_session() -> String {
    crate::oauth::session(ProviderId::Antigravity)
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
        .and_then(|v| {
            v.get("project_id")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

/// 把一轮成功响应装进 snapshot（窗口投影 + credits 嗅探）。
fn fill_snapshot(snap: &mut ProviderSnapshot, windows: Vec<AgyWindow>, payload: &Value) -> bool {
    if windows.is_empty() {
        return false;
    }
    let (r, w, m) = project(&windows);
    snap.rolling = r;
    snap.weekly = w;
    snap.monthly = m;
    snap.balance = sniff_credits(payload);
    true
}

/// fetch 外壳（纯函数，离线可测）：`row_key` / email 透传。
/// `row_key` 空 = 归一单行键（单账号 `fetch()` 路径）；多账号传
/// `"antigravity:{account_id}"`。email = 邮箱本体（面板自己拼前缀）。
fn base_snapshot(row_key: &str, email: Option<&str>) -> ProviderSnapshot {
    let mut snap = ProviderSnapshot::new(ProviderId::Antigravity);
    snap.row_key = row_key.to_string();
    snap.account_label = email.map(|s| s.to_string());
    snap
}

/// 就地装错误：**保留** row_key/account_label —— `ProviderSnapshot::error`
/// 会新建对象丢掉行标识，多账号行必须带着邮箱入错（否则看不出是哪个号挂了）。
fn set_error(snap: &mut ProviderSnapshot, code: &str, message: impl Into<String>) {
    snap.error = Some(code.to_string());
    snap.message = Some(message.into());
}

/// token Err 的账号行：行键 `"antigravity:{account_id}"` + 邮箱 + `auth` 分类。
/// 纯函数（离线可测）。
fn auth_error_row(
    account_id: &str,
    email: Option<&str>,
    message: impl Into<String>,
) -> ProviderSnapshot {
    let mut snap = base_snapshot(&format!("antigravity:{account_id}"), email);
    set_error(&mut snap, "auth", message);
    snap
}

/// 单账号抓取主体：summary → legacy 双路径 + terms 判断 + credits 嗅探。
/// `project_id` **原样透传**：空 = 只发 `{}` body（不注入默认项目，见文件头
/// 实测结论）；`row_key` 透传进快照行。
fn fetch_with(
    http: &ureq::Agent,
    token: &str,
    project_id: &str,
    email: Option<&str>,
    row_key: &str,
) -> ProviderSnapshot {
    let mut snap = base_snapshot(row_key, email);

    // 1) 首选 summary 端点。
    let mut last_error: Option<String> = None;
    match fetch_quota_json(http, SUMMARY_PATH, token, project_id) {
        Ok(json) => {
            if fill_snapshot(&mut snap, parse_quota_summary(&json), &json) {
                return snap;
            }
        }
        Err(e) => {
            if is_terms_disabled(&e) {
                set_error(&mut snap, "usage", TERMS_DISABLED_MESSAGE);
                return snap;
            }
            last_error = Some(e);
        }
    }

    // 2) legacy 回退：retrieveUserQuota → fetchAvailableModels（同款解析）。
    for path in [QUOTA_PATH, MODELS_PATH] {
        match fetch_quota_json(http, path, token, project_id) {
            Ok(json) => {
                if fill_snapshot(&mut snap, parse_model_quotas(&json), &json) {
                    return snap;
                }
            }
            Err(e) => {
                if is_terms_disabled(&e) {
                    set_error(&mut snap, "usage", TERMS_DISABLED_MESSAGE);
                    return snap;
                }
                last_error = Some(e);
            }
        }
    }

    // 3) 全部落空：带上游信息（或"没有可用窗口"）就地入错，不 panic。
    match last_error {
        Some(e) => set_error(&mut snap, "http", e),
        None => set_error(&mut snap, "usage", "Antigravity 响应里没有可用的额度窗口"),
    }
    snap
}

pub struct AntigravityProvider;

impl AntigravityProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaProvider for AntigravityProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Antigravity
    }

    /// 单账号/活动语义（兼容保留）：活动账号 token + project/email → 单行快照，
    /// `row_key` 留空（归一 key —— 旧测试与单账号行为不变）。
    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        // OAuth access token（未登录/刷新失败的中文提示已含重新登录指引）。
        let token = match crate::oauth::access_token(ProviderId::Antigravity) {
            Ok(t) => t,
            Err(e) => return ProviderSnapshot::error(ProviderId::Antigravity, "auth", e),
        };
        let project = project_from_session();
        // 账号 label = 活动账号邮箱本体。
        let email = crate::oauth::account_label(ProviderId::Antigravity);
        fetch_with(http, &token, &project, email.as_deref(), "")
    }

    /// 多账号抓取（`run()` 走这里）：全部账号按**存储顺序**并行、逐账号一行。
    fn fetch_multi(&self, http: &ureq::Agent) -> Vec<ProviderSnapshot> {
        let accounts = crate::oauth::account_tokens(ProviderId::Antigravity);
        if accounts.is_empty() {
            // 未登录（无会话/账号表空）→ 单条现有文案行（含重新登录指引）。
            return vec![ProviderSnapshot::error(
                ProviderId::Antigravity,
                "auth",
                crate::oauth::antigravity::not_logged_in_message(),
            )];
        }
        // thread::scope（run() 同款模式）：按存储序 spawn、按序 join 收集；
        // 每账号独立 agent clone，行内失败只落进该账号的快照。
        std::thread::scope(|scope| {
            let handles: Vec<_> = accounts
                .iter()
                .map(|acct| {
                    let agent = http.clone();
                    let account_id = acct.account_id.clone();
                    let email = acct.email.clone();
                    let project_id = acct.project_id.clone();
                    let token = acct.token.clone();
                    let row_key = format!("antigravity:{account_id}");
                    scope.spawn(move || match token {
                        // token Err（未登录/损坏/刷新失败/失效）→ 坏账号也占一行
                        // 并带邮箱，条上能看出是哪个号挂了。
                        Err(e) => auth_error_row(&account_id, email.as_deref(), e),
                        Ok(t) => fetch_with(&agent, &t, &project_id, email.as_deref(), &row_key),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        ProviderSnapshot::error(ProviderId::Antigravity, "panic", "抓取线程异常")
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

    /// fixture 内容以 `include_str!`（路径必须是字面量）传入，这里只负责解析。
    fn fixture(raw: &str) -> Value {
        parse(raw)
    }

    /// 在窗口列表里找 usedPercent 精确匹配的那条。
    fn find(windows: &[AgyWindow], used: f64) -> &AgyWindow {
        windows
            .iter()
            .find(|w| w.used_percent == used)
            .unwrap_or_else(|| panic!("应存在 used={used} 的窗口"))
    }

    #[test]
    fn summary_parses_buckets_dedups_and_converts_remaining_fraction() {
        let windows = parse_quota_summary(&fixture(include_str!("../../tests/fixtures/antigravity_summary.json")));
        // 8 个 bucket → dedup 去掉 1 条完全重复的 weekly → 7 条（保序）
        assert_eq!(windows.len(), 7, "kind:scope:resetsAt 去重");
        // 重复条只剩一份 used=25
        assert_eq!(
            windows.iter().filter(|w| w.used_percent == 25.0).count(),
            1
        );
        // (1-0.4)*100 → 60 出现两条：gemini-5h（session，带 ISO reset）与
        // gemini-month（other，无 resetTime）——分别断言
        assert!(windows
            .iter()
            .any(|w| w.used_percent == 60.0 && w.kind == Kind::Session));
        let month = windows
            .iter()
            .find(|w| w.used_percent == 60.0 && w.kind == Kind::Other)
            .expect("30 day → Other 且 used=60");
        assert_eq!(month.scope.as_deref(), Some("Gemini"));
        assert_eq!(month.resets_at, None, "gemini-month 没有 resetTime");
        // nested remaining:{remainingFraction:0.05} → 95（Claude + GPT scope）
        let w = find(&windows, 95.0);
        assert_eq!(w.scope.as_deref(), Some("Claude + GPT"));
        assert_eq!(w.kind, Kind::Session);
        // epoch 毫秒 reset：(1-0.9)*100 → 10（周窗另一条）
        let weekly_ms = windows
            .iter()
            .find(|w| w.kind == Kind::Weekly && w.resets_at == Some(1_790_856_000))
            .expect("epoch 毫秒 resetTime 被解析");
        assert_eq!(weekly_ms.used_percent, 10.0);
        // proto {seconds,nanos} reset
        let proto = windows
            .iter()
            .find(|w| w.kind == Kind::Other && w.resets_at == Some(1_790_769_600))
            .expect("protobuf timestamp 被解析");
        assert_eq!(proto.used_percent, 50.0);
        // scope 规范化：Claude + GPT
        assert!(windows.iter().any(|w| w.scope.as_deref() == Some("Claude + GPT")));
    }

    #[test]
    fn projection_picks_highest_used_per_slot() {
        let windows = parse_quota_summary(&fixture(include_str!("../../tests/fixtures/antigravity_summary.json")));
        let (r, w, m) = project(&windows);
        // rolling 候选：60 / 80 / 95 → 取 95（Claude+GPT 那条，reset 1790251200）
        let r = r.expect("rolling");
        assert_eq!(r.percent, Some(95.0));
        assert_eq!(r.resets_at, Some(1_790_251_200));
        // weekly 候选：25（ISO）与 10（epoch ms）→ 25
        let w = w.expect("weekly");
        assert_eq!(w.percent, Some(25.0));
        assert_eq!(w.resets_at, Some(1_790_769_600));
        // monthly 候选：60（remainingFraction 40 → 40/100=0.4）与 50（proto）→ 60
        let m = m.expect("monthly");
        assert_eq!(m.percent, Some(60.0));
        assert_eq!(m.resets_at, None, "最高那条恰好没有 resetTime");
        // 倒计时由 resets_at 推导；fixture 过期后合法值为 0
        let ris = r.reset_in_sec.expect("推导 reset_in_sec");
        let expected = secs_until(r.resets_at.unwrap(), now_ms());
        assert!((ris - expected).abs() <= 1.0, "ris={ris}, expected={expected}");
    }

    #[test]
    fn remaining_fraction_boundary_forms() {
        // >1..100 视为百分比：40 → 0.4
        assert_eq!(parse_remaining_fraction(&parse("40")), Some(0.4));
        // 0..1 直认
        assert_eq!(parse_remaining_fraction(&parse("0.4")), Some(0.4));
        // 边界 1 → 1（不除）
        assert_eq!(parse_remaining_fraction(&parse("1")), Some(1.0));
        assert_eq!(parse_remaining_fraction(&parse("0")), Some(0.0));
        // 100 → 1.0；越界 → None
        assert_eq!(parse_remaining_fraction(&parse("100")), Some(1.0));
        assert_eq!(parse_remaining_fraction(&parse("101")), None);
        assert_eq!(parse_remaining_fraction(&parse("-0.5")), None);
        assert_eq!(parse_remaining_fraction(&parse("\"0.5\"")), None);
    }

    #[test]
    fn label_regex_kinds_map_to_slots() {
        // session / rolling / 5h / 5 hour → Rolling
        assert_eq!(quota_window_kind(&[Some("gemini-5h")]), Kind::Session);
        assert_eq!(quota_window_kind(&[Some("5 hour")]), Kind::Session);
        assert_eq!(quota_window_kind(&[Some("5 -hour")]), Kind::Session);
        assert_eq!(quota_window_kind(&[Some("rolling session")]), Kind::Session);
        // week / weekly → Weekly
        assert_eq!(quota_window_kind(&[Some("weekly")]), Kind::Weekly);
        assert_eq!(quota_window_kind(&[Some("per week")]), Kind::Weekly);
        // month / monthly / 30 day → Other（月）
        assert_eq!(quota_window_kind(&[Some("monthly")]), Kind::Other);
        assert_eq!(quota_window_kind(&[Some("30 day")]), Kind::Other);
        assert_eq!(quota_window_kind(&[Some("30-day")]), Kind::Other);
        // 无匹配 → Other（默认月）
        assert_eq!(quota_window_kind(&[Some("unknown")]), Kind::Other);
        // 多 label 拼接后判定
        assert_eq!(
            quota_window_kind(&[Some("gemini"), Some("5h bucket")]),
            Kind::Session
        );
    }

    #[test]
    fn scope_normalization_two_pools() {
        assert_eq!(
            quota_scope(&Value::String("Gemini Models".into())).as_deref(),
            Some("Gemini")
        );
        assert_eq!(
            quota_scope(&Value::String("Claude and GPT models".into())).as_deref(),
            Some("Claude + GPT")
        );
        assert_eq!(
            quota_scope(&Value::String("openai-compat".into())).as_deref(),
            Some("Claude + GPT")
        );
        // 其他池保留原文（trim 后）
        assert_eq!(
            quota_scope(&Value::String("  custom pool ".into())).as_deref(),
            Some("custom pool")
        );
        // 空 → None
        assert_eq!(quota_scope(&Value::String("   ".into())), None);
        assert_eq!(quota_scope(&Value::Number(3.into())), None);
    }

    #[test]
    fn legacy_model_quota_fallback_and_per_scope_grouping() {
        let windows = parse_model_quotas(&fixture(include_str!("../../tests/fixtures/antigravity_models.json")));
        // 两条模型行（Gemini / Claude），不同 scope 都保留
        assert_eq!(windows.len(), 2);
        let (r, w, m) = project(&windows);
        // 同 kind（legacy 默认 session）取最高：100（remaining=0 → 用尽）
        let r = r.expect("legacy → session/rolling");
        assert_eq!(r.percent, Some(100.0));
        assert_eq!(r.resets_at, Some(1_788_440_400), "2026-09-03T13:00:00Z");
        assert!(w.is_none() && m.is_none(), "legacy 只有 session 池");
        // scope 分组：两条不同 scope 各留一条（serde Map 按键排序，按 scope 找）
        assert!(windows.iter().any(|x| x.scope.as_deref() == Some("Gemini")));
        assert!(windows
            .iter()
            .any(|x| x.scope.as_deref() == Some("Claude + GPT")));
        // response 包裹被解开
        assert!(windows.iter().all(|x| x.kind == Kind::Session));
    }

    #[test]
    fn terms_disabled_detected_from_google_error_envelope() {
        let text = fixture(include_str!("../../tests/fixtures/antigravity_terms.json")).to_string();
        assert!(is_terms_disabled(&text), "TOS_VIOLATION + service disabled 都应命中");
        assert!(is_terms_disabled(
            "This service has been disabled in this account for violation of Terms of Service."
        ));
        assert!(!is_terms_disabled("PERMISSION_DENIED: API key not valid"));
        assert!(!is_terms_disabled("service has been disabled\nbut no TOS mention here"));
    }

    #[test]
    fn credits_sniff_only_when_credit_field_exists() {
        let summary = fixture(include_str!("../../tests/fixtures/antigravity_summary.json"));
        assert_eq!(
            sniff_credits(&summary),
            Some(Money { amount: 888.0, unit: MoneyUnit::Credits })
        );
        // 百分比语义的 credit 键不认（creditUsagePercent 是已用比例）
        assert!(sniff_credits(&parse(r#"{"creditUsagePercent": 5}"#)).is_none());
        assert!(sniff_credits(&parse(r#"{"usedCredits": 12}"#)).is_none());
        // 无 credit 字段 → None
        assert!(sniff_credits(&fixture(include_str!("../../tests/fixtures/antigravity_models.json"))).is_none());
        assert!(sniff_credits(&parse("{}")).is_none());
    }

    #[test]
    fn base_snapshot_passes_row_key_and_email() {
        // 单账号 fetch 路径：空 row_key + 活动邮箱
        let single = base_snapshot("", Some("me@gmail.com"));
        assert_eq!(single.provider, ProviderId::Antigravity);
        assert!(single.row_key.is_empty(), "单账号 row_key 留空 = 归一 key");
        assert_eq!(single.row_key(), "antigravity", "空串经助手归一到 provider key");
        assert_eq!(single.account_label.as_deref(), Some("me@gmail.com"));
        assert!(single.error.is_none() && single.message.is_none());
        // 多账号路径：行键透传 + 无邮箱
        let multi = base_snapshot("antigravity:0123456789abcdef01234567", None);
        assert_eq!(multi.row_key, "antigravity:0123456789abcdef01234567");
        assert_eq!(
            multi.row_account_id(),
            Some("0123456789abcdef01234567"),
            "行键后半段 = 账号 id"
        );
        assert_eq!(multi.row_provider(), ProviderId::Antigravity);
        assert!(multi.account_label.is_none());
    }

    #[test]
    fn auth_error_row_marks_failing_account_with_email() {
        // 坏账号（token Err）也占一行：行键带账号 id、label 带邮箱、分类 auth
        let row = auth_error_row(
            "0123456789abcdef01234567",
            Some("broken@gmail.com"),
            "Antigravity 登录已失效，请执行 quota-bar login antigravity 重新登录",
        );
        assert_eq!(row.provider, ProviderId::Antigravity);
        assert_eq!(row.row_key, "antigravity:0123456789abcdef01234567");
        assert_eq!(row.row_account_id(), Some("0123456789abcdef01234567"));
        assert_eq!(row.account_label.as_deref(), Some("broken@gmail.com"));
        assert_eq!(row.error.as_deref(), Some("auth"));
        assert!(row.message.as_deref().unwrap().contains("quota-bar login antigravity"));
        // 无邮箱的坏账号也不 panic
        let anon = auth_error_row("000000000000000000000000", None, "boom");
        assert!(anon.account_label.is_none());
        assert_eq!(anon.error.as_deref(), Some("auth"));
    }

    #[test]
    fn set_error_keeps_row_identity() {
        // 就地入错必须保留 row_key/email（error() 构造会丢 —— 这里防回归）
        let mut snap = base_snapshot("antigravity:abc123def456abc123def45", Some("keep@me.t"));
        set_error(&mut snap, "http", "upstream HTTP 500: boom");
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 500: boom"));
        assert_eq!(snap.row_key, "antigravity:abc123def456abc123def45");
        assert_eq!(snap.account_label.as_deref(), Some("keep@me.t"));
        // 对照：ProviderSnapshot::error 构造行键为空（归一）——正是要避免的
        let fresh = ProviderSnapshot::error(ProviderId::Antigravity, "auth", "x");
        assert!(fresh.row_key.is_empty());
        assert!(fresh.account_label.is_none());
    }

    #[test]
    fn empty_payloads_yield_no_windows_without_panic() {
        assert!(parse_quota_summary(&parse("{}")).is_empty());
        assert!(parse_quota_summary(&parse("[1,2]")).is_empty());
        assert!(parse_model_quotas(&parse("{}")).is_empty());
        let (r, w, m) = project(&[]);
        assert!(r.is_none() && w.is_none() && m.is_none());
        // 缺 remainingFraction 的 bucket 整条跳过
        let no_frac = parse(r#"{"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"x","window":"5h"}]}]}"#);
        assert!(parse_quota_summary(&no_frac).is_empty());
    }

    #[test]
    fn reset_at_accepts_iso_epoch_and_proto_shapes() {
        // ISO
        assert_eq!(parse_reset_at(&Value::String("2026-09-24T12:00:00Z".into())), Some(1_790_251_200));
        // epoch 秒（<1e10 → 当秒）
        assert_eq!(parse_reset_at(&parse("1790251200")), Some(1_790_251_200));
        // epoch 毫秒（≥1e10 → 当毫秒）
        assert_eq!(parse_reset_at(&parse("1790251200000")), Some(1_790_251_200));
        // protobuf seconds/nanos（nanos 不影响整秒）
        assert_eq!(
            parse_reset_at(&parse(r#"{"seconds": 1790251200, "nanos": 500000000}"#)),
            Some(1_790_251_200)
        );
        // seconds 为字符串
        assert_eq!(
            parse_reset_at(&parse(r#"{"seconds": "1790251200"}"#)),
            Some(1_790_251_200)
        );
        // 嵌套 timestamp 回落
        assert_eq!(
            parse_reset_at(&parse(r#"{"stamp": {"timestamp": {"seconds": 1790251200}}}"#)),
            None,
            "无 seconds/timestamp 直接键 → None"
        );
        assert_eq!(parse_reset_at(&parse(r#"{"timestamp": "2026-09-24T12:00:00Z"}"#)), Some(1_790_251_200));
        // 非法
        assert_eq!(parse_reset_at(&parse("-5")), None);
        assert_eq!(parse_reset_at(&Value::Bool(true)), None);
    }
}
