//! 额度数据模型：套餐三窗口（5小时 / 周 / 月已用百分比）+ 金额余额。
//!
//! 纪律（沿用 Quota Lens SPEC）：上游缺失的数值一律 `None`，绝不默认填 0 ——
//! 0% 是合法的观测值，只能来自上游响应，不能由"没取到"伪装而来。

use serde::{Deserialize, Serialize};

/// 支持的额度来源。配置文件里 provider map 的 key 即 `as_str()`。
///
/// **serde 名必须与 `as_str()` 一致**：`rename_all = "kebab-case"` 会把
/// `OpenCodeGo` / `DeepSeek` / `OpenRouter` / `StepFun` 写成 `open-code-go`、
/// `deep-seek`、`open-router`、`step-fun` —— 与配置键 / CLI id 不同，故逐个显式
/// rename；单测钉住这条不变量（JSON 是 `quota-bar json` 的对外调试格式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    Mimo,
    MimoSg,
    #[serde(rename = "opencode-go")]
    OpenCodeGo,
    Antigravity,
    Codex,
    Claude,
    Grok,
    KimiCode,
    GlmZai,
    GlmZhipu,
    CommandCode,
    #[serde(rename = "deepseek")]
    DeepSeek,
    #[serde(rename = "openrouter")]
    OpenRouter,
    /// 阶跃星辰 StepFun（platform.stepfun.com）。
    #[serde(rename = "stepfun")]
    StepFun,
}

impl ProviderId {
    /// 展示/配置顺序：核心三家在前。
    pub const ALL: [ProviderId; 14] = [
        ProviderId::Mimo,
        ProviderId::MimoSg,
        ProviderId::OpenCodeGo,
        ProviderId::Antigravity,
        ProviderId::Codex,
        ProviderId::Claude,
        ProviderId::Grok,
        ProviderId::KimiCode,
        ProviderId::GlmZai,
        ProviderId::GlmZhipu,
        ProviderId::CommandCode,
        ProviderId::DeepSeek,
        ProviderId::OpenRouter,
        ProviderId::StepFun,
    ];

    /// 配置文件 / CLI 使用的稳定字符串 key。
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Mimo => "mimo",
            ProviderId::MimoSg => "mimo-sg",
            ProviderId::OpenCodeGo => "opencode-go",
            ProviderId::Antigravity => "antigravity",
            ProviderId::Codex => "codex",
            ProviderId::Claude => "claude",
            ProviderId::Grok => "grok",
            ProviderId::KimiCode => "kimi-code",
            ProviderId::GlmZai => "glm-zai",
            ProviderId::GlmZhipu => "glm-zhipu",
            ProviderId::CommandCode => "command-code",
            ProviderId::DeepSeek => "deepseek",
            ProviderId::OpenRouter => "openrouter",
            ProviderId::StepFun => "stepfun",
        }
    }

    pub fn parse(s: &str) -> Option<ProviderId> {
        let canonical = match s {
            "chatgpt" => "codex",
            "step" => "stepfun",
            _ => s,
        };
        ProviderId::ALL.into_iter().find(|p| p.as_str() == canonical)
    }

    /// 面板/菜单里的完整展示名。
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderId::Mimo => "小米 MiMo",
            ProviderId::MimoSg => "小米 MiMo 新加坡",
            ProviderId::OpenCodeGo => "OpenCode Go",
            ProviderId::Antigravity => "Google Antigravity",
            ProviderId::Codex => "ChatGPT / Codex (OpenAI)",
            ProviderId::Claude => "Claude",
            ProviderId::Grok => "Grok",
            ProviderId::KimiCode => "Kimi Code",
            ProviderId::GlmZai => "GLM Z.AI",
            ProviderId::GlmZhipu => "GLM 智谱",
            ProviderId::CommandCode => "Command Code",
            ProviderId::DeepSeek => "DeepSeek",
            ProviderId::OpenRouter => "OpenRouter",
            ProviderId::StepFun => "阶跃星辰 StepFun",
        }
    }

    /// 任务栏条上的短标签（宽度预算有限，见 strip 布局）。
    pub fn short_name(self) -> &'static str {
        match self {
            ProviderId::Mimo | ProviderId::MimoSg => "MiMo",
            ProviderId::OpenCodeGo => "GO",
            ProviderId::Antigravity => "AGY",
            ProviderId::Codex => "Codex",
            ProviderId::Claude => "Claude",
            ProviderId::Grok => "Grok",
            ProviderId::KimiCode => "Kimi",
            ProviderId::GlmZai => "Z.AI",
            ProviderId::GlmZhipu => "智谱",
            ProviderId::CommandCode => "Cmd",
            ProviderId::DeepSeek => "DS",
            ProviderId::OpenRouter => "OR",
            ProviderId::StepFun => "Step",
        }
    }

    /// 该 provider 的 API key 环境变量候选（优先级从高到低，dsh 插件同款）。
    pub fn env_key_names(self) -> &'static [&'static str] {
        match self {
            ProviderId::Mimo => &[
                "XIAOMI_MIMO_API_KEY",
                "MIMO_API_KEY",
                "MI_MIMO_API_KEY",
                "XIAOMI_API_KEY",
            ],
            ProviderId::MimoSg => &[
                "XIAOMI_MIMO_SG_API_KEY",
                "MIMO_SG_API_KEY",
                "MI_MIMO_SG_API_KEY",
                "MIMO_API_KEY_SG",
            ],
            ProviderId::OpenCodeGo => &["OPENCODE_GO_API_KEY", "OPENCODE_API_KEY"],
            ProviderId::DeepSeek => &["DEEPSEEK_API_KEY"],
            ProviderId::OpenRouter => &["OPENROUTER_API_KEY"],
            ProviderId::StepFun => &[],
            ProviderId::GlmZai | ProviderId::GlmZhipu => &["GLM_API_KEY", "ZHIPU_API_KEY"],
            ProviderId::KimiCode => &["KIMI_API_KEY", "MOONSHOT_API_KEY"],
            // OAuth 类（Antigravity/Codex/Claude/Grok）没有静态 API key。
            ProviderId::Antigravity
            | ProviderId::Codex
            | ProviderId::Claude
            | ProviderId::Grok
            | ProviderId::CommandCode => &[],
        }
    }
}

/// 凭据形态（「添加供应商」菜单据此分流：API key / 平台 Cookie / OAuth 登录）。
/// 与 `env_key_names` 的空表语义独立——command-code 虽在空表（env 走
/// `env_fallback`），本质是 Bearer key 类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    Cookie,
    OAuth,
}

impl ProviderId {
    /// 该 provider 的凭据形态：MiMo 系 / StepFun = 平台 Cookie（前者 Bearer 端点
    /// 已下线、后者 API key 查不了额度）；Antigravity / Codex = OAuth 登录；
    /// 其余全部 = API key。
    pub fn credential_kind(self) -> CredentialKind {
        match self {
            ProviderId::Mimo | ProviderId::MimoSg | ProviderId::StepFun => CredentialKind::Cookie,
            ProviderId::Antigravity | ProviderId::Codex => CredentialKind::OAuth,
            _ => CredentialKind::ApiKey,
        }
    }

    /// 该 provider 是否走 OAuth 账号表（多账号会话；见 `oauth::uses_account_store`）。
    pub fn uses_oauth_accounts(self) -> bool {
        matches!(self.credential_kind(), CredentialKind::OAuth)
    }

    /// Cookie 类 provider 的**取 Cookie 指引**（输入框提示行；各站路径不同）。
    pub fn cookie_source_hint(self) -> &'static str {
        match self {
            ProviderId::Mimo | ProviderId::MimoSg => {
                "platform.xiaomimimo.com → F12 → Network → 复制任意 /api/ 请求的 Cookie 请求头；留空取消。"
            }
            ProviderId::StepFun => {
                "platform.stepfun.com → F12 → Application → Cookies → 复制 Oasis-Token 的值；留空取消。"
            }
            _ => "复制平台 Cookie 请求头；留空取消。",
        }
    }
}

/// 三个展示窗口（与 dsh 插件 badge 顺序一致：5小时 / 周 / 月）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowSlot {
    Rolling,
    Weekly,
    Monthly,
}

impl WindowSlot {
    pub const ALL: [WindowSlot; 3] = [WindowSlot::Rolling, WindowSlot::Weekly, WindowSlot::Monthly];

    pub fn label(self) -> &'static str {
        match self {
            WindowSlot::Rolling => "5小时",
            WindowSlot::Weekly => "周",
            WindowSlot::Monthly => "月",
        }
    }
}

/// 一个窗口的观测值。全部 Option：缺失即未知。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct UsageWindow {
    /// 已用百分比 0–100（"0% 3% 63%" 里的数字）。
    pub percent: Option<f64>,
    /// 重置时刻（unix 秒）。
    pub resets_at: Option<i64>,
    /// 距重置秒数（上游直接给倒计时或由 resets_at 推算）。
    pub reset_in_sec: Option<f64>,
    /// 上游状态串（如 `rate-limited`）。
    pub status: Option<String>,
}

impl UsageWindow {
    /// 测试与后续批次构造用（面板/告警的便捷入口）。
    #[allow(dead_code)]
    pub fn new_percent(pct: f64) -> Self {
        Self {
            percent: Some(pct),
            ..Default::default()
        }
    }

    /// 已用百分比，钳到 0–100（上游偶发越界值不外溢到显示层）。
    pub fn pct(&self) -> Option<f64> {
        self.percent.map(|p| p.clamp(0.0, 100.0))
    }

    /// 上游限流状态（如 OpenCode `rate-limited`，面板可显示"限流"）。
    #[allow(dead_code)]
    pub fn rate_limited(&self) -> bool {
        self.status.as_deref() == Some("rate-limited")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoneyUnit {
    Cny,
    Usd,
    Credits,
}

impl MoneyUnit {
    pub fn format(self, amount: f64) -> String {
        match self {
            MoneyUnit::Cny => format!("¥{amount:.2}"),
            MoneyUnit::Usd => format!("${amount:.2}"),
            MoneyUnit::Credits => {
                // credits 常为整数；带小数时不显示无意义的 .00
                if amount.fract().abs() < 1e-9 {
                    format!("{} cr", amount as i64)
                } else {
                    format!("{amount:.2} cr")
                }
            }
        }
    }
}

/// 金额类额度（余额）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Money {
    pub amount: f64,
    pub unit: MoneyUnit,
}

impl Money {
    pub fn format(&self) -> String {
        self.unit.format(self.amount)
    }
}

/// 单个 provider 的一次观测结果（面板一行 = 一个 snapshot）。
///
/// `row_key`：条/存储层的行标识。默认空 = 归一为 `provider.as_str()`；
/// 多账号 provider（Antigravity）为每个账号生成独立行：
/// `"{provider}:{account_id}"`（account_id 为24位hex，不含`:`，前缀解析无歧义）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderSnapshot {
    pub provider: ProviderId,
    /// 行键（见类型文档）；空串经 [`ProviderSnapshot::row_key`] 归一。
    #[serde(default)]
    pub row_key: String,
    /// 账号提示（Antigravity 邮箱等），条上不显示、面板里显示。
    pub account_label: Option<String>,
    /// 会员档位（GLM level、Kimi membership 等）。
    pub level: Option<String>,
    /// 套餐**已过期**（上游明确告知时置位：MiMo `tokenPlan/detail.expired`、
    /// StepFun `subscription.expired_at` 已过）。显示层据此隐藏整行
    /// （见 `poll::filter_expired_plans` + `Settings::hide_expired_plans`）；
    /// `quota-bar json` 仍照常输出（调试可见）。
    pub plan_expired: bool,
    pub rolling: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    pub monthly: Option<UsageWindow>,
    /// OpenCode Go 专用：超额度后改扣 Zen 余额。
    pub use_balance: bool,
    /// 金额类余额（能取到才显示）。
    pub balance: Option<Money>,
    /// 错误码（`no-key` / `http` / `api` / `exec` …）。
    pub error: Option<String>,
    /// 错误详情（展示给用户）。
    pub message: Option<String>,
    /// 观测时刻（unix 秒）。
    pub fetched_at: u64,
}

impl ProviderSnapshot {
    pub fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            row_key: String::new(),
            account_label: None,
            level: None,
            plan_expired: false,
            rolling: None,
            weekly: None,
            monthly: None,
            use_balance: false,
            balance: None,
            error: None,
            message: None,
            fetched_at: now_sec(),
        }
    }

    /// 构造一个仅含错误的快照（凭证缺失、请求失败等）。
    pub fn error(provider: ProviderId, code: &str, message: impl Into<String>) -> Self {
        let mut s = Self::new(provider);
        s.error = Some(code.to_string());
        s.message = Some(message.into());
        s
    }

    /// 行键（空串归一为 provider key）。
    pub fn row_key(&self) -> &str {
        if self.row_key.is_empty() {
            self.provider.as_str()
        } else {
            &self.row_key
        }
    }

    /// 从行键反解所属 provider（`antigravity:abcd…` → Antigravity；
    /// 解析失败回落到字段值，保证旧数据/手改数据不 panic）。
    pub fn row_provider(&self) -> ProviderId {
        self.row_key
            .split(':')
            .next()
            .and_then(ProviderId::parse)
            .unwrap_or(self.provider)
    }

    /// 多账号行的账号 id（`{provider}:{account_id}` 的后半段；单行 → None）。
    pub fn row_account_id(&self) -> Option<&str> {
        let key = self.row_key();
        key.split_once(':').map(|(_, id)| id)
    }

    pub fn window(&self, slot: WindowSlot) -> Option<&UsageWindow> {
        match slot {
            WindowSlot::Rolling => self.rolling.as_ref(),
            WindowSlot::Weekly => self.weekly.as_ref(),
            WindowSlot::Monthly => self.monthly.as_ref(),
        }
    }

    /// 消耗最严重（已用百分比最高）的窗口 —— 任务栏条上显示的那个数字。
    pub fn worst_used(&self) -> Option<(WindowSlot, f64)> {
        WindowSlot::ALL
            .into_iter()
            .filter_map(|slot| self.window(slot).and_then(|w| w.pct()).map(|p| (slot, p)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// 是否拿到了套餐窗口或余额（区分"有内容"和"只有错误/空"）。
    pub fn has_data(&self) -> bool {
        self.rolling.is_some() || self.weekly.is_some() || self.monthly.is_some() || self.balance.is_some()
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

pub fn now_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys_roundtrip_and_order_is_stable() {
        for id in ProviderId::ALL {
            assert_eq!(ProviderId::parse(id.as_str()), Some(id));
            assert!(!id.display_name().is_empty());
            assert!(!id.short_name().is_empty());
        }
        // config 顺序 = ALL 顺序：核心三家在前
        let mut sorted = ProviderId::ALL;
        sorted.sort();
        assert_eq!(sorted[0], ProviderId::Mimo);
        assert_eq!(sorted[1], ProviderId::MimoSg);
        assert_eq!(sorted[2], ProviderId::OpenCodeGo);
        assert_eq!(sorted[3], ProviderId::Antigravity);
    }

    #[test]
    fn chatgpt_alias_uses_codex_oauth_and_stable_storage() {
        let id = ProviderId::parse("chatgpt").unwrap();
        assert_eq!(id, ProviderId::Codex);
        assert_eq!(id.credential_kind(), CredentialKind::OAuth);
        assert_eq!(id.as_str(), "codex");
        assert_eq!(ProviderId::parse("step"), Some(ProviderId::StepFun));
    }

    /// serde 名 == `as_str()`（配置键 / CLI id / JSON 输出三处同一个串）。
    #[test]
    fn serde_name_matches_as_str_for_every_provider() {
        for id in ProviderId::ALL {
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", id.as_str()),
                "{id:?} 的 serde 名与 as_str() 不一致（JSON 输出会对不上配置键）"
            );
            let back: ProviderId = serde_json::from_str(&json).unwrap();
            assert_eq!(back, id);
        }
    }

    #[test]
    fn missing_values_stay_none_and_worst_ignores_them() {
        let mut s = ProviderSnapshot::new(ProviderId::Mimo);
        // 没取到 ≠ 0%：worst 不应把 None 当 0 混进来
        assert_eq!(s.worst_used(), None);
        assert!(!s.has_data());

        s.monthly = Some(UsageWindow::new_percent(63.0));
        s.weekly = Some(UsageWindow::new_percent(3.0));
        // rolling 缺失 → 不参与"最严重"计算（而不是当成 0%）
        assert_eq!(s.worst_used(), Some((WindowSlot::Monthly, 63.0)));
        assert!(s.has_data());
    }

    #[test]
    fn worst_clamps_out_of_range_upstream_values() {
        let mut s = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        s.rolling = Some(UsageWindow {
            percent: Some(350.0),
            ..Default::default()
        });
        assert_eq!(s.worst_used(), Some((WindowSlot::Rolling, 100.0)));
        let mut s2 = ProviderSnapshot::new(ProviderId::OpenCodeGo);
        s2.weekly = Some(UsageWindow {
            percent: Some(-4.0),
            ..Default::default()
        });
        assert_eq!(s2.worst_used().unwrap().1, 0.0);
    }

    #[test]
    fn money_formats_by_unit() {
        assert_eq!(Money { amount: 12.34, unit: MoneyUnit::Cny }.format(), "¥12.34");
        assert_eq!(Money { amount: 5.0, unit: MoneyUnit::Usd }.format(), "$5.00");
        assert_eq!(Money { amount: 42.0, unit: MoneyUnit::Credits }.format(), "42 cr");
        assert_eq!(Money { amount: 42.5, unit: MoneyUnit::Credits }.format(), "42.50 cr");
    }

    #[test]
    fn snapshot_serde_keeps_none_as_none() {
        let s = ProviderSnapshot::error(ProviderId::Mimo, "no-key", "未配置 API Key");
        let json = serde_json::to_string(&s).unwrap();
        let back: ProviderSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rolling, None);
        assert_eq!(back.error.as_deref(), Some("no-key"));
        // 缺字段的旧 JSON 也能解析（UsageWindow 全 #[serde(default)]）
        let w: UsageWindow = serde_json::from_str(r#"{"percent":12.5}"#).unwrap();
        assert_eq!(w.percent, Some(12.5));
        assert_eq!(w.resets_at, None);
    }

    #[test]
    fn row_key_roundtrip_and_provider_parse() {
        // 默认空 → 归一 provider key；多账号键 → 前缀解析 + 账号 id 提取
        let s = ProviderSnapshot::new(ProviderId::Antigravity);
        assert_eq!(s.row_key(), "antigravity");
        assert_eq!(s.row_account_id(), None);

        let mut multi = ProviderSnapshot::new(ProviderId::Antigravity);
        multi.row_key = "antigravity:983960aba4e3fcf0bd4b2221".into();
        assert_eq!(multi.row_key(), "antigravity:983960aba4e3fcf0bd4b2221");
        assert_eq!(multi.row_provider(), ProviderId::Antigravity);
        assert_eq!(multi.row_account_id(), Some("983960aba4e3fcf0bd4b2221"));

        // 手改垃圾键不 panic：前缀解析失败 → 回落字段 provider
        let mut junk = ProviderSnapshot::new(ProviderId::Mimo);
        junk.row_key = "not-a-provider:xx".into();
        assert_eq!(junk.row_provider(), ProviderId::Mimo);

        // serde：缺字段旧数据 → 空串（归一逻辑接管）：用完整快照序列化后剔除该字段模拟
        let mut base = ProviderSnapshot::new(ProviderId::Antigravity);
        base.error = Some("auth".into());
        let mut v = serde_json::to_value(&base).unwrap();
        v.as_object_mut().unwrap().remove("row_key");
        let back: ProviderSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(back.row_key, "");
        assert_eq!(back.row_key(), "antigravity");
    }

    /// 凭据形态分流：MiMo 系 = Cookie，AGY/Codex = OAuth，其余 = API key
    /// （含被 env 空表归为 OAuth 类但实为 Bearer 的 command-code）。
    #[test]
    fn credential_kind_routes_by_provider() {
        use CredentialKind::*;
        assert_eq!(ProviderId::Mimo.credential_kind(), Cookie);
        assert_eq!(ProviderId::MimoSg.credential_kind(), Cookie);
        assert_eq!(ProviderId::Antigravity.credential_kind(), OAuth);
        assert_eq!(ProviderId::Codex.credential_kind(), OAuth);
        assert_eq!(ProviderId::OpenCodeGo.credential_kind(), ApiKey);
        assert_eq!(ProviderId::CommandCode.credential_kind(), ApiKey);
        assert_eq!(ProviderId::DeepSeek.credential_kind(), ApiKey);
        // 未接入 fetch 的两家按 API key 存储（与 CLI setkey 语义一致）
        assert_eq!(ProviderId::Claude.credential_kind(), ApiKey);
        assert_eq!(ProviderId::Grok.credential_kind(), ApiKey);
        // 全表覆盖：全表都能判型
        for id in ProviderId::ALL {
            let _ = id.credential_kind();
        }
        // OAuth 账号表判定与凭据形态一致
        assert!(ProviderId::Antigravity.uses_oauth_accounts());
        assert!(ProviderId::Codex.uses_oauth_accounts());
        assert!(!ProviderId::Mimo.uses_oauth_accounts());
        assert!(!ProviderId::DeepSeek.uses_oauth_accounts());
    }

    #[test]
    fn rate_limited_status_detected() {
        let w = UsageWindow {
            status: Some("rate-limited".into()),
            percent: Some(100.0),
            ..Default::default()
        };
        assert!(w.rate_limited());
    }
}
