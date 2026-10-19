//! Command Code 套餐额度（dsh `src/plans/command-code.ts` 移植）。
//!
//! **API key / Bearer 类，不是 OAuth**（源码 `source = { endpoint, bearer: true,
//! refs: [...] }`，凭据是静态 key），所以本批次实现：
//! - 端点：`GET https://api.command-code.com/v1/usage`，`Authorization: Bearer <key>`。
//! - 凭据 env（dsh `refs`）：`COMMAND_CODE_API_KEY` / `COMMANDCODE_API_KEY` /
//!   `COMMAND_API_KEY`。`model.rs::env_key_names` 把 command-code 归进了 OAuth 空表 ——
//!   keyring 走 `setkey` 正常，env 由 registry 调本文件的 `env_fallback()` 补齐。
//! - 响应"没有严格版本化"，dsh 复用弹性归一化 `normMimo`（`mimo.ts`）——
//!   本文件**完整移植**其窗口/档位分支（detail 窗、TOKENS/TIME 类型、direct 字段、
//!   usage、quotas/credits 容器、windows[]、config、兜底、rate_limit，全程 first-wins）。
//!   （与代理 A 的 `mimo.rs` 各自移植、互不引用 —— 共享代码无处安放：mod.rs 只许加声明与注册行。）

use serde_json::{Map, Value};

use crate::model::{ProviderId, ProviderSnapshot, UsageWindow, WindowSlot};
use crate::providers::QuotaProvider;

const URL: &str = "https://api.command-code.com/v1/usage";

pub struct CommandCodeProvider {
    api_key: Option<String>,
}

impl CommandCodeProvider {
    pub fn new(api_key: Option<String>) -> Self {
        Self { api_key }
    }
}

/// dsh `refs` 候选（`resolve_api_key` 未命中时由 registry 兜底调用）。
pub fn env_fallback() -> Option<String> {
    ["COMMAND_CODE_API_KEY", "COMMANDCODE_API_KEY", "COMMAND_API_KEY"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

impl QuotaProvider for CommandCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::CommandCode
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        const ID: ProviderId = ProviderId::CommandCode;
        let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            return ProviderSnapshot::error(
                ID,
                "no-key",
                "未配置 Command Code API Key（quota-bar setkey command-code <key> 或环境变量 COMMAND_CODE_API_KEY）",
            );
        };
        let auth = format!("Bearer {key}");
        let req = http
            .get(URL)
            .set("Authorization", &auth)
            .set("Accept", "application/json");
        match finish(req.call()) {
            Ok((status, body)) => snapshot_from_body(ID, status, &body),
            Err((code, message)) => ProviderSnapshot::error(ID, &code, message),
        }
    }
}

/// 响应体 → 快照（测试直接喂 fixture，不碰网络）。
pub(crate) fn snapshot_from_body(id: ProviderId, status: u16, body: &str) -> ProviderSnapshot {
    match interpret(status, body) {
        Ok(value) => {
            let (windows, level) = norm_mimo(&value);
            let mut snap = ProviderSnapshot::new(id);
            snap.rolling = windows.rolling;
            snap.weekly = windows.weekly;
            snap.monthly = windows.monthly;
            snap.level = level;
            snap
        }
        Err((code, message)) => ProviderSnapshot::error(id, &code, message),
    }
}

fn finish(resp: Result<ureq::Response, ureq::Error>) -> Result<(u16, String), (String, String)> {
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

// ---------------------------------------------------------------------------
// normMimo 移植（dsh `src/plans/mimo.ts`）：三窗口 + 档位，first-wins。
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Windows {
    rolling: Option<UsageWindow>,
    weekly: Option<UsageWindow>,
    monthly: Option<UsageWindow>,
}

impl Windows {
    fn get(&self, slot: WindowSlot) -> &Option<UsageWindow> {
        match slot {
            WindowSlot::Rolling => &self.rolling,
            WindowSlot::Weekly => &self.weekly,
            WindowSlot::Monthly => &self.monthly,
        }
    }

    fn is_empty_slot(&self, slot: WindowSlot) -> bool {
        self.get(slot).is_none()
    }

    /// first-wins 写入（normMimo 所有分支都是 `windows[s] == null` 才写）。
    fn set(&mut self, slot: WindowSlot, window: UsageWindow) {
        let target = match slot {
            WindowSlot::Rolling => &mut self.rolling,
            WindowSlot::Weekly => &mut self.weekly,
            WindowSlot::Monthly => &mut self.monthly,
        };
        if target.is_none() {
            *target = Some(window);
        }
    }

    fn is_empty(&self) -> bool {
        self.rolling.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }
}

/// `normMimo(payload)` 主流程（分支顺序与源码一一对应）。
fn norm_mimo(value: &Value) -> (Windows, Option<String>) {
    let mut w = Windows::default();
    // const obj = asRec(payload) ?? {}; const inner = asRec(obj.data) ?? obj;
    let obj = value.as_object();
    let inner = obj
        .and_then(|o| o.get("data"))
        .and_then(|d| d.as_object())
        .or(obj);
    let level = extract_level(obj, inner);

    // ---- limits[] ----
    let limits = inner
        .and_then(|i| i.get("limits"))
        .and_then(Value::as_array)
        .or_else(|| obj.and_then(|o| o.get("limits")).and_then(Value::as_array));
    if let Some(limits) = limits {
        let now_ms = crate::model::now_sec() as f64 * 1000.0;
        for raw in limits {
            let Some(it) = raw.as_object() else {
                continue;
            };
            // A) detail 窗（window.duration/timeUnit 定槽）
            if let Some(detail) = it.get("detail").filter(|d| d.is_object()) {
                let win_map = it.get("window").and_then(Value::as_object);
                let dur = win_map.and_then(|m| to_num(m.get("duration")));
                let unit: &str = win_map
                    .map(|m| {
                        m.get("timeUnit")
                            .and_then(Value::as_str)
                            .or_else(|| m.get("unit").and_then(Value::as_str))
                            .unwrap_or("")
                    })
                    .unwrap_or("");
                let Some(dw) = win_from_detail(detail) else {
                    continue;
                };
                let slot = if dur == Some(300.0) && unit.contains("MINUTE") {
                    Some(WindowSlot::Rolling)
                } else if dur == Some(7.0) && unit.to_lowercase().contains("day") {
                    Some(WindowSlot::Weekly)
                } else if dur == Some(30.0) && unit.to_lowercase().contains("day") {
                    Some(WindowSlot::Monthly)
                } else if dur == Some(1.0) && unit.contains("WEEK") {
                    Some(WindowSlot::Weekly)
                } else if dur == Some(1.0) && unit.contains("MONTH") {
                    Some(WindowSlot::Monthly)
                } else {
                    None
                };
                if let Some(slot) = slot {
                    w.set(slot, dw);
                }
                continue;
            }
            // B) TOKENS_LIMIT / TOKEN_LIMIT / QUOTA
            let typ = it.get("type").and_then(Value::as_str);
            if matches!(typ, Some("TOKENS_LIMIT" | "TOKEN_LIMIT" | "QUOTA")) {
                let pct = it
                    .get("percentage")
                    .and_then(Value::as_f64)
                    .or_else(|| to_num(pick(it, &["percent", "percentage", "usedPercent"])));
                let Some(pct) = pct else {
                    continue;
                };
                let ra = parse_resets_at(pick(it, &["resetsAt", "nextResetTime"]));
                let nrt = it.get("nextResetTime").and_then(Value::as_f64);
                let ris = match nrt {
                    Some(t) if t > 0.0 => Some(((t - now_ms) / 1000.0).max(0.0)),
                    _ => parse_reset_in_sec(pick(it, &["resetInSec"]), ra),
                };
                let window = UsageWindow {
                    percent: Some(pct.clamp(0.0, 100.0).round()),
                    resets_at: ra,
                    reset_in_sec: ris,
                    status: it
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                };
                if let Some(slot) = typed_slot(it) {
                    w.set(slot, window);
                }
                continue;
            }
            // C) TIME_LIMIT → 月窗
            if typ == Some("TIME_LIMIT") {
                let pct = it
                    .get("percentage")
                    .and_then(Value::as_f64)
                    .or_else(|| it.get("percent").and_then(Value::as_f64));
                let Some(pct) = pct else {
                    continue;
                };
                let ra = parse_resets_at(pick(it, &["resetsAt", "nextResetTime"]));
                let ris = it.get("nextResetTime").and_then(Value::as_f64).map(|t| {
                    ((t - now_ms) / 1000.0).max(0.0)
                });
                w.set(
                    WindowSlot::Monthly,
                    UsageWindow {
                        percent: Some(pct.clamp(0.0, 100.0).round()),
                        resets_at: ra,
                        reset_in_sec: ris,
                        status: None,
                    },
                );
                continue;
            }
            // D) 通用条目：winFromDetail(it) + hint 定槽，否则补最前面的空槽
            if let Some(gw) = win_from_detail(raw) {
                let hint: String = if let Some(s) = it.get("window").and_then(Value::as_str) {
                    s.to_lowercase()
                } else if let Some(s) = it.get("period").and_then(Value::as_str) {
                    s.to_lowercase()
                } else if let Some(s) = it.get("kind").and_then(Value::as_str) {
                    s.to_lowercase()
                } else {
                    String::new()
                };
                let hinted = if hint.contains("5h")
                    || hint.contains("rolling")
                    || hint.contains("session")
                {
                    Some(WindowSlot::Rolling)
                } else if hint.contains("week") {
                    Some(WindowSlot::Weekly)
                } else if hint.contains("month") {
                    Some(WindowSlot::Monthly)
                } else {
                    None
                };
                let slot = match hinted {
                    Some(s) => Some(s),
                    None if w.is_empty_slot(WindowSlot::Rolling) => Some(WindowSlot::Rolling),
                    None if w.is_empty_slot(WindowSlot::Weekly) => Some(WindowSlot::Weekly),
                    None if w.is_empty_slot(WindowSlot::Monthly) => Some(WindowSlot::Monthly),
                    None => None,
                };
                if let Some(slot) = slot {
                    w.set(slot, gw);
                }
            }
        }
    }

    // ---- direct 字段：rolling/weekly/monthly(Usage) ----
    for (slot, keys) in [
        (WindowSlot::Rolling, &["rolling", "rollingUsage"][..]),
        (WindowSlot::Weekly, &["weekly", "weeklyUsage"][..]),
        (WindowSlot::Monthly, &["monthly", "monthlyUsage"][..]),
    ] {
        if !w.is_empty_slot(slot) {
            continue;
        }
        let Some(v) = pick_two(inner, obj, keys) else {
            continue;
        };
        if let Some(win) = win_from_direct(v) {
            w.set(slot, win);
        }
    }

    // ---- usage 字段（无 limits 时当月窗，有 limits 时当周窗；子字段补槽） ----
    let usage_val = inner
        .and_then(|i| i.get("usage"))
        .filter(|u| u.is_object())
        .or_else(|| obj.and_then(|o| o.get("usage")).filter(|u| u.is_object()));
    if let Some(usage_v) = usage_val {
        let uf = usage_v.as_object().expect("filtered to object");
        if w.is_empty_slot(WindowSlot::Weekly) {
            if let Some(win) = win_from_detail(usage_v) {
                if has_nn(inner, "limits") || has_nn(obj, "limits") {
                    w.set(WindowSlot::Weekly, win);
                } else if w.is_empty_slot(WindowSlot::Monthly) && w.is_empty_slot(WindowSlot::Rolling)
                {
                    w.set(WindowSlot::Monthly, win);
                }
            }
        }
        for (key, slot) in [
            ("rolling", WindowSlot::Rolling),
            ("weekly", WindowSlot::Weekly),
            ("monthly", WindowSlot::Monthly),
        ] {
            if !w.is_empty_slot(slot) {
                continue;
            }
            if let Some(v) = uf.get(key).filter(|v| !v.is_null()) {
                if let Some(win) = win_from_direct(v) {
                    w.set(slot, win);
                }
            }
        }
    }

    // ---- quotas/quota/balances/credits/credit/usages 容器（对象 + 数组两种形态） ----
    for key in [
        "quotas", "quota", "balances", "credits", "credit", "usages",
    ] {
        let cont_val = inner
            .and_then(|i| i.get(key))
            .filter(|c| c.is_object())
            .or_else(|| obj.and_then(|o| o.get(key)).filter(|c| c.is_object()));
        if let Some(cont_v) = cont_val {
            let cont = cont_v.as_object().expect("filtered to object");
            for (k, slot) in [
                ("rolling", WindowSlot::Rolling),
                ("5h", WindowSlot::Rolling),
                ("session", WindowSlot::Rolling),
                ("weekly", WindowSlot::Weekly),
                ("monthly", WindowSlot::Monthly),
                ("total", WindowSlot::Monthly),
            ] {
                if !w.is_empty_slot(slot) {
                    continue;
                }
                if let Some(v) = cont.get(k).filter(|v| !v.is_null()) {
                    if let Some(win) = win_from_direct(v) {
                        w.set(slot, win);
                    }
                }
            }
            if w.is_empty_slot(WindowSlot::Monthly) && (has_nn(Some(cont), "limit") || has_nn(Some(cont), "used"))
            {
                if let Some(win) = win_from_detail(cont_v) {
                    w.set(WindowSlot::Monthly, win);
                }
            }
        }
        let arr = inner
            .and_then(|i| i.get(key))
            .and_then(Value::as_array)
            .or_else(|| obj.and_then(|o| o.get(key)).and_then(Value::as_array));
        if let Some(arr) = arr {
            for itv in arr {
                let Some(r) = itv.as_object() else {
                    continue;
                };
                let src = r.get("detail").filter(|d| !d.is_null()).unwrap_or(itv);
                let Some(win) = win_from_detail(src) else {
                    continue;
                };
                let hint: String = if let Some(s) = r.get("window").and_then(Value::as_str) {
                    s.to_lowercase()
                } else if let Some(s) = r.get("period").and_then(Value::as_str) {
                    s.to_lowercase()
                } else if let Some(s) = r.get("kind").and_then(Value::as_str) {
                    s.to_lowercase()
                } else {
                    String::new()
                };
                let slot = if hint.contains("5h")
                    || hint.contains("rolling")
                    || hint.contains("hour")
                {
                    Some(WindowSlot::Rolling)
                } else if hint.contains("week") {
                    Some(WindowSlot::Weekly)
                } else if hint.contains("month") {
                    Some(WindowSlot::Monthly)
                } else {
                    None
                };
                if let Some(slot) = slot {
                    w.set(slot, win);
                }
            }
        }
    }

    // ---- windows[]（kind 定槽，无兜底） ----
    let warr = inner
        .and_then(|i| i.get("windows"))
        .and_then(Value::as_array)
        .or_else(|| obj.and_then(|o| o.get("windows")).and_then(Value::as_array));
    if let Some(warr) = warr {
        for raw in warr {
            let Some(r) = raw.as_object() else {
                continue;
            };
            let Some(p) = to_num(pick(r, &["usedPercent", "percent", "percentage"])) else {
                continue;
            };
            let ra = parse_resets_at(pick(r, &["resetsAt", "resetAt"]));
            let ris = parse_reset_in_sec(r.get("resetInSec"), ra);
            let window = UsageWindow {
                percent: Some(p.clamp(0.0, 100.0).round()),
                resets_at: ra,
                reset_in_sec: ris,
                status: r.get("status").and_then(Value::as_str).map(str::to_string),
            };
            let slot = match r.get("kind").and_then(Value::as_str) {
                Some("session") => Some(WindowSlot::Rolling),
                Some("weekly") => Some(WindowSlot::Weekly),
                Some("other") => Some(WindowSlot::Monthly),
                _ => None,
            };
            if let Some(slot) = slot {
                w.set(slot, window);
            }
        }
    }

    // ---- config.creditUsagePercent → 周窗 ----
    let cfg = inner
        .and_then(|i| i.get("config"))
        .and_then(Value::as_object)
        .or_else(|| obj.and_then(|o| o.get("config")).and_then(Value::as_object));
    if let Some(cfg) = cfg {
        let cp = to_num(pick(cfg, &["creditUsagePercent", "usagePercent"]));
        let bv = parse_resets_at(
            cfg.get("billingPeriodEnd")
                .filter(|v| !v.is_null())
                .or_else(|| {
                    cfg.get("currentPeriod")
                        .and_then(Value::as_object)
                        .and_then(|cp| cp.get("end"))
                        .filter(|v| !v.is_null())
                }),
        );
        if w.is_empty_slot(WindowSlot::Weekly) {
            if let Some(cp) = cp {
                w.set(
                    WindowSlot::Weekly,
                    UsageWindow {
                        percent: Some(cp.clamp(0.0, 100.0).round()),
                        resets_at: bv,
                        reset_in_sec: parse_reset_in_sec(None, bv),
                        status: None,
                    },
                );
            }
        }
    }

    // ---- 全空兜底：inner.limit/used → 月窗；否则 obj.limit → 月窗 ----
    if w.is_empty() {
        if let Some(inner_map) = inner {
            if let Some(d) = win_from_detail_map(inner_map) {
                if has_nn(Some(inner_map), "limit") || has_nn(Some(inner_map), "used") {
                    w.set(WindowSlot::Monthly, d);
                }
            }
        }
        if w.is_empty() {
            if let Some(obj_map) = obj {
                if has_nn(Some(obj_map), "limit") {
                    if let Some(d) = win_from_detail_map(obj_map) {
                        w.set(WindowSlot::Monthly, d);
                    }
                }
            }
        }
    }

    // ---- rate_limit：primary → 5h，secondary → 周 ----
    let rl = inner
        .and_then(|i| i.get("rate_limit"))
        .filter(|v| v.is_object())
        .or_else(|| obj.and_then(|o| o.get("rate_limit")).filter(|v| v.is_object()))
        .or_else(|| inner.and_then(|i| i.get("rateLimit")).filter(|v| v.is_object()))
        .or_else(|| obj.and_then(|o| o.get("rateLimit")).filter(|v| v.is_object()));
    if let Some(rl_v) = rl {
        let rl = rl_v.as_object().expect("filtered to object");
        // `primary_window ?? primaryWindow`：别名取第一个对象，不再回退第二个别名。
        let prim = rl
            .get("primary_window")
            .filter(|v| v.is_object())
            .or_else(|| rl.get("primaryWindow").filter(|v| v.is_object()));
        let sec = rl
            .get("secondary_window")
            .filter(|v| v.is_object())
            .or_else(|| rl.get("secondaryWindow").filter(|v| v.is_object()));
        for (entry, slot) in [(prim, WindowSlot::Rolling), (sec, WindowSlot::Weekly)] {
            if !w.is_empty_slot(slot) {
                continue;
            }
            let Some(entry) = entry else {
                continue;
            };
            let entry = entry.as_object().expect("filtered to object");
            let Some(p) = to_num(pick(entry, &["used_percent", "usedPercent"])) else {
                continue;
            };
            let ra = parse_resets_at(pick(entry, &["reset_at", "resetsAt"]));
            w.set(
                slot,
                UsageWindow {
                    percent: Some(p.round()),
                    resets_at: ra,
                    reset_in_sec: parse_reset_in_sec(None, ra),
                    status: None,
                },
            );
        }
    }

    (w, level)
}

/// 类型化条目的定槽：unit/number → window 字符串 → limitWindowSeconds（else-if 链）。
fn typed_slot(it: &Map<String, Value>) -> Option<WindowSlot> {
    let unit = it.get("unit").and_then(Value::as_f64);
    let number = it.get("number").and_then(Value::as_f64);
    if unit == Some(3.0) && number == Some(5.0) {
        return Some(WindowSlot::Rolling);
    }
    if unit == Some(6.0) && number == Some(1.0) {
        return Some(WindowSlot::Weekly);
    }
    if unit == Some(5.0) && number == Some(1.0) {
        return Some(WindowSlot::Monthly);
    }
    if let Some(s) = it.get("window").and_then(Value::as_str) {
        // window 是字符串但没命中 hint → slot 保持 null（不再看 limitWindowSeconds，同源码 else-if 链）。
        let s = s.to_lowercase();
        if s.contains("5h") || s.contains("rolling") {
            return Some(WindowSlot::Rolling);
        }
        if s.contains("week") {
            return Some(WindowSlot::Weekly);
        }
        if s.contains("month") {
            return Some(WindowSlot::Monthly);
        }
        return None;
    }
    if let Some(lws) = it.get("limitWindowSeconds").and_then(Value::as_f64) {
        if lws == 18_000.0 {
            return Some(WindowSlot::Rolling);
        }
        if lws == 604_800.0 {
            return Some(WindowSlot::Weekly);
        }
        if lws == 2_592_000.0 {
            return Some(WindowSlot::Monthly);
        }
    }
    None
}

/// `extractLevel`：inn.level/plan/tier/subscriptionTier/sub.{level,tier} → pay.{level,plan,tier} → inn.data 递归。
fn extract_level(
    obj: Option<&Map<String, Value>>,
    inn: Option<&Map<String, Value>>,
) -> Option<String> {
    fn s(m: &Map<String, Value>, k: &str) -> Option<String> {
        m.get(k)
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    }
    let empty = Map::new();
    let inn = inn.unwrap_or(&empty);
    let sub = inn.get("subscription").and_then(Value::as_object);
    for cand in [
        s(inn, "level"),
        s(inn, "plan"),
        s(inn, "tier"),
        s(inn, "subscriptionTier"),
        sub.and_then(|m| s(m, "level")),
        sub.and_then(|m| s(m, "tier")),
        obj.and_then(|m| s(m, "level")),
        obj.and_then(|m| s(m, "plan")),
        obj.and_then(|m| s(m, "tier")),
    ] {
        if let Some(c) = cand {
            return Some(c);
        }
    }
    if let Some(ds) = inn.get("data").and_then(Value::as_object) {
        return extract_level(obj, Some(ds));
    }
    None
}

/// `winFromDetail`（对象形态）。
fn win_from_detail(v: &Value) -> Option<UsageWindow> {
    v.as_object().and_then(win_from_detail_map)
}

fn win_from_detail_map(r: &Map<String, Value>) -> Option<UsageWindow> {
    let limit = to_num(pick(r, &["limit", "quota", "total", "max"]));
    let used = to_num(pick(r, &["used", "used_quota", "consumed", "current"]));
    let rem = to_num(pick(r, &[
        "remaining", "remain", "available", "left",
    ]));
    let pr = to_num(pick(r, &[
        "percent", "percentage", "usagePercent", "usedPercent", "used_percent",
    ]));
    let ratio = to_num(pick(r, &["ratio", "amountUsedRatio", "kimiCodeUsedRatio"]));
    let mut pct = pct_from(limit, used, rem, pr, ratio);
    if pct.is_none() {
        if let Some(u) = to_num(r.get("utilization")) {
            pct = Some(u.clamp(0.0, 100.0).round());
        }
    }
    let pct = pct?;
    let ra = parse_resets_at(pick(
        r,
        &[
            "resetsAt",
            "reset_at",
            "resetTime",
            "nextResetTime",
            "expireTime",
            "expiresAt",
            "billingPeriodEnd",
            "end",
            "reset",
        ],
    ));
    let ris = parse_reset_in_sec(
        pick(r, &["resetInSec", "reset_in_sec", "resetAfterSeconds", "nextResetTime"]),
        ra,
    );
    Some(UsageWindow {
        percent: Some(pct),
        resets_at: ra,
        reset_in_sec: ris,
        status: r.get("status").and_then(Value::as_str).map(str::to_string),
    })
}

/// `winFromDirect`：`norm(w)`（percent/usagePercent 直读）否则退回 winFromDetail。
fn win_from_direct(w: &Value) -> Option<UsageWindow> {
    if let Some(o) = w.as_object() {
        let pct = o
            .get("percent")
            .and_then(Value::as_f64)
            .or_else(|| o.get("usagePercent").and_then(Value::as_f64));
        if let Some(p) = pct {
            let ra = parse_resets_at(o.get("resetsAt"));
            // 源码 norm() 不做 resetInSec↔resetsAt 交叉推导：只认显式数字字段。
            let ris = o
                .get("resetInSec")
                .filter(|v| v.is_number())
                .and_then(|v| parse_reset_in_sec(Some(v), ra));
            return Some(UsageWindow {
                percent: Some(p),
                resets_at: ra,
                reset_in_sec: ris,
                status: o.get("status").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    win_from_detail(w)
}

/// `pctFrom`：显式百分比 → ratio → limit/used(remaining) 推导；缺失一律 None。
fn pct_from(
    limit: Option<f64>,
    used: Option<f64>,
    rem: Option<f64>,
    pct: Option<f64>,
    ratio: Option<f64>,
) -> Option<f64> {
    if let Some(p) = pct {
        if (0.0..=100.0).contains(&p) {
            return Some(p.round());
        }
        if p > 100.0 {
            return Some(100.0);
        }
        // p < 0 → 落到后续分支（同源码）
    }
    if let Some(r) = ratio {
        let p = if (0.0..=1.0).contains(&r) { r * 100.0 } else { r };
        return Some(p.clamp(0.0, 100.0).round());
    }
    if let Some(lim) = limit.filter(|l| *l > 0.0) {
        let mut u = used;
        if u.is_none() {
            // dsh：used 缺失时用 remaining 推导 `limit - rem`
            if let Some(r) = rem {
                u = Some(lim - r);
            }
        }
        if let Some(u) = u {
            let capped = u.clamp(0.0, lim);
            return Some(((capped / lim) * 100.0).clamp(0.0, 100.0).round());
        }
    }
    None
}

/// `??` 链：第一个存在且非 null 的键。
fn pick<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| {
        map.get(*k)
            .filter(|v| !v.is_null())
    })
}

/// `inner.k ?? obj.k` 链。
fn pick_two<'a>(
    inner: Option<&'a Map<String, Value>>,
    obj: Option<&'a Map<String, Value>>,
    keys: &[&str],
) -> Option<&'a Value> {
    inner
        .and_then(|m| pick(m, keys))
        .or_else(|| obj.and_then(|m| pick(m, keys)))
}

/// `map.k != null`（键存在且非 null）。
fn has_nn(map: Option<&Map<String, Value>>, key: &str) -> bool {
    map.is_some_and(|m| m.get(key).is_some_and(|v| !v.is_null()))
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

/// dsh `parseResetsAt`：ISO 串 / 数字（秒或毫秒）→ unix 秒。
/// 源码对不可解析的字符串会原样带回 wire —— quota-bar 的 `resets_at` 是 i64，存不了垃圾串 → None。
fn parse_resets_at(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::String(s) => {
            if s.is_empty() {
                return None;
            }
            if let Some(e) = iso_to_epoch_sec(s) {
                return Some(e);
            }
            if let Ok(n) = s.trim().parse::<f64>() {
                if n > 1e9 {
                    return Some(if n < 1e12 { n as i64 } else { (n / 1000.0) as i64 });
                }
            }
            None
        }
        Value::Number(n) => {
            let v = n.as_f64()?;
            if v > 0.0 {
                Some(if v < 1e12 { v as i64 } else { (v / 1000.0) as i64 })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// dsh `parseResetInSec`：≤1e9 当秒直读；>1e9 当 epoch 由 resetsAt 推导；
/// 非数字 → 由 resetsAt 推导；都缺 → None。
fn parse_reset_in_sec(v: Option<&Value>, resets_at: Option<i64>) -> Option<f64> {
    let now = crate::model::now_sec() as i64;
    if let Some(raw) = v.and_then(Value::as_f64) {
        if raw >= 0.0 {
            if raw > 1e9 {
                return resets_at.map(|t| (t - now).max(0) as f64);
            }
            return Some(raw);
        }
    }
    resets_at.map(|t| (t - now).max(0) as f64)
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
        Some(
            digit(b, i)? * 1000
                + digit(b, i + 1)? * 100
                + digit(b, i + 2)? * 10
                + digit(b, i + 3)?,
        )
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
    fn ext_command_parses_detail_windows_and_level_from_fixture() {
        let body = fixture("ext_command_usage.json");
        let snap = snapshot_from_body(ProviderId::CommandCode, 200, &body);
        assert!(snap.is_ok());
        // 300-MINUTE detail → 5 小时：40/100 → 40%
        let rolling = snap.rolling.as_ref().expect("5小时窗口");
        assert_eq!(rolling.percent, Some(40.0));
        assert_eq!(rolling.resets_at, Some(1893456000)); // 2030-01-01
        assert!(rolling.reset_in_sec.is_some());
        // 7-DAY detail + remaining 推导：used = 1000-250 = 750 → 75%
        let weekly = snap.weekly.as_ref().expect("周窗口");
        assert_eq!(weekly.percent, Some(75.0));
        assert_eq!(weekly.resets_at, Some(1893801600)); // 2030-01-05
        // 30-DAY detail → 月窗：1000/2000 → 50%
        let monthly = snap.monthly.as_ref().expect("月窗口");
        assert_eq!(monthly.percent, Some(50.0));
        assert_eq!(monthly.resets_at, Some(1895961600)); // 2030-01-30
        assert_eq!(snap.level.as_deref(), Some("pro"));
        assert!(snap.has_data());
    }

    #[test]
    fn ext_command_typed_and_direct_shapes_map_to_slots() {
        // GLM 同款类型化条目 + unit/number 定槽。
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"data":{"limits":[
                {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":45},
                {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":12},
                {"type":"TIME_LIMIT","percentage":63}
            ]}}"#,
        );
        assert_eq!(snap.rolling.as_ref().unwrap().percent, Some(45.0));
        assert_eq!(snap.weekly.as_ref().unwrap().percent, Some(12.0));
        assert_eq!(snap.monthly.as_ref().unwrap().percent, Some(63.0));
        // direct 字段形态（rolling/weekly 对象）。
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"rolling":{"percent":10,"resetsAt":"2030-01-01T00:00:00Z"},"weekly":{"percent":20}}"#,
        );
        assert_eq!(snap.rolling.as_ref().unwrap().percent, Some(10.0));
        assert_eq!(snap.rolling.as_ref().unwrap().resets_at, Some(1893456000));
        assert_eq!(snap.weekly.as_ref().unwrap().percent, Some(20.0));
        assert_eq!(snap.monthly, None);
        // rate_limit primary/secondary 形态。
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"rate_limit":{"primary_window":{"used_percent":55},"secondary_window":{"used_percent":66}}}"#,
        );
        assert_eq!(snap.rolling.as_ref().unwrap().percent, Some(55.0));
        assert_eq!(snap.weekly.as_ref().unwrap().percent, Some(66.0));
    }

    #[test]
    fn ext_command_missing_percent_creates_no_window_not_zero() {
        // 类型化条目缺 percentage → continue，不落任何窗口。
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"data":{"limits":[{"type":"TOKEN_LIMIT"},{"detail":{"used":5}}]}}"#,
        );
        assert!(snap.is_ok());
        assert!(!snap.has_data());
        assert_eq!(snap.rolling, None);
        assert_eq!(snap.weekly, None);
        assert_eq!(snap.monthly, None);
        // 空 limits → 同样全 None 而不是 0。
        let snap = snapshot_from_body(ProviderId::CommandCode, 200, r#"{"data":{"limits":[]}}"#);
        assert!(snap.is_ok());
        assert!(!snap.has_data());
        // 全空兜底分支：inner 顶层 limit/used → 月窗（有数据才填）。
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"data":{"limit":100,"used":40}}"#,
        );
        assert_eq!(snap.monthly.as_ref().unwrap().percent, Some(40.0));
        // 只有 limit、没有 used/remaining → 兜底也造不出百分比。
        let snap =
            snapshot_from_body(ProviderId::CommandCode, 200, r#"{"data":{"limit":100}}"#);
        assert_eq!(snap.monthly, None);
        assert!(!snap.has_data());
    }

    #[test]
    fn ext_command_error_envelope_and_http_status() {
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"error":{"message":"quota exceeded"}}"#,
        );
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert_eq!(snap.message.as_deref(), Some("quota exceeded"));
        // {code != 200, msg} 信封（HTTP 200 也算错，dsh 同款）。
        let snap = snapshot_from_body(
            ProviderId::CommandCode,
            200,
            r#"{"code":1429,"msg":"rate limited"}"#,
        );
        assert_eq!(snap.error.as_deref(), Some("api"));
        assert_eq!(snap.message.as_deref(), Some("rate limited"));
        let snap = snapshot_from_body(ProviderId::CommandCode, 500, "");
        assert_eq!(snap.error.as_deref(), Some("http"));
        assert_eq!(snap.message.as_deref(), Some("upstream HTTP 500"));
        let snap = snapshot_from_body(ProviderId::CommandCode, 200, "<html>");
        assert_eq!(snap.error.as_deref(), Some("parse"));
    }

    #[test]
    fn ext_command_no_key_before_network() {
        let http = crate::providers::make_agent();
        let snap = CommandCodeProvider::new(None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        assert!(snap
            .message
            .as_deref()
            .unwrap()
            .contains("setkey command-code"));
    }
}
