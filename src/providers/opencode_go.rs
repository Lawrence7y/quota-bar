//! OpenCode Go 套餐额度：`GET https://opencode.ai/zen/go/v1/usage`。
//! Bearer `OPENCODE_GO_API_KEY` / `OPENCODE_API_KEY`；解析移植自 dsh `src/plans/opencode-go.ts`（MIT）。
//! 响应 `useBalance: true` 表示超额度后改扣 Zen 余额（标志位）；余额**数值**
//! dsh 未透出 —— 这里只做保守嗅探（见 `sniff_usd_balance`），认不出就不给。

use serde_json::{Map, Value};

use crate::model::{Money, MoneyUnit, ProviderId, ProviderSnapshot, UsageWindow};
use crate::providers::mimo::{
    clamp_pct, http_status_error, now_ms, secs_until, upstream_envelope_error,
};
use crate::providers::QuotaProvider;

const ENDPOINT: &str = "https://opencode.ai/zen/go/v1/usage";

/// `norm()`：dsh plans/util.ts 的单窗口归一（percent|usagePercent / status /
/// resetsAt ISO / resetInSec）。复用 mimo 的 `norm_wire`（同一份 dsh norm 移植）。
fn norm(raw: Option<&Value>) -> Option<UsageWindow> {
    raw.and_then(crate::providers::mimo::norm_wire)
}

/// OpenCode usage 响应归一（dsh opencode-go.ts fetchPlan 主体）。
///
/// - `usage.rolling|weekly|monthly` 优先，缺 `usage.*` 落到根级 `*Usage`；
/// - `useBalance` 是严格布尔 true 才置位；
/// - 有 `resetsAt`（ISO → unix 秒）而无倒计时时补推 `reset_in_sec`；
/// - 余额数值字段（若有）按美元嗅探 —— dsh 不透出该字段，取不到就 None。
pub fn norm_opencode(
    payload: &Value,
) -> (Option<UsageWindow>, Option<UsageWindow>, Option<UsageWindow>, bool, Option<Money>) {
    let empty_map = Map::new();
    let obj = payload.as_object().unwrap_or(&empty_map);
    let use_balance = obj.get("useBalance").and_then(Value::as_bool) == Some(true);
    // `obj.usage` 是对象才生效，否则整根当 usage 域（dsh 同）。
    let usage = obj
        .get("usage")
        .filter(|u| !u.is_null())
        .and_then(|u| u.as_object())
        .unwrap_or(obj);
    let root = obj;

    let mut rolling = norm(usage.get("rolling").filter(|v| !v.is_null()).or_else(|| {
        root.get("rollingUsage").filter(|v| !v.is_null())
    }));
    let mut weekly = norm(usage.get("weekly").filter(|v| !v.is_null()).or_else(|| {
        root.get("weeklyUsage").filter(|v| !v.is_null())
    }));
    let mut monthly = norm(usage.get("monthly").filter(|v| !v.is_null()).or_else(|| {
        root.get("monthlyUsage").filter(|v| !v.is_null())
    }));

    // resetsAt → reset_in_sec（任务允许的推导；上游直接给了倒计时则不覆盖）。
    let now = now_ms();
    for w in [&mut rolling, &mut weekly, &mut monthly] {
        if let Some(win) = w {
            if win.reset_in_sec.is_none() {
                if let Some(ra) = win.resets_at {
                    win.reset_in_sec = Some(secs_until(ra, now));
                }
            }
        }
        // 上游偶发越界的 percent 钳到 0–100（存储原值，显示层用 pct() 也钳）。
        if let Some(win) = w.as_mut() {
            if let Some(p) = win.percent {
                win.percent = Some(clamp_pct(p));
            }
        }
    }

    let balance = sniff_usd_balance(payload);
    (rolling, weekly, monthly, use_balance, balance)
}

/// 余额数值嗅探（美元）。
///
/// dsh `opencode-go.ts` 只透出 `useBalance` 标志、没有余额数值字段；若上游
/// 某天在响应里补了余额，这里按常见字段名认领为 `Money{Usd}`（opencode zen
/// 以美元计价 —— 上下文表明币种）。认不出 → None，**绝不造数**。
fn sniff_usd_balance(v: &Value) -> Option<Money> {
    const USD_KEYS: &[&str] = &[
        "zenbalance",
        "zen_balance",
        "balanceusd",
        "balance_usd",
        "availablebalance",
        "available_balance",
        "remainingbalance",
        "remaining_balance",
        "creditbalance",
        "credit_balance",
        "walletbalance",
        "credits",
        "balance",
    ];
    let mut layer: Vec<&Value> = vec![v];
    for _ in 0..3 {
        let mut next: Vec<&Value> = Vec::new();
        for node in layer {
            let Some(obj) = node.as_object() else { continue };
            for key in USD_KEYS {
                if let Some(val) = obj
                    .iter()
                    .find(|(k, _)| k.to_ascii_lowercase() == *key)
                    .map(|(_, val)| val)
                    .filter(|val| !val.is_null())
                {
                    let num = match val {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.trim().parse::<f64>().ok(),
                        _ => None,
                    };
                    if let Some(num) = num.filter(|n| n.is_finite() && *n >= 0.0) {
                        return Some(Money {
                            amount: num,
                            unit: MoneyUnit::Usd,
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

pub struct OpenCodeGoProvider {
    api_key: Option<String>,
}

impl OpenCodeGoProvider {
    pub fn new(api_key: Option<String>) -> Self {
        Self { api_key }
    }

    /// dsh noKey 风格的缺 key 错误（环境变量两个候选都提示）。
    fn no_key_error(&self) -> ProviderSnapshot {
        let envs = ProviderId::OpenCodeGo
            .env_key_names()
            .join(" / ");
        ProviderSnapshot::error(
            ProviderId::OpenCodeGo,
            "no-key",
            format!(
                "未配置 OpenCode Go API Key，用 quota-bar setkey opencode-go <key> 或设环境变量 {envs}"
            ),
        )
    }
}

impl QuotaProvider for OpenCodeGoProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCodeGo
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        let Some(key) = self.api_key.as_deref().filter(|k| !k.trim().is_empty()) else {
            return self.no_key_error();
        };

        let result = http
            .get(ENDPOINT)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Accept", "application/json")
            .call();
        let (status, body) = match result {
            Ok(resp) => (resp.status(), resp.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
            Err(ureq::Error::Transport(t)) => {
                return ProviderSnapshot::error(
                    ProviderId::OpenCodeGo,
                    "http",
                    format!("upstream request failed: {t}"),
                );
            }
        };

        let value: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                return ProviderSnapshot::error(
                    ProviderId::OpenCodeGo,
                    "parse",
                    "invalid upstream response",
                );
            }
        };
        // 错误信封优先于 HTTP 状态（dsh curlJson :93-115 顺序）。
        if let Some((code, message)) = upstream_envelope_error(&value) {
            return ProviderSnapshot::error(ProviderId::OpenCodeGo, &code, message);
        }
        if status != 200 {
            let (code, message) = http_status_error(status);
            return ProviderSnapshot::error(ProviderId::OpenCodeGo, &code, message);
        }
        if !value.is_object() && !value.is_array() {
            return ProviderSnapshot::error(
                ProviderId::OpenCodeGo,
                "empty",
                "empty response",
            );
        }

        let (rolling, weekly, monthly, use_balance, balance) = norm_opencode(&value);
        let mut snap = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        snap.rolling = rolling;
        snap.weekly = weekly;
        snap.monthly = monthly;
        snap.use_balance = use_balance;
        snap.balance = balance;
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mimo::upstream_envelope_error;

    fn parse(json: &str) -> Value {
        serde_json::from_str(json).expect("fixture 是合法 JSON")
    }

    /// fixture 内容以 `include_str!`（路径必须是字面量）传入，这里只负责解析。
    fn fixture(raw: &str) -> Value {
        parse(raw)
    }

    fn pct(w: &Option<UsageWindow>, label: &str) -> f64 {
        w.as_ref()
            .and_then(|w| w.percent)
            .unwrap_or_else(|| panic!("{label} 应有 percent"))
    }

    #[test]
    fn three_windows_use_balance_and_rate_limited_status() {
        let (r, wk, mo, use_balance, balance) = norm_opencode(&fixture(include_str!("../../tests/fixtures/opencode_usage.json")));
        // usage.rolling → 5小时槽（ISO reset → unix 秒 + 推导倒计时）
        let r = r.expect("rolling");
        assert_eq!(r.percent, Some(35.0));
        assert_eq!(r.status.as_deref(), Some("rate-limited"));
        assert_eq!(r.resets_at, Some(1_790_208_000), "2026-09-24T00:00:00Z");
        let ris = r.reset_in_sec.expect("由 resetsAt 推 reset_in_sec");
        // fixture 的 resetsAt 是**固定时刻**（2026-09-24T00:00:00Z）：未到点时是正
        // 倒计时，到点后由 `secs_until` 钳成 0 —— 恒落在 [0, 7天) 内、绝不为负。
        assert!((0.0..7.0 * 86400.0).contains(&ris), "ris={ris}");
        // 根级 weeklyUsage / monthlyUsage（usage 域缺键时的回落）
        assert_eq!(pct(&wk, "weekly"), 12.0);
        assert_eq!(wk.as_ref().unwrap().reset_in_sec, Some(3600.0), "上游直给");
        assert_eq!(pct(&mo, "monthly"), 61.0);
        // useBalance 标志置位；无余额数值字段 → 不造 Money
        assert!(use_balance);
        assert!(balance.is_none(), "dsh 响应无余额数值，balance 必须 None");
    }

    #[test]
    fn missing_percent_stays_none_and_never_defaults_to_zero() {
        let (r, wk, mo, use_balance, balance) =
            norm_opencode(&fixture(include_str!("../../tests/fixtures/opencode_missing_percent.json")));
        // rolling 有窗口对象但缺 percent → percent 保持 None（绝不 0）
        let r = r.expect("rolling 窗口对象仍在");
        assert!(r.percent.is_none());
        assert_eq!(r.status.as_deref(), Some("rate-limited"));
        assert_eq!(r.resets_at, Some(1_790_208_000));
        // weekly 空对象 → 全 None；monthly 非法 resetsAt → None
        let wk = wk.expect("weekly 窗口对象仍在");
        assert!(wk.percent.is_none() && wk.resets_at.is_none());
        let mo = mo.expect("monthly 窗口对象仍在");
        assert!(mo.percent.is_none() && mo.resets_at.is_none());
        assert!(!use_balance);
        assert!(balance.is_none());
        // worst_used 不把 None 当 0 参与（模型层纪律）
        let mut snap = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        snap.rolling = Some(r);
        assert!(snap.worst_used().is_none() || snap.worst_used().unwrap().1 != 0.0);
    }

    #[test]
    fn error_envelope_is_recognized_before_status() {
        // {type:'error', error:{type,message}}
        let (code, msg) =
            upstream_envelope_error(&fixture(include_str!("../../tests/fixtures/opencode_error.json"))).expect("应识别 error 信封");
        assert_eq!(code, "rate_limit");
        assert_eq!(msg, "quota exceeded");
        // {code != 200, msg}
        let (code, msg) =
            upstream_envelope_error(&fixture(include_str!("../../tests/fixtures/opencode_error_code.json"))).expect("应识别 code 信封");
        assert_eq!(code, "api");
        assert_eq!(msg, "server busy");
        // 正常 usage 响应不误报
        assert!(upstream_envelope_error(&fixture(include_str!("../../tests/fixtures/opencode_usage.json"))).is_none());
    }

    #[test]
    fn sniff_balance_only_when_numeric_field_exists() {
        // 若上游补了余额数值 → 美元
        assert_eq!(
            sniff_usd_balance(&parse(r#"{"useBalance": true, "zenBalance": 3.5}"#)),
            Some(Money { amount: 3.5, unit: MoneyUnit::Usd })
        );
        assert_eq!(
            sniff_usd_balance(&parse(r#"{"balance": "12.00"}"#)),
            Some(Money { amount: 12.0, unit: MoneyUnit::Usd })
        );
        // useBalance 是布尔不是数值 → 不认
        assert!(sniff_usd_balance(&parse(r#"{"useBalance": true}"#)).is_none());
        // 认不出 → None
        assert!(sniff_usd_balance(&parse(r#"{"usage": {"rolling": {"percent": 1}}}"#)).is_none());
    }

    #[test]
    fn no_key_error_names_both_env_vars() {
        let p = OpenCodeGoProvider::new(None);
        let snap = p.fetch(&crate::providers::make_agent());
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        let msg = snap.message.unwrap();
        assert!(msg.contains("OPENCODE_GO_API_KEY"), "{msg}");
        assert!(msg.contains("OPENCODE_API_KEY"), "{msg}");
        assert!(msg.contains("quota-bar setkey opencode-go"), "{msg}");
    }
}
