//! 小米 MiMo 套餐额度：平台 Cookie 三接口（`platform.xiaomimimo.com` 的
//! `tokenPlan/usage` / `tokenPlan/detail` / `balance`）。旧 `api.mimo.xiaomi.com`
//! Bearer 接口域名已 NXDOMAIN 下线；凭证 = `quota-bar setcookie mimo <cookie>`
//! （或环境变量 `MIMO_USAGE_COOKIE`）。结构参考 opencode-quota `lib/mimo.ts`（MIT）。
//!
//! 本文件同时承载三家 provider 共用的解析工具（ISO 时间 / 数字 / 错误信封）——
//! `providers/mod.rs` 不在可改范围、无法新增子模块，故挂在首个 provider 模块下，
//! 由 `opencode_go` / `antigravity` 以 `super::mimo::…` 引用。
//!
//! dsh 时代的 `norm_mimo` 全套解析器**保留**（测试持续验证移植正确性，亦备
//! Bearer 接口有朝一日恢复），生产路径已切到 `parse_platform_*`。
#![allow(dead_code)] // 上段：遗留解析簇不构成告警噪音

use serde_json::{Map, Value};

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow, WindowSlot};
use crate::providers::QuotaProvider;

/// 平台余额端点（可选增强；cookie 原样带，失败静默）。
const BALANCE_ENDPOINT: &str = "https://platform.xiaomimimo.com/api/v1/balance";
/// 平台套餐用量 / 套餐详情（opencode-quota `lib/mimo.ts` 同款；Cookie 认证）。
/// 旧 `api.mimo.xiaomi.com/v1/usage`（Bearer）域名已 NXDOMAIN 下线。
const USAGE_ENDPOINT: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/usage";
const DETAIL_ENDPOINT: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/detail";
const PLATFORM_ORIGIN: &str = "https://platform.xiaomimimo.com";
const PLATFORM_REFERER: &str = "https://platform.xiaomimimo.com/";
/// 平台接口要求浏览器形态 UA（opencode-quota 同样带 UA / Origin / Referer）。
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

// ---------------------------------------------------------------------------
// 共享工具：三家 provider 都要用（dsh Date.parse / toNum / curlJson 的子集）。
// ---------------------------------------------------------------------------

/// 当前 unix 毫秒。
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `Math.round` 的 JS 语义（floor(x+0.5)，对负数同样向 +∞ 取整）。
pub(crate) fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// 钳到 0–100（`Math.max(0, Math.min(100, v))`）。
pub(crate) fn clamp_pct(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

/// dsh `toNum`：数字原样、数字串 trim 后解析；不可解析 → None。
/// （额外过滤非有限值：JSON `1e400` 会被 serde 解析成 inf，dsh 靠后续
/// `Number.isFinite` 挡，这里前置挡掉更稳。）
pub(crate) fn to_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok().filter(|f| f.is_finite())
            }
        }
        _ => None,
    }
}

/// JS `a ?? b ?? c`：取第一个**存在且非 null** 的值（undefined 语义 = 缺失）。
pub(crate) fn pick<'a>(m: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| m.get(*k).filter(|v| !v.is_null()))
}

/// `pick` + `toNum`。
pub(crate) fn pick_num(m: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    pick(m, keys).and_then(to_num)
}

/// 跨多个对象取键（map 外层、keys 内层，与 dsh `inner.x ?? inner.y ?? obj.x ?? obj.y` 同序）。
pub(crate) fn pick_any<'a>(
    maps: &[&'a Map<String, Value>],
    keys: &[&str],
) -> Option<&'a Value> {
    maps.iter().find_map(|m| pick(m, keys))
}

/// 距目标时刻的秒数（恒 ≥0；目标已过 → 0）。`target` 为 unix 秒，`now` 为毫秒。
pub(crate) fn secs_until(target_sec: i64, now_ms: u64) -> f64 {
    let delta_ms = (target_sec as i128) * 1000 - now_ms as i128;
    (delta_ms as f64 / 1000.0).max(0.0)
}

/// ISO-8601 → unix 秒（dsh `Date.parse` 的常用子集，无 chrono 依赖）。
///
/// 支持 `2026-09-24T00:00:00Z` / 毫秒 / `+08:00` `+0800` 偏移 / 空格分隔 /
/// `HH:MM`（无秒）/ 仅日期。无时区标记按 **UTC**（JS 按本地时区——差异仅
/// 出现在上游不带 Z 的串；dsh fixture 与线上响应都带 Z）。
pub(crate) fn parse_iso8601(input: &str) -> Option<i64> {
    fn digits(s: &[u8], from: usize, count: usize) -> Option<i64> {
        if from + count > s.len() {
            return None;
        }
        let mut v = 0i64;
        for &c in &s[from..from + count] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    }

    let s = input.trim();
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(b, 0, 4)?;
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || day > days_in_month(year, month) {
        return None;
    }

    let mut pos = 10usize;
    let (mut hh, mut mm, mut ss) = (0i64, 0i64, 0i64);
    let mut offset_min = 0i64;
    if pos < b.len() {
        let sep = b[pos];
        if sep == b'T' || sep == b't' || sep == b' ' {
            pos += 1;
            if pos + 5 > b.len() {
                return None;
            }
            hh = digits(b, pos, 2)?;
            if b.get(pos + 2) != Some(&b':') {
                return None;
            }
            mm = digits(b, pos + 3, 2)?;
            if hh > 23 || mm > 59 {
                return None;
            }
            pos += 5;
            if b.get(pos) == Some(&b':') {
                ss = digits(b, pos + 1, 2)?;
                if ss > 60 {
                    return None;
                }
                pos += 3;
                // 小数秒：丢弃（我们的精度是整秒）。
                if b.get(pos) == Some(&b'.') {
                    pos += 1;
                    while b.get(pos).is_some_and(|c| c.is_ascii_digit()) {
                        pos += 1;
                    }
                }
            }
            // 时区
            match b.get(pos) {
                None => {} // 缺省按 UTC（见 doc）
                Some(&c) if c == b'Z' || c == b'z' => {
                    pos += 1;
                }
                Some(&c) if c == b'+' || c == b'-' => {
                    let sign = if c == b'-' { -1 } else { 1 };
                    pos += 1;
                    let oh = digits(b, pos, 2)?;
                    pos += 2;
                    if b.get(pos) == Some(&b':') {
                        pos += 1;
                    }
                    let om = digits(b, pos, 2)?;
                    pos += 2;
                    if oh > 23 || om > 59 {
                        return None;
                    }
                    offset_min = sign * (oh * 60 + om);
                }
                Some(_) => return None, // 尾随垃圾：dsh Date.parse 同样拒绝
            }
        } else {
            return None; // 日期后接非法分隔
        }
    }
    if pos != b.len() {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hh * 3600 + mm * 60 + ss - offset_min * 60)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// Howard Hinnant 的 days-from-civil 算法（proleptic Gregorian）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// dsh `curlJson` 的上游错误信封识别（`plans/util.ts` :93-115）：
/// - `{type:'error', error:{type,message}}` / `{error:{…}}`
/// - `{code != 200, msg}`
/// 命中 → Some((错误码, 中文可展示消息))。
pub(crate) fn upstream_envelope_error(v: &Value) -> Option<(String, String)> {
    let obj = v.as_object()?;
    let type_is_error = obj.get("type").and_then(Value::as_str) == Some("error");
    let err_field = obj.get("error").filter(|e| !e.is_null());
    let err_is_object = err_field.is_some_and(|e| e.is_object());
    if type_is_error || err_is_object {
        if let Some(e) = err_field {
            if let Some(e) = e.as_object() {
                let code = e
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("api")
                    .to_string();
                let message = e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("API error")
                    .to_string();
                return Some((code, message));
            }
        }
    }
    // typeof obj.code === 'number' && obj.code !== 200
    if let Some(code) = obj.get("code").and_then(Value::as_f64) {
        if code != 200.0 {
            let message = obj
                .get("msg")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("API error (code {code})"));
            return Some(("api".into(), message));
        }
    }
    None
}

/// 非 200 且无信封时的 HTTP 错误（dsh `upstream HTTP {status}`）。
pub(crate) fn http_status_error(status: u16) -> (String, String) {
    ("http".into(), format!("upstream HTTP {status}"))
}

// ---------------------------------------------------------------------------
// dsh mimo.ts 的窗口辅助函数（parseResetsAt / parseResetInSec / pctFrom / …）
// ---------------------------------------------------------------------------

/// dsh `parseResetsAt`：ISO 串 / 数字时间戳 → unix 秒。
fn parse_resets_at(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => {
            if s.is_empty() {
                return None;
            }
            if let Some(sec) = parse_iso8601(s) {
                return Some(sec);
            }
            // `Number(v)`：纯数字串且 >1e9 → 当时间戳（秒或毫秒）。
            let n = s.trim().parse::<f64>().unwrap_or(f64::NAN);
            if n.is_finite() && n > 1e9 {
                let ms = if n < 1e12 { n * 1000.0 } else { n };
                return Some((ms / 1000.0) as i64);
            }
            None // dsh 返回原串（后续 Date.parse 失败 → 无有效时间）
        }
        Value::Number(num) => {
            let f = num.as_f64()?;
            if f > 0.0 {
                let ms = if f < 1e12 { f * 1000.0 } else { f };
                Some((ms / 1000.0) as i64)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// dsh `parseResetInSec`：倒计时数字直接用；是绝对时间戳（>1e9）或缺失时由
/// resetsAt 推导（`max(0, (t - now)/1000)`）。`raw` 必须是上游原生 number。
fn parse_reset_in_sec(raw: Option<f64>, resets_at: Option<i64>, now: u64) -> Option<f64> {
    if let Some(v) = raw {
        if v.is_finite() && v >= 0.0 {
            if v > 1e9 {
                // 是绝对时间戳而非倒计时 → 只能由 resetsAt 推。
                return resets_at.map(|t| secs_until(t, now));
            }
            return Some(v);
        }
    }
    resets_at.map(|t| secs_until(t, now))
}

/// dsh `pctFrom`：percent → ratio → limit/used/remaining 逐级兜底算已用百分比。
fn pct_from(
    limit: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
    pct: Option<f64>,
    ratio: Option<f64>,
) -> Option<f64> {
    if let Some(p) = pct {
        if (0.0..=100.0).contains(&p) {
            return Some(js_round(p));
        }
        if p > 100.0 {
            return Some(100.0);
        }
        // p < 0 → 落到 ratio 分支（dsh 同）
    }
    if let Some(rt) = ratio {
        let p = if (0.0..=1.0).contains(&rt) { rt * 100.0 } else { rt };
        return Some(js_round(clamp_pct(p)));
    }
    if let Some(l) = limit {
        if l > 0.0 {
            let u = match used {
                Some(u) => Some(u),
                None => remaining.map(|r| l - r),
            };
            if let Some(u) = u {
                let capped = u.clamp(0.0, l) / l * 100.0;
                return Some(js_round(clamp_pct(capped)));
            }
        }
    }
    None
}

/// dsh `norm()`：在线形态（percent / usagePercent / status / resetsAt / resetInSec）。
pub(crate) fn norm_wire(v: &Value) -> Option<UsageWindow> {
    let w = v.as_object()?; // typeof !== 'object' → null（数组在此已被 serde 排除，见 win_from_direct 注）
    let percent = pick_num(w, &["percent", "usagePercent"]);
    let status = w
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let resets_at = w
        .get("resetsAt")
        .and_then(Value::as_str)
        .and_then(parse_iso8601); // 只认字符串（dsh typeof === 'string'）
    let reset_in_sec = w.get("resetInSec").and_then(Value::as_f64);
    Some(UsageWindow {
        percent,
        resets_at,
        reset_in_sec,
        status,
    })
}

/// dsh `winFromDetail`：从 limit/used/remaining/percent/ratio/utilization 兜底算窗口。
fn win_from_detail(v: &Value) -> Option<UsageWindow> {
    let r = v.as_object()?;
    let limit = pick_num(r, &["limit", "quota", "total", "max"]);
    let used = pick_num(r, &["used", "used_quota", "consumed", "current"]);
    let remaining = pick_num(r, &["remaining", "remain", "available", "left"]);
    let pr = pick_num(r, &["percent", "percentage", "usagePercent", "usedPercent", "used_percent"]);
    let ratio = pick_num(r, &["ratio", "amountUsedRatio", "kimiCodeUsedRatio"]);
    let mut pct = pct_from(limit, used, remaining, pr, ratio);
    if pct.is_none() {
        if let Some(u) = r.get("utilization").and_then(to_num) {
            pct = Some(js_round(clamp_pct(u)));
        }
    }
    let pct = pct?;
    let ra = pick(
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
    )
    .and_then(parse_resets_at);
    let ris_raw = pick(
        r,
        &["resetInSec", "reset_in_sec", "resetAfterSeconds", "nextResetTime"],
    )
    .and_then(Value::as_f64);
    Some(UsageWindow {
        status: r
            .get("status")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        percent: Some(pct),
        resets_at: ra,
        reset_in_sec: parse_reset_in_sec(ris_raw, ra, now_ms()),
    })
}

/// dsh `winFromDirect`：norm(percent 是数字) 优先，否则 winFromDetail。
fn win_from_direct(v: &Value) -> Option<UsageWindow> {
    v.as_object()?; // asRec 检查（数组 → null）
    if let Some(n) = norm_wire(v) {
        if n.percent.is_some() {
            return Some(n);
        }
    }
    win_from_detail(v)
}

/// dsh direct 段的 `norm(dr) ?? winFromDetail(dr)`：norm 命中对象即用
/// （percent 可为 None —— 保留"有窗口对象但百分比未知"语义）。
fn norm_or_detail(v: &Value) -> Option<UsageWindow> {
    if v.is_object() {
        return norm_wire(v);
    }
    win_from_detail(v)
}

/// dsh `extractLevel`：订阅档位候选逐个取第一个非空字符串，data 再递归。
fn extract_level(pay: &Map<String, Value>, inn: &Map<String, Value>) -> Option<String> {
    let sub = inn.get("subscription").and_then(|v| v.as_object());
    let cand: [Option<&Value>; 9] = [
        inn.get("level"),
        inn.get("plan"),
        inn.get("tier"),
        inn.get("subscriptionTier"),
        sub.and_then(|s| s.get("level")),
        sub.and_then(|s| s.get("tier")),
        pay.get("level"),
        pay.get("plan"),
        pay.get("tier"),
    ];
    for c in cand {
        if let Some(s) = c.and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if let Some(ds) = inn.get("data").and_then(|v| v.as_object()) {
        return extract_level(pay, ds);
    }
    None
}

// ---------------------------------------------------------------------------
// norm_mimo 主体
// ---------------------------------------------------------------------------

/// 三窗口暂存：`set_if_empty` 实现 dsh "首个占优"（`windows[slot] == null` 才写）。
#[derive(Default)]
struct Wins {
    rolling: Option<UsageWindow>,
    weekly: Option<UsageWindow>,
    monthly: Option<UsageWindow>,
}

impl Wins {
    fn get(&self, slot: WindowSlot) -> &Option<UsageWindow> {
        match slot {
            WindowSlot::Rolling => &self.rolling,
            WindowSlot::Weekly => &self.weekly,
            WindowSlot::Monthly => &self.monthly,
        }
    }

    fn set(&mut self, slot: WindowSlot, w: UsageWindow) {
        match slot {
            WindowSlot::Rolling => self.rolling = Some(w),
            WindowSlot::Weekly => self.weekly = Some(w),
            WindowSlot::Monthly => self.monthly = Some(w),
        }
    }

    fn set_if_empty(&mut self, slot: WindowSlot, w: UsageWindow) {
        if self.get(slot).is_none() {
            self.set(slot, w);
        }
    }

    fn first_empty(&self) -> Option<WindowSlot> {
        WindowSlot::ALL
            .into_iter()
            .find(|s| self.get(*s).is_none())
    }

    fn all_empty(&self) -> bool {
        self.rolling.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }
}

/// 从 `it.window` 字符串提示判槽（dsh mimo.ts :17 的 else-if 链）。
fn slot_from_window_string(s: &str) -> Option<WindowSlot> {
    let sl = s.to_lowercase();
    if sl.contains("5h") || sl.contains("rolling") {
        Some(WindowSlot::Rolling)
    } else if sl.contains("week") {
        Some(WindowSlot::Weekly)
    } else if sl.contains("month") {
        Some(WindowSlot::Monthly)
    } else {
        None
    }
}

/// 通用条目 hint（window / period / kind，dsh :18）。
fn slot_from_hint(obj: &Map<String, Value>) -> Option<WindowSlot> {
    let hint = pick(obj, &["window", "period", "kind"])
        .and_then(Value::as_str)
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    if hint.contains("5h") || hint.contains("rolling") || hint.contains("session") {
        Some(WindowSlot::Rolling)
    } else if hint.contains("week") {
        Some(WindowSlot::Weekly)
    } else if hint.contains("month") {
        Some(WindowSlot::Monthly)
    } else {
        None
    }
}

/// normMimo 移植：把任意 MiMo usage 响应归一成三窗口 + 档位。
///
/// 分支与 dsh `src/plans/mimo.ts` `normMimo` 逐条对齐（detail/duration 分槽、
/// TOKENS_LIMIT unit-number、limitWindowSeconds、direct 三窗口、usage/containers/
/// windows 数组、config.creditUsagePercent、兜底、rate_limit primary/secondary）；
/// 全程容错 —— 认不出的形状只留 None，绝不猜值。
pub fn norm_mimo(payload: &Value) -> (Option<UsageWindow>, Option<UsageWindow>, Option<UsageWindow>, Option<String>) {
    let empty_map = Map::new();
    let obj = payload.as_object().unwrap_or(&empty_map);
    let inner = obj.get("data").and_then(|v| v.as_object()).unwrap_or(obj);
    let now = now_ms();

    let mut w = Wins::default();
    let level = extract_level(obj, inner);

    // ---- limits 数组（dsh :16-18）----
    let limits = inner
        .get("limits")
        .and_then(Value::as_array)
        .or_else(|| obj.get("limits").and_then(Value::as_array));
    if let Some(limits) = limits {
        for raw in limits {
            let Some(it) = raw.as_object() else { continue };

            // 1) detail + window.duration/timeUnit 分槽
            let detail_val = it.get("detail").filter(|d| !d.is_null());
            if detail_val.is_some_and(|d| d.is_object()) {
                let win = it.get("window").and_then(|v| v.as_object());
                let dur = win.and_then(|m| m.get("duration")).and_then(to_num);
                let unit = win
                    .map(|m| {
                        m.get("timeUnit")
                            .and_then(Value::as_str)
                            .or_else(|| m.get("unit").and_then(Value::as_str))
                            .unwrap_or("")
                    })
                    .unwrap_or("");
                let lower = unit.to_lowercase();
                let Some(dw) = detail_val.and_then(win_from_detail) else {
                    continue;
                };
                let slot = if dur == Some(300.0) && unit.contains("MINUTE") {
                    Some(WindowSlot::Rolling)
                } else if dur == Some(7.0) && lower.contains("day") {
                    Some(WindowSlot::Weekly)
                } else if dur == Some(30.0) && lower.contains("day") {
                    Some(WindowSlot::Monthly)
                } else if dur == Some(1.0) && unit.contains("WEEK") {
                    Some(WindowSlot::Weekly)
                } else if dur == Some(1.0) && unit.contains("MONTH") {
                    Some(WindowSlot::Monthly)
                } else {
                    None
                };
                if let Some(s) = slot {
                    w.set_if_empty(s, dw);
                }
                continue;
            }

            let typ = it.get("type").and_then(Value::as_str);

            // 2) TOKENS_LIMIT / TOKEN_LIMIT / QUOTA
            if matches!(typ, Some("TOKENS_LIMIT") | Some("TOKEN_LIMIT") | Some("QUOTA")) {
                let pct = if it
                    .get("percentage")
                    .is_some_and(|v| v.is_number() && to_num(v).is_some())
                {
                    it.get("percentage").and_then(to_num)
                } else {
                    pick_num(it, &["percent", "percentage", "usedPercent"])
                };
                let Some(pct) = pct else { continue };
                // resetInSec：nextResetTime 是正的毫秒时间戳时优先按"距今"推。
                let ris = match it.get("nextResetTime").and_then(Value::as_f64) {
                    Some(t) if t > 0.0 => Some(((t - now as f64) / 1000.0).max(0.0)),
                    _ => parse_reset_in_sec(
                        it.get("resetInSec").and_then(Value::as_f64),
                        pick(it, &["resetsAt", "nextResetTime"]).and_then(parse_resets_at),
                        now,
                    ),
                };
                let win = UsageWindow {
                    status: it
                        .get("status")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string()),
                    percent: Some(js_round(clamp_pct(pct))),
                    resets_at: pick(it, &["resetsAt", "nextResetTime"]).and_then(parse_resets_at),
                    reset_in_sec: ris,
                };
                let unit = it.get("unit").and_then(Value::as_f64);
                let number = it.get("number").and_then(Value::as_f64);
                let slot = if unit == Some(3.0) && number == Some(5.0) {
                    Some(WindowSlot::Rolling)
                } else if unit == Some(6.0) && number == Some(1.0) {
                    Some(WindowSlot::Weekly)
                } else if unit == Some(5.0) && number == Some(1.0) {
                    Some(WindowSlot::Monthly)
                } else if let Some(s) = it.get("window").and_then(Value::as_str) {
                    slot_from_window_string(s)
                } else if let Some(s) = it.get("limitWindowSeconds").and_then(Value::as_f64) {
                    match s {
                        18000.0 => Some(WindowSlot::Rolling),
                        604800.0 => Some(WindowSlot::Weekly),
                        2592000.0 => Some(WindowSlot::Monthly),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(s) = slot {
                    w.set_if_empty(s, win);
                }
                continue;
            }

            // 3) TIME_LIMIT → 月窗
            if typ == Some("TIME_LIMIT") {
                let pct = if it
                    .get("percentage")
                    .is_some_and(|v| v.is_number() && to_num(v).is_some())
                {
                    it.get("percentage").and_then(to_num)
                } else {
                    pick_num(it, &["percent"])
                };
                let Some(pct) = pct else { continue };
                let ra = pick(it, &["resetsAt", "nextResetTime"]).and_then(parse_resets_at);
                let ris = it
                    .get("nextResetTime")
                    .and_then(Value::as_f64)
                    .map(|t| ((t - now as f64) / 1000.0).max(0.0));
                w.set_if_empty(
                    WindowSlot::Monthly,
                    UsageWindow {
                        status: None,
                        percent: Some(js_round(clamp_pct(pct))),
                        resets_at: ra,
                        reset_in_sec: ris,
                    },
                );
                continue;
            }

            // 4) 通用条目：hint 判槽，认不出就填第一个空槽
            if let Some(gw) = win_from_detail(raw) {
                let slot = slot_from_hint(it).or_else(|| w.first_empty());
                if let Some(s) = slot {
                    w.set_if_empty(s, gw);
                }
            }
        }
    }

    // ---- direct 三窗口（dsh :19-20）----
    for (keys, slot) in [
        (&["rolling", "rollingUsage"][..], WindowSlot::Rolling),
        (&["weekly", "weeklyUsage"][..], WindowSlot::Weekly),
        (&["monthly", "monthlyUsage"][..], WindowSlot::Monthly),
    ] {
        if w.get(slot).is_none() {
            if let Some(v) = pick_any(&[inner, obj], keys).filter(|v| !v.is_null()) {
                if let Some(win) = norm_or_detail(v) {
                    w.set(slot, win);
                }
            }
        }
    }

    // ---- usage 容器（dsh :21-22）----
    let usage_val = pick_any(&[inner, obj], &["usage"]);
    if let Some(usage_obj) = usage_val.and_then(|v| v.as_object()) {
        let has_limits = inner.contains_key("limits") && !inner["limits"].is_null()
            || obj.contains_key("limits") && !obj["limits"].is_null();
        if w.weekly.is_none() {
            if let Some(gw) = win_from_detail(usage_val.unwrap()) {
                if has_limits {
                    w.set_if_empty(WindowSlot::Weekly, gw);
                } else if w.monthly.is_none() && w.rolling.is_none() {
                    w.set_if_empty(WindowSlot::Monthly, gw);
                }
            }
        }
        for (k, slot) in [
            ("rolling", WindowSlot::Rolling),
            ("weekly", WindowSlot::Weekly),
            ("monthly", WindowSlot::Monthly),
        ] {
            if w.get(slot).is_none() {
                if let Some(v) = usage_obj.get(k).filter(|v| !v.is_null()) {
                    if let Some(win) = win_from_direct(v) {
                        w.set(slot, win);
                    }
                }
            }
        }
    }

    // ---- quota/balance/credit 容器（dsh :23）----
    for key in ["quotas", "quota", "balances", "credits", "credit", "usages"] {
        let v_inner = inner.get(key).filter(|v| !v.is_null());
        let v_obj = obj.get(key).filter(|v| !v.is_null());
        // 对象容器（asRec 排除数组）
        if let Some(cont_val) = v_inner
            .filter(|v| v.is_object())
            .or_else(|| v_obj.filter(|v| v.is_object()))
        {
            let cont = cont_val.as_object().expect("checked object");
            for (k, slot) in [
                ("rolling", WindowSlot::Rolling),
                ("5h", WindowSlot::Rolling),
                ("session", WindowSlot::Rolling),
                ("weekly", WindowSlot::Weekly),
                ("monthly", WindowSlot::Monthly),
                ("total", WindowSlot::Monthly),
            ] {
                if w.get(slot).is_none() {
                    if let Some(v) = cont.get(k).filter(|v| !v.is_null()) {
                        if let Some(win) = win_from_direct(v) {
                            w.set(slot, win);
                        }
                    }
                }
            }
            if w.monthly.is_none()
                && (cont.contains_key("limit") && !cont["limit"].is_null()
                    || cont.contains_key("used") && !cont["used"].is_null())
            {
                if let Some(win) = win_from_detail(cont_val) {
                    w.set_if_empty(WindowSlot::Monthly, win);
                }
            }
        }
        // 数组容器
        if let Some(arr) = v_inner
            .and_then(Value::as_array)
            .or_else(|| v_obj.and_then(Value::as_array))
        {
            for raw in arr {
                let Some(r) = raw.as_object() else { continue };
                let detail = r.get("detail").filter(|d| !d.is_null()).unwrap_or(raw);
                let Some(gw) = win_from_detail(detail) else {
                    continue;
                };
                let hint = pick(r, &["window", "period", "kind"])
                    .and_then(Value::as_str)
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                let slot = if hint.contains("5h") || hint.contains("rolling") || hint.contains("hour") {
                    Some(WindowSlot::Rolling)
                } else if hint.contains("week") {
                    Some(WindowSlot::Weekly)
                } else if hint.contains("month") {
                    Some(WindowSlot::Monthly)
                } else {
                    None
                };
                if let Some(s) = slot {
                    w.set_if_empty(s, gw);
                }
            }
        }
    }

    // ---- windows 数组（dsh :24）----
    let w_arr = inner
        .get("windows")
        .and_then(Value::as_array)
        .or_else(|| obj.get("windows").and_then(Value::as_array));
    if let Some(w_arr) = w_arr {
        for raw in w_arr {
            let Some(r) = raw.as_object() else { continue };
            let Some(p) = pick_num(r, &["usedPercent", "percent", "percentage"]) else {
                continue;
            };
            let ra = pick(r, &["resetsAt", "resetAt"]).and_then(parse_resets_at);
            let win = UsageWindow {
                status: r
                    .get("status")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
                percent: Some(js_round(clamp_pct(p))),
                resets_at: ra,
                reset_in_sec: parse_reset_in_sec(r.get("resetInSec").and_then(Value::as_f64), ra, now),
            };
            let slot = match r.get("kind").and_then(Value::as_str) {
                Some("session") => Some(WindowSlot::Rolling),
                Some("weekly") => Some(WindowSlot::Weekly),
                Some("other") => Some(WindowSlot::Monthly),
                _ => None,
            };
            if let Some(s) = slot {
                w.set_if_empty(s, win);
            }
        }
    }

    // ---- config.creditUsagePercent → 周窗（dsh :25）----
    let cfg = pick_any(&[inner, obj], &["config"]).and_then(|v| v.as_object());
    if let Some(c) = cfg {
        let cp = pick_num(c, &["creditUsagePercent", "usagePercent"]);
        let bv = pick(c, &["billingPeriodEnd"])
            .or_else(|| {
                c.get("currentPeriod")
                    .and_then(|p| p.as_object())
                    .and_then(|p| p.get("end"))
                    .filter(|v| !v.is_null())
            })
            .and_then(parse_resets_at);
        if w.weekly.is_none() {
            if let Some(cp) = cp {
                w.set_if_empty(
                    WindowSlot::Weekly,
                    UsageWindow {
                        status: None,
                        percent: Some(js_round(clamp_pct(cp))),
                        resets_at: bv,
                        reset_in_sec: bv.map(|t| secs_until(t, now)),
                    },
                );
            }
        }
    }

    // ---- 三窗口全空时的兜底（dsh :26）----
    if w.all_empty() {
        let d = win_from_detail_inner(inner);
        let inner_has = inner.contains_key("limit") && !inner["limit"].is_null()
            || inner.contains_key("used") && !inner["used"].is_null();
        if d.is_some() && inner_has {
            w.set_if_empty(WindowSlot::Monthly, d.unwrap());
        } else {
            let d2 = win_from_detail(payload);
            let obj_has_limit = obj.contains_key("limit") && !obj["limit"].is_null();
            if d2.is_some() && obj_has_limit {
                w.set_if_empty(WindowSlot::Monthly, d2.unwrap());
            }
        }
    }

    // ---- rate_limit primary/secondary（dsh :27）----
    let rl = inner
        .get("rate_limit")
        .filter(|v| !v.is_null())
        .or_else(|| obj.get("rate_limit").filter(|v| !v.is_null()))
        .or_else(|| inner.get("rateLimit").filter(|v| !v.is_null()))
        .or_else(|| obj.get("rateLimit").filter(|v| !v.is_null()))
        .and_then(|v| v.as_object());
    if let Some(rl) = rl {
        for (keys, slot) in [
            (&["primary_window", "primaryWindow"][..], WindowSlot::Rolling),
            (&["secondary_window", "secondaryWindow"][..], WindowSlot::Weekly),
        ] {
            if w.get(slot).is_none() {
                if let Some(p) = rl.get(keys[0]).or_else(|| rl.get(keys[1])).filter(|v| !v.is_null())
                    .and_then(|v| v.as_object())
                {
                    if let Some(pct) = pick_num(p, &["used_percent", "usedPercent"]) {
                        let ra = pick(p, &["reset_at", "resetsAt"]).and_then(parse_resets_at);
                        w.set_if_empty(
                            slot,
                            UsageWindow {
                                status: None,
                                // dsh 此处只 round 不 clamp（原样透传上游）。
                                percent: Some(js_round(pct)),
                                resets_at: ra,
                                reset_in_sec: ra.map(|t| secs_until(t, now)),
                            },
                        );
                    }
                }
            }
        }
    }

    (w.rolling, w.weekly, w.monthly, level)
}

/// `win_from_detail(inner)`：inner 是 Map，借 Value 视图复用同一实现。
fn win_from_detail_inner(inner: &Map<String, Value>) -> Option<UsageWindow> {
    // Map 序列化回 Value 开销可忽略（仅三窗口全空的兜底路径）。
    win_from_detail(&Value::Object(inner.clone()))
}

// ---------------------------------------------------------------------------
// 余额（可选增强）
// ---------------------------------------------------------------------------

/// 金额字段名单（按优先级）。只有字段名明确"分/cent"才除以 100，否则按元原样。
const CNY_YUAN_KEYS: &[&str] = &[
    "balance",
    "available",
    "remaining",
    "rechargeBalance",
    "recharge_balance",
    "userBalance",
    "walletBalance",
    "availableBalance",
    "balanceAmount",
    "amount",
];
const CNY_CENT_KEYS: &[&str] = &[
    "balanceCent",
    "balance_cent",
    "rechargeBalanceCent",
    "fen",
];

/// 从余额 JSON 容错取金额（元）。解析不到 → None（绝不瞎猜）。
pub(crate) fn parse_money_cny(v: &Value) -> Option<Money> {
    // BFS 两层（root → data/balance 等包装对象），每层先查元名单再查分名单。
    let mut layer: Vec<&Value> = vec![v];
    for _ in 0..3 {
        let mut next: Vec<&Value> = Vec::new();
        for node in layer {
            let Some(obj) = node.as_object() else { continue };
            for keys in [CNY_YUAN_KEYS, CNY_CENT_KEYS] {
                for k in keys {
                    if let Some(val) = obj.get(*k).filter(|x| !x.is_null()) {
                        let is_cent = std::ptr::eq(keys, CNY_CENT_KEYS);
                        let num = match val {
                            Value::Number(n) => n.as_f64(),
                            Value::String(s) => s.trim().parse::<f64>().ok(),
                            _ => None,
                        };
                        if let Some(num) = num.filter(|n| n.is_finite() && *n >= 0.0) {
                            let amount = if is_cent { num / 100.0 } else { num };
                            return Some(Money {
                                amount,
                                unit: MoneyUnit::Cny,
                            });
                        }
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

/// `x-timeZone` 头（opencode-quota 每次请求现算 `UTC±HH:MM`；bias = 西向分钟）。
pub(crate) fn format_tz(bias_west_min: i32) -> String {
    let offset = -bias_west_min;
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 60, abs % 60)
}

fn timezone_header() -> String {
    // 注册表 ActiveTimeBias = 当前生效的西向分钟数（含夏令时修正）；
    // 读不到回退 UTC+00:00（x-timeZone 仅美容头，不影响鉴权）。
    let bias = {
        use winreg::enums::HKEY_LOCAL_MACHINE;
        use winreg::RegKey;
        RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(r"SYSTEM\CurrentControlSet\Control\TimeZoneInformation")
            .ok()
            .and_then(|k| k.get_value::<u32, _>("ActiveTimeBias").ok())
            .map(|b| b as i32)
            .unwrap_or(0)
    };
    format_tz(bias)
}

/// GET 平台接口（Cookie + 浏览器头；信封/HTTP/解析错误统一成 (code, message)）。
fn platform_get(http: &ureq::Agent, url: &str, cookie: &str) -> Result<Value, (String, String)> {
    let result = http
        .get(url)
        .set("Cookie", cookie)
        .set("Accept", "application/json")
        .set("Origin", PLATFORM_ORIGIN)
        .set("Referer", PLATFORM_REFERER)
        .set("User-Agent", BROWSER_UA)
        .set("x-timeZone", &timezone_header())
        .call();
    let (status, body) = match result {
        Ok(resp) => (200u16, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(ureq::Error::Transport(t)) => {
            return Err(("http".into(), format!("upstream request failed: {t}")));
        }
    };
    let value: Value = serde_json::from_str(&body)
        .map_err(|_| ("parse".to_string(), "invalid upstream response".to_string()))?;
    if let Some(err) = platform_envelope_error(&value) {
        return Err(err);
    }
    if status != 200 {
        return Err(http_status_error(status));
    }
    Ok(value)
}

/// 平台信封：`{code:0}` 成功；非 0 取 `msg|message`。401（带 loginUrl）=
/// Cookie 失效，给出**可操作**的更新指引（`upstream_envelope_error` 不能用于
/// 这里——它把 `code!=200` 全判错，而平台的成功码是 0）。
///
/// 平台会话是**服务端**签发的短寿命 token（`api-platform_serviceToken`），过期后
/// 只能由用户在浏览器里重新登录后取新 Cookie —— 客户端无法自行续期（无刷新端点，
/// 且 SSO 的 passport cookie 在 account.xiaomi.com 域下，跨域拿不到）。
fn platform_envelope_error(v: &Value) -> Option<(String, String)> {
    let obj = v.as_object()?;
    let code = match obj.get("code") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    }?;
    if code == 0 {
        return None;
    }
    let has_login = obj.get("loginUrl").is_some();
    if code == 401 && has_login {
        return Some((
            "auth".into(),
            "平台 Cookie 已失效：右键菜单 →「供应商管理 → 小米 MiMo → 添加账号…」重新粘贴 Cookie\
             （platform.xiaomimimo.com 登录后 F12 → Network → 复制任意 /api/ 请求的 Cookie），\
             或执行 quota-bar setcookie mimo <新cookie>"
                .into(),
        ));
    }
    let msg = obj
        .get("message")
        .or_else(|| obj.get("msg"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("平台返回 code {code}"));
    Some(("api".to_string(), msg))
}

/// 键名 → 槽位（`monthUsage`/`weeklyUsage`/`rollingUsage`/`5h…` 等）。
fn slot_from_key(kl: &str) -> Option<WindowSlot> {
    if kl.contains("5h") || kl.contains("hour") || kl.contains("rolling") || kl.contains("session")
    {
        Some(WindowSlot::Rolling)
    } else if kl.contains("week") {
        Some(WindowSlot::Weekly)
    } else if kl.contains("month") {
        Some(WindowSlot::Monthly)
    } else {
        None
    }
}

/// items[] → 一个窗口：优先 `*total*` 条目，取已用最高的有效项
/// （used/limit 或直给 percent；limit 必须 >0，缺失绝不造 0）。
fn best_item_window(items: &[Value], now: u64) -> Option<UsageWindow> {
    // 优先 `*total*` 条目（余额拆分项 gift/cash 不参与百分比）。
    let use_total = items.iter().any(|it| {
        it.get("name")
            .and_then(Value::as_str)
            .map(|n| n.contains("total"))
            .unwrap_or(false)
    });
    let mut best: Option<(f64, &Value)> = None;
    for it in items {
        if use_total
            && !it
                .get("name")
                .and_then(Value::as_str)
                .map(|n| n.contains("total"))
                .unwrap_or(false)
        {
            continue;
        }
        let Some(obj) = it.as_object() else { continue };
        let pct = match pick_num(obj, &["percent", "percentage", "usagePercent", "usedPercent"]) {
            Some(p) if p.is_finite() => Some(clamp_pct(js_round(p))),
            _ => {
                let used = pick_num(obj, &["used", "usedQuota", "consumed"]);
                let limit = pick_num(obj, &["limit", "quota", "total"]);
                let remaining = pick_num(obj, &["remaining", "remainingQuota", "available"]);
                match (used, remaining, limit) {
                    (Some(u), _, Some(l)) if l > 0.0 => Some(clamp_pct(js_round(u / l * 100.0))),
                    (_, Some(r), Some(l)) if l > 0.0 => Some(clamp_pct(js_round((1.0 - r / l) * 100.0))),
                    _ => None,
                }
            }
        };
        let Some(pct) = pct else { continue };
        if best.map(|(p, _)| pct > p).unwrap_or(true) {
            best = Some((pct, it));
        }
    }
    let (pct, chosen) = best?;
    let obj = chosen.as_object()?;
    let resets_at = pick(
        obj,
        &["resetsAt", "resetAt", "nextResetTime", "resetTime", "expireAt", "end"],
    )
    .and_then(parse_resets_at);
    Some(UsageWindow {
        percent: Some(pct),
        resets_at,
        reset_in_sec: parse_reset_in_sec(
            pick_num(obj, &["resetInSec", "reset_in_sec"]),
            resets_at,
            now,
        ),
        status: obj
            .get("status")
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
    })
}

/// 平台 `tokenPlan/usage` → 三窗口（键扫描 `*Usage.items[]`，兜底顶层 items 按名分槽）。
///
/// 两种上游形态都要认：
/// 1. `{"monthUsage":{"items":[{name,used,limit}…]}}` —— 逐条目取已用最高的有效项；
/// 2. `{"monthUsage":{"percent":0,"items":null}}` —— **容器自身就是窗口**
///    （实测：套餐已过期/无明细时上游只给一个 percent，没有 items 数组）。
pub(crate) fn parse_platform_usage(v: &Value) -> (Option<UsageWindow>, Option<UsageWindow>, Option<UsageWindow>) {
    let now = now_ms();
    let (mut rolling, mut weekly, mut monthly) = (None, None, None);
    let Some(data) = v.get("data").and_then(Value::as_object) else {
        return (None, None, None);
    };
    let set = |slot: WindowSlot, win: UsageWindow, rolling: &mut Option<UsageWindow>, weekly: &mut Option<UsageWindow>, monthly: &mut Option<UsageWindow>| {
        let target = match slot {
            WindowSlot::Rolling => rolling,
            WindowSlot::Weekly => weekly,
            WindowSlot::Monthly => monthly,
        };
        if target.is_none() {
            *target = Some(win);
        }
    };
    for (key, val) in data {
        let kl = key.to_lowercase();
        if !kl.contains("usage") {
            continue;
        }
        let Some(slot) = slot_from_key(&kl) else { continue };
        // 形态 1：items[] 明细
        let from_items = val
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| best_item_window(items, now));
        // 形态 2：容器自身（percent / used+limit / remaining+limit / resetsAt）
        let win = from_items.or_else(|| best_item_window(std::slice::from_ref(val), now));
        if let Some(win) = win {
            set(slot, win, &mut rolling, &mut weekly, &mut monthly);
        }
    }
    if rolling.is_none() && weekly.is_none() && monthly.is_none() {
        if let Some(items) = data.get("items").and_then(Value::as_array) {
            for it in items {
                let Some(name) = it.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let kl = name.to_lowercase();
                let Some(slot) = slot_from_key(&kl) else { continue };
                if let Some(win) = best_item_window(std::slice::from_ref(it), now) {
                    set(slot, win, &mut rolling, &mut weekly, &mut monthly);
                }
            }
        }
    }
    (rolling, weekly, monthly)
}

/// 平台 `tokenPlan/detail` 的 `currentPeriodEnd`（"2026-06-08 23:59:59"）→ unix 秒。
/// 上游 usage 不给月窗重置时刻时用它补（套餐周期结束即月窗重置）。
pub(crate) fn parse_platform_period_end(v: &Value) -> Option<i64> {
    let data = v.get("data")?;
    let raw = data.get("currentPeriodEnd").and_then(Value::as_str)?;
    parse_iso8601(raw)
}

/// 平台 `tokenPlan/detail` → 档位（`planName`，过期缀 `·已过期`）。
pub(crate) fn parse_platform_level(v: &Value) -> Option<String> {
    let data = v.get("data")?;
    let name = data.get("planName").and_then(Value::as_str)?;
    if name.trim().is_empty() {
        return None;
    }
    Some(if parse_platform_expired(v) {
        format!("{name} · 已过期")
    } else {
        name.to_string()
    })
}

/// 平台 `tokenPlan/detail.expired`：套餐是否已过期（显示层据此隐藏整行）。
pub(crate) fn parse_platform_expired(v: &Value) -> bool {
    v.get("data")
        .and_then(|d| d.get("expired"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct MimoProvider {
    id: ProviderId,
    /// 旧 Bearer 接口域名已下线；保留字段以备官方恢复（当前 fetch 不使用）。
    #[allow(dead_code)]
    api_key: Option<String>,
    /// 平台 Cookie：tokenPlan/usage、tokenPlan/detail、balance 三接口共用。
    cookie: Option<String>,
}

impl MimoProvider {
    /// 大陆站（默认）。
    pub fn new(api_key: Option<String>, cookie: Option<String>) -> Self {
        Self {
            id: ProviderId::Mimo,
            api_key,
            cookie,
        }
    }

    /// 新加坡站（平台同源，凭证独立）。
    pub fn sg(api_key: Option<String>, cookie: Option<String>) -> Self {
        Self {
            id: ProviderId::MimoSg,
            api_key,
            cookie,
        }
    }

    /// 缺 Cookie 错误（dsh noKey 风格指引：setcookie / 环境变量 / 复制路径）。
    fn no_cookie_error(&self) -> ProviderSnapshot {
        let env = if self.id == ProviderId::MimoSg {
            "MIMO_SG_USAGE_COOKIE"
        } else {
            "MIMO_USAGE_COOKIE"
        };
        ProviderSnapshot::error(
            self.id,
            "no-cookie",
            format!(
                "{} 已迁移平台 Cookie 接口：quota-bar setcookie {} <cookie>（platform.xiaomimimo.com 登录后 F12 → Network → 任意 /api/ 请求的 Cookie），或设环境变量 {}",
                self.id.display_name(),
                self.id.as_str(),
                env
            ),
        )
    }
}

impl QuotaProvider for MimoProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        let Some(cookie) = self.cookie.as_deref().filter(|c| !c.trim().is_empty()) else {
            return self.no_cookie_error();
        };

        // 1) 套餐用量（核心）；失败 → 错误行（面板/条上直接显示原因）。
        let balance = platform_get(http, BALANCE_ENDPOINT, cookie)
            .ok().and_then(|v| parse_money_cny(&v));
        let usage = match platform_get(http, USAGE_ENDPOINT, cookie) {
            Ok(v) => v,
            Err((code, message)) => {
                let mut snap = ProviderSnapshot::error(self.id, &code, message);
                snap.balance = balance;
                return snap;
            },
        };
        let (rolling, weekly, monthly) = parse_platform_usage(&usage);
        let mut snap = ProviderSnapshot::new(self.id);
        snap.rolling = rolling;
        snap.weekly = weekly;
        snap.monthly = monthly;

        // 2) 套餐详情（档位 + 周期结束时刻）与 3) 余额：尽力而为，失败静默不影响窗口。
        let detail = platform_get(http, DETAIL_ENDPOINT, cookie).ok();
        snap.level = detail.as_ref().and_then(parse_platform_level);
        snap.plan_expired = detail.as_ref().is_some_and(parse_platform_expired);
        // 月窗重置：usage 没给就用 detail 的套餐周期结束时刻补（过期套餐即过去时刻）。
        if let Some(monthly) = snap.monthly.as_mut() {
            if monthly.resets_at.is_none() {
                if let Some(end) = detail.as_ref().and_then(parse_platform_period_end) {
                    monthly.resets_at = Some(end);
                    monthly.reset_in_sec = Some(secs_until(end, now_ms()));
                }
            }
        }
        snap.balance = balance;
        snap
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

    /// 断言窗口存在并返回其 percent。
    fn pct(w: &Option<UsageWindow>, label: &str) -> f64 {
        w.as_ref()
            .and_then(|w| w.percent)
            .unwrap_or_else(|| panic!("{label} 应有 percent"))
    }

    // ---- 平台 Cookie 接口（生产路径）----

    #[test]
    fn platform_remaining_quota_is_used_percentage_without_guessing_zero() {
        let payload = parse(r#"{"data":{"monthUsage":{"remaining":"75","limit":"100"}}}"#);
        let (_, _, monthly) = parse_platform_usage(&payload);
        assert_eq!(monthly.unwrap().percent, Some(25.0));
        let payload = parse(r#"{"data":{"monthUsage":{"remaining":"0","limit":"100"}}}"#);
        assert_eq!(parse_platform_usage(&payload).2.unwrap().percent, Some(100.0));
        let payload = parse(r#"{"data":{"monthUsage":{"remaining":"75"}}}"#);
        assert!(parse_platform_usage(&payload).2.is_none());
    }

    #[test]
    fn platform_usage_items_prefer_total_and_map_slots() {
        let v = parse(
            r#"{"code":0,"data":{
                "monthUsage":{"items":[
                    {"name":"month_total_token","used":630,"limit":1000,"nextResetTime":1790208000000},
                    {"name":"month_gift_token","used":99,"limit":100}
                ]},
                "weeklyUsage":{"items":[{"name":"week_total_token","used":3,"limit":100}]},
                "rollingUsage":{"items":[{"name":"5h_total_token","used":1,"limit":100}]}
            }}"#,
        );
        let (r, w, m) = parse_platform_usage(&v);
        // 月：total(63%) 胜出，不被 gift(99%) 污染；nextResetTime 毫秒 → unix 秒
        assert_eq!(pct(&m, "monthly"), 63.0);
        assert_eq!(m.as_ref().and_then(|x| x.resets_at), Some(1_790_208_000));
        // 周 / 5h
        assert_eq!(pct(&w, "weekly"), 3.0);
        assert_eq!(pct(&r, "rolling"), 1.0);
    }

    #[test]
    fn platform_usage_percent_fallback_and_missing_limit_gives_none() {
        // 直给 percent（无 limit）也能用
        let v = parse(r#"{"code":0,"data":{"monthUsage":{"items":[{"name":"m","percent":55.4}]}}}"#);
        let (_, _, m) = parse_platform_usage(&v);
        assert_eq!(pct(&m, "monthly"), 55.0);
        // limit 缺失 → 整窗 None（绝不造 0）
        let v2 = parse(r#"{"code":0,"data":{"monthUsage":{"items":[{"name":"m","used":5}]}}}"#);
        let (_, _, m2) = parse_platform_usage(&v2);
        assert!(m2.is_none(), "limit 缺失不得伪造百分比");
        // data 缺失 → 三窗全 None，不 panic
        assert_eq!(
            parse_platform_usage(&parse(r#"{"code":0}"#)),
            (None, None, None)
        );
    }

    #[test]
    fn platform_slot_from_key_variants() {
        use crate::model::WindowSlot::*;
        assert_eq!(slot_from_key("monthusage"), Some(Monthly));
        assert_eq!(slot_from_key("weeklyusage"), Some(Weekly));
        assert_eq!(slot_from_key("rollingusage"), Some(Rolling));
        assert_eq!(slot_from_key("5husage"), Some(Rolling));
        assert_eq!(slot_from_key("hourlyusage"), Some(Rolling));
        assert_eq!(slot_from_key("balanceusage"), None);
    }

    /// 容器级 percent 形态（实测：套餐无明细/已过期时上游只给
    /// `{"monthUsage":{"percent":0,"items":null}}`）——**必须**出月窗，
    /// 否则条上只剩余额、看不到"月 0%"。
    #[test]
    fn platform_usage_container_percent_without_items() {
        let v = parse(r#"{"code":0,"message":"","data":{"monthUsage":{"percent":0,"items":null},"usage":null}}"#);
        let (r, w, m) = parse_platform_usage(&v);
        assert!(r.is_none() && w.is_none(), "上游只给了月窗");
        assert_eq!(pct(&m, "monthly"), 0.0, "容器 percent 直接成窗");
        // 容器级 used+limit 同样认（与 items 明细同款口径）
        let v2 = parse(r#"{"code":0,"data":{"monthUsage":{"used":25,"limit":100}}}"#);
        assert_eq!(pct(&parse_platform_usage(&v2).2, "monthly"), 25.0);
        // items 明细优先于容器字段（两者并存时不被容器拉平）
        let v4 = parse(
            r#"{"code":0,"data":{"monthUsage":{"percent":9,"items":[{"name":"month_total_token","used":63,"limit":100}]}}}"#,
        );
        assert_eq!(pct(&parse_platform_usage(&v4).2, "monthly"), 63.0);
        // 容器既无 percent 也无 used+limit → 不造窗（宁缺毋滥）
        let v5 = parse(r#"{"code":0,"data":{"monthUsage":{"items":null}}}"#);
        assert_eq!(parse_platform_usage(&v5), (None, None, None));
        let v6 = parse(r#"{"code":0,"data":{"monthUsage":{"limit":100,"remaining":40}}}"#);
        assert_eq!(pct(&parse_platform_usage(&v6).2, "monthly"), 60.0,
            "remaining + limit 可计算已用百分比");
    }

    /// detail 的 `currentPeriodEnd`（空格分隔的本地时间串）→ unix 秒。
    #[test]
    fn platform_period_end_parses_detail_string() {
        let v = parse(r#"{"code":0,"data":{"planName":"Standard","currentPeriodEnd":"2026-06-08 23:59:59"}}"#);
        assert_eq!(parse_platform_period_end(&v), parse_iso8601("2026-06-08 23:59:59"));
        // 缺字段 / 空串 → None（不猜）
        assert_eq!(parse_platform_period_end(&parse(r#"{"code":0,"data":{}}"#)), None);
        assert_eq!(
            parse_platform_period_end(&parse(r#"{"code":0,"data":{"currentPeriodEnd":""}}"#)),
            None
        );
    }

    #[test]
    fn platform_envelope_success_error_and_cookie_expiry() {
        // code:0 成功（注意：upstream_envelope_error 会把 0 误判为错，故独立信封）
        assert_eq!(
            platform_envelope_error(&parse(r#"{"code":0,"data":{}}"#)),
            None
        );
        // 无 code → 交给 HTTP 状态/解析处理
        assert_eq!(platform_envelope_error(&parse(r#"{"data":{}}"#)), None);
        // 业务错误取 msg
        let (code, msg) =
            platform_envelope_error(&parse(r#"{"code":500,"msg":"服务器错误"}"#)).unwrap();
        assert_eq!(code, "api");
        assert_eq!(msg, "服务器错误");
        // 401 + loginUrl → Cookie 失效，带**菜单路径**与 CLI 双指引
        let (code, msg) = platform_envelope_error(
            &parse(r#"{"code":401,"loginUrl":"https://account.xiaomi.com/..."}"#),
        )
        .unwrap();
        assert_eq!(code, "auth");
        assert!(msg.contains("供应商管理"), "{msg}");
        assert!(msg.contains("添加账号"), "{msg}");
        assert!(msg.contains("setcookie mimo"), "{msg}");
    }

    #[test]
    fn platform_detail_level_with_expired_suffix() {
        let v = parse(r#"{"code":0,"data":{"planName":"Pro 套餐","expired":true}}"#);
        assert_eq!(parse_platform_level(&v).as_deref(), Some("Pro 套餐 · 已过期"));
        assert!(parse_platform_expired(&v), "过期标志独立可取（显示层据此隐藏整行）");
        let v2 = parse(r#"{"code":0,"data":{"planName":"Basic"}}"#);
        assert_eq!(parse_platform_level(&v2).as_deref(), Some("Basic"));
        assert!(!parse_platform_expired(&v2), "缺字段 = 未过期（宁显示不隐藏）");
        // 空名 / 缺 data → None（档位缺失不冒充）
        assert_eq!(
            parse_platform_level(&parse(r#"{"code":0,"data":{"planName":"  "}}"#)),
            None
        );
        assert_eq!(parse_platform_level(&parse(r#"{"code":0}"#)), None);
        assert!(!parse_platform_expired(&parse(r#"{"code":0}"#)));
    }

    #[test]
    fn timezone_formats_from_west_bias() {
        // 中国（西向 -480 分钟）→ UTC+08:00
        assert_eq!(format_tz(-480), "UTC+08:00");
        assert_eq!(format_tz(300), "UTC-05:00");
        assert_eq!(format_tz(0), "UTC+00:00");
        assert_eq!(format_tz(-210), "UTC+03:30");
    }

    #[test]
    fn missing_cookie_gives_setcookie_guidance_without_network() {
        // 无 cookie → 立即返回 no-cookie 错误（不建连接，测试离线安全）
        let p = MimoProvider::new(None, None);
        let snap = p.fetch(&crate::providers::make_agent());
        assert_eq!(snap.error.as_deref(), Some("no-cookie"));
        assert!(snap.message.as_deref().unwrap().contains("setcookie mimo"));
    }

    #[test]
    fn limits_duration_variants_fill_slots_first_wins() {
        let (r, wk, mo, level) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_limits.json")));
        // 300 MINUTE detail（percent 150 → clamp 100）
        assert_eq!(pct(&r, "rolling"), 100.0);
        assert_eq!(
            r.as_ref().and_then(|w| w.resets_at),
            Some(1_790_208_000),
            "2026-09-24T00:00:00Z"
        );
        // 7 DAY detail（limit/used → 25%；reset_at epoch 秒）
        assert_eq!(pct(&wk, "weekly"), 25.0);
        assert_eq!(wk.as_ref().and_then(|w| w.resets_at), Some(1_788_782_400));
        // 30 DAY detail（limit/remaining → 60%）
        assert_eq!(pct(&mo, "monthly"), 60.0);
        // 后两条 1 WEEK / 1 MONTH 是同槽已占用 → 首占优，不覆盖
        assert_eq!(pct(&wk, "weekly"), 25.0, "88 不应覆盖 25");
        assert_eq!(pct(&mo, "monthly"), 60.0, "90 不应覆盖 60");
        // level：inner 无 → 顶层 code/level 里的 level
        assert_eq!(level.as_deref(), Some("PRO"));
    }

    #[test]
    fn tokens_limit_unit_number_slots_and_reset_shapes() {
        let (r, wk, mo, _) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_tokens.json")));
        // unit=3 number=5 → 5小时槽；percentage + status + ISO reset
        assert_eq!(pct(&r, "rolling"), 44.0);
        let r = r.unwrap();
        assert_eq!(r.status.as_deref(), Some("ok"));
        assert_eq!(r.resets_at, Some(1_790_222_400), "2026-09-24T04:00:00Z");
        // unit=6 number=1 → 周槽；nextResetTime epoch 毫秒
        assert_eq!(pct(&wk, "weekly"), 7.0);
        assert_eq!(wk.as_ref().and_then(|w| w.resets_at), Some(1_789_000_000));
        // unit=5 number=1 → 月槽；resetInSec 直给倒计时
        assert_eq!(pct(&mo, "monthly"), 61.0);
        assert_eq!(mo.as_ref().and_then(|w| w.reset_in_sec), Some(9999.0));
    }

    #[test]
    fn limit_window_seconds_slotting_and_hint_does_not_override() {
        let (r, wk, mo, _) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_window.json")));
        assert_eq!(pct(&r, "rolling"), 11.0);
        assert_eq!(pct(&wk, "weekly"), 22.0);
        assert_eq!(wk.as_ref().and_then(|w| w.resets_at), Some(1_790_294_400));
        assert_eq!(pct(&mo, "monthly"), 33.0);
        // 第4项 window:"5h" 命中 rolling 已占 → 首占优不覆盖
        assert_eq!(pct(&r, "rolling"), 11.0, "99 不应覆盖 11");
    }

    #[test]
    fn rate_limit_primary_secondary_fill_rolling_and_weekly() {
        let (r, wk, mo, _) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_rate_limit.json")));
        let r = r.expect("primary_window → rolling");
        assert_eq!(r.percent, Some(27.0));
        assert_eq!(r.resets_at, Some(1_790_251_200));
        // 固定 fixture 可能已过期；倒计时按当前时刻计算并截断到 0。
        let ris = r.reset_in_sec.expect("reset_in_sec 由 reset_at 推");
        let expected = secs_until(r.resets_at.unwrap(), now_ms());
        assert!((ris - expected).abs() <= 1.0, "ris={ris}, expected={expected}");
        let wk = wk.expect("secondary_window → weekly");
        assert_eq!(wk.percent, Some(4.0));
        assert_eq!(wk.resets_at, Some(1_790_769_600), "2026-09-30T12:00:00Z");
        assert!(mo.is_none(), "rate_limit 不该产出月窗");
    }

    #[test]
    fn direct_windows_use_norm_shape_and_beat_usage_keys() {
        let (r, wk, mo, level) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_direct.json")));
        let r = r.expect("direct rolling");
        assert_eq!(r.percent, Some(8.0));
        assert_eq!(r.status.as_deref(), Some("rate-limited"));
        assert_eq!(r.resets_at, Some(1_790_236_800), "2026-09-24T08:00:00Z");
        assert_eq!(r.reset_in_sec, Some(1234.0));
        // norm() 认 usagePercent
        assert_eq!(pct(&wk, "weekly"), 19.0);
        assert_eq!(pct(&mo, "monthly"), 52.0);
        // data.usage.rolling 因 rolling 已占不覆盖
        assert_eq!(r.percent, Some(8.0), "99 不应覆盖 8");
        // level：inn.subscription.tier 命中候选
        assert_eq!(level.as_deref(), Some("MAX"));
    }

    #[test]
    fn time_limit_goes_monthly_and_config_credit_usage_goes_weekly() {
        let (r, wk, mo, _) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_config.json")));
        assert!(r.is_none(), "rolling 无来源");
        assert_eq!(pct(&wk, "weekly"), 5.0, "config.creditUsagePercent");
        let wk = wk.unwrap();
        assert_eq!(wk.resets_at, Some(1_790_726_400), "billingPeriodEnd");
        assert_eq!(pct(&mo, "monthly"), 90.0, "TIME_LIMIT → 月窗");
        assert_eq!(
            mo.as_ref().unwrap().resets_at,
            Some(1_790_812_800),
            "2026-10-01T00:00:00Z"
        );
    }

    #[test]
    fn fallback_uses_inner_limit_used_for_monthly() {
        let (r, wk, mo, _) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_fallback.json")));
        assert!(r.is_none() && wk.is_none());
        assert_eq!(pct(&mo, "monthly"), 25.0, "used/limit → 25%");
    }

    #[test]
    fn empty_object_yields_all_none_without_panic() {
        let (r, wk, mo, level) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_empty.json")));
        assert!(r.is_none() && wk.is_none() && mo.is_none() && level.is_none());
        // 非对象入参也不 panic
        let (a, b, c, d) = norm_mimo(&parse("[1,2,3]"));
        assert!(a.is_none() && b.is_none() && c.is_none() && d.is_none());
        let (a, b, c, d) = norm_mimo(&parse("\"str\""));
        assert!(a.is_none() && b.is_none() && c.is_none() && d.is_none());
    }

    #[test]
    fn windows_array_maps_kind_and_subscription_level() {
        let (r, wk, mo, level) = norm_mimo(&fixture(include_str!("../../tests/fixtures/mimo_misc.json")));
        let r = r.expect("kind=session");
        assert_eq!(r.percent, Some(15.0));
        assert_eq!(r.reset_in_sec, Some(500.0));
        assert_eq!(
            r.resets_at,
            Some(1_790_208_000),
            "2026-09-24T00:00:00Z"
        );
        assert_eq!(pct(&wk, "kind=weekly"), 25.0);
        assert_eq!(pct(&mo, "kind=other"), 35.0);
        assert_eq!(
            mo.as_ref().unwrap().resets_at,
            Some(1_790_251_200),
            "resetAt epoch 秒"
        );
        // data.subscription.level → ELITE
        assert_eq!(level.as_deref(), Some("ELITE"));
    }

    #[test]
    fn money_field_name_decides_yuan_vs_cent() {
        // 字段名是"分"才除以 100
        assert_eq!(
            parse_money_cny(&parse(r#"{"balanceCent": 1250}"#)),
            Some(Money { amount: 12.5, unit: MoneyUnit::Cny })
        );
        // 否则按元原样（1250 元不动）
        assert_eq!(
            parse_money_cny(&parse(r#"{"balance": 1250}"#)),
            Some(Money { amount: 1250.0, unit: MoneyUnit::Cny })
        );
        // 常见包装 + 字符串数字
        assert_eq!(
            parse_money_cny(&parse(r#"{"data": {"available": "3.00"}}"#)),
            Some(Money { amount: 3.0, unit: MoneyUnit::Cny })
        );
        // 认不出就 None，绝不瞎猜
        assert_eq!(parse_money_cny(&parse(r#"{"used": 42}"#)), None);
        assert_eq!(parse_money_cny(&parse("{}")), None);
    }

    #[test]
    fn envelope_error_recognizes_both_shapes() {
        // {type:'error', error:{type,message}}
        let (code, msg) = upstream_envelope_error(&parse(r#"{"type":"error","error":{"type":"rate_limit","message":"quota exceeded"}}"#))
            .expect("应识别 error 信封");
        assert_eq!(code, "rate_limit");
        assert_eq!(msg, "quota exceeded");
        // {code != 200, msg}
        let (code, msg) =
            upstream_envelope_error(&parse(r#"{"code":500,"msg":"server busy"}"#)).expect("应识别 code 信封");
        assert_eq!(code, "api");
        assert_eq!(msg, "server busy");
        // 正常数据没有信封
        assert!(upstream_envelope_error(&parse(r#"{"code":200,"msg":"ok"}"#)).is_none());
        assert!(upstream_envelope_error(&fixture(include_str!("../../tests/fixtures/mimo_limits.json"))).is_none());
    }

    #[test]
    fn iso8601_parser_covers_common_shapes() {
        assert_eq!(parse_iso8601("2026-09-24T00:00:00Z"), Some(1_790_208_000));
        assert_eq!(parse_iso8601("2026-09-24T00:00:00.500Z"), Some(1_790_208_000));
        // +08:00 偏移
        assert_eq!(
            parse_iso8601("2026-09-24T08:00:00+08:00"),
            Some(1_790_208_000)
        );
        // 无时区 → UTC
        assert_eq!(parse_iso8601("2026-09-24 00:00:00"), Some(1_790_208_000));
        // 仅日期
        assert_eq!(parse_iso8601("2026-09-24"), Some(1_790_208_000));
        // 非法输入
        assert_eq!(parse_iso8601("not-a-time"), None);
        assert_eq!(parse_iso8601("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-02-30T00:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-09-24T00:00:00Zx"), None);
    }
}
