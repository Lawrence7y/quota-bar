//! Provider 注册表与抓取执行。
//!
//! 新增一家 = 新增一个模块 + 在 `registry()` 挂一行（构造时解析好凭证，
//! 网络阶段不再触碰 Settings —— 这样抓取可以在锁外并行跑）。
//!
//! 阶段2（核心三家）先挂：MiMo、OpenCode Go、Antigravity。
//! 阶段8（扩展批次）再挂：智谱/DeepSeek/OpenRouter/Codex/Claude/Kimi/Grok/Command Code。

pub mod antigravity;
pub mod codex;
pub mod command_code;
pub mod deepseek;
pub mod glm;
pub mod kimi_code;
pub mod mimo;
pub mod opencode_go;
pub mod openrouter;
pub mod stepfun;
pub mod zhipu;

use std::time::Duration;

use crate::config::Settings;
use crate::model::{ProviderId, ProviderSnapshot};
use crate::secrets;

/// 一个可抓取的额度来源。实现必须**不阻塞超过 HTTP 超时**，
/// 且把一切失败装进 `snapshot.error/message`（永不 panic/Err）。
pub trait QuotaProvider: Send {
    fn id(&self) -> ProviderId;
    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot;

    /// 一轮抓取可产出**多行**快照（多账号 provider 覆写本方法；
    /// 其余 provider 零改动 —— 默认 = 单账号 `[fetch]`）。
    /// 返回 Vec 的顺序即条/存储侧的行序。
    fn fetch_multi(&self, http: &ureq::Agent) -> Vec<ProviderSnapshot> {
        vec![self.fetch(http)]
    }
}

/// 账号包装：把单账号 provider 的快照钉上**账号行键**与展示名
/// （`"{provider}:{account_id}"`，与 OAuth 多账号行同构）。
/// API key / Cookie 类的多账号就是「同一种 provider × N 个凭据」——
/// 每个账号一个实例，条/面板/账号白名单因此天然按账号分行。
struct AccountProvider {
    inner: Box<dyn QuotaProvider>,
    account_id: String,
    label: Option<String>,
}

impl AccountProvider {
    fn wrap(inner: Box<dyn QuotaProvider>, account_id: String, label: Option<String>) -> Self {
        Self {
            inner,
            account_id,
            label,
        }
    }
}

impl QuotaProvider for AccountProvider {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn fetch(&self, http: &ureq::Agent) -> ProviderSnapshot {
        self.stamp(self.inner.fetch(http))
    }

    fn fetch_multi(&self, http: &ureq::Agent) -> Vec<ProviderSnapshot> {
        // 内层若自己就是多行 provider（OAuth 类），逐行盖账号上下文会串号 ——
        // 只有单行 provider 才走本包装（registry 侧保证），这里按行原样盖。
        self.inner
            .fetch_multi(http)
            .into_iter()
            .map(|s| self.stamp(s))
            .collect()
    }
}

impl AccountProvider {
    fn stamp(&self, mut snap: ProviderSnapshot) -> ProviderSnapshot {
        snap.row_key = format!("{}:{}", self.id().as_str(), self.account_id);
        if snap.account_label.is_none() {
            snap.account_label = self.label.clone();
        }
        snap
    }
}

/// API key 解析：keyring `secret_ref` 优先，环境变量兜底（dsh 同款变量名）。
pub fn resolve_api_key(settings: &Settings, id: ProviderId) -> Option<String> {
    if let Some(r) = settings.secret_ref(id) {
        if let Some(v) = secrets::get_opt(r) {
            return Some(v);
        }
    }
    for name in id.env_key_names() {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 读取指定 keyring 引用（多账号：每个账号自己的 ref）。
pub fn resolve_ref(secret_ref: &str) -> Option<String> {
    secrets::get_opt(secret_ref)
}

/// Cookie 解析：keyring `provider/{id}/cookie`（`quota-bar setcookie` 写入）优先，环境变量兜底。
pub fn resolve_cookie(id: ProviderId, env_names: &[&str]) -> Option<String> {
    if let Some(v) = secrets::get_opt(&secrets::provider_cookie_ref(id.as_str())) {
        return Some(v);
    }
    for name in env_names {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// MiMo 系平台 Cookie 的环境变量候选（registry 解析与 `is_configured` 判定
/// **共用**，防两处名单漂移）；非 Cookie 类 provider 返回空表。
pub fn cookie_env_names(id: ProviderId) -> &'static [&'static str] {
    match id {
        ProviderId::Mimo => &["MIMO_USAGE_COOKIE"],
        ProviderId::MimoSg => &["MIMO_SG_USAGE_COOKIE", "MIMO_USAGE_COOKIE"],
        ProviderId::StepFun => &["STEPFUN_OASIS_TOKEN", "STEPFUN_COOKIE"],
        _ => &[],
    }
}

/// 旧单账号 Cookie 槽的 keyring 引用（`quota-bar setcookie <id>` 写入的位置；
/// 多账号的每账号引用是它的子路径 `…/cookie/<账号id>`）。
pub fn legacy_cookie_ref(id: ProviderId) -> String {
    secrets::provider_cookie_ref(id.as_str())
}

/// 旧单账号 Cookie 槽的**隐式账号 id**（= `legacy_account_id(旧槽引用)`，
/// 与 `accounts::list` 共用 → 行键稳定、显示白名单可寻址）。
pub fn legacy_cookie_account_id(id: ProviderId) -> String {
    crate::config::legacy_account_id(&legacy_cookie_ref(id))
}

/// 旧单账号 Cookie 槽里是否有值（Cookie 类 provider 的兼容入口）。
pub fn legacy_cookie_value(id: ProviderId) -> Option<String> {
    secrets::get_opt(&legacy_cookie_ref(id))
}

/// 一个已配置账号的凭据（API key / Cookie 类；OAuth 类走 `oauth` 账号表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountCredential {
    /// 账号键（行键后半段；与 `Settings::accounts` / 显示白名单同源）。
    pub id: String,
    /// 展示名（用户填的 label，或展示层兜底的 key 尾号）。
    pub label: Option<String>,
    /// keyring 引用（会话类 provider 续期后据此把新凭据写回；环境变量兜底时为空）。
    pub secret_ref: String,
    /// 该账号的 API key（Cookie 类为 None）。
    pub api_key: Option<String>,
    /// 该账号的平台 Cookie（API key 类为 None）。
    pub cookie: Option<String>,
}

/// 该 provider 已配置的账号凭据（顺序 = 配置顺序）：
/// 1. `settings.providers[id].accounts` 逐条解析（keyring 值缺失的条目**跳过**）；
/// 2. 一条都没有且是 Cookie 类 → 旧单账号槽 `provider/{id}/cookie`
///    （`quota-bar setcookie` 写的，物化成隐式账号；`id = legacy_cookie_account_id`）；
/// 3. 仍没有 → **环境变量**单账号（`id = "env"`），保持"免配置兜底"的旧行为；
/// 4. OAuth 类恒空（账号在 `oauth` 存储里）。
pub fn credentials(settings: &Settings, id: ProviderId) -> Vec<AccountCredential> {
    if id.uses_oauth_accounts() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let cookie_kind = id.credential_kind() == crate::model::CredentialKind::Cookie;
    for acc in settings.accounts(id) {
        let value = resolve_ref(&acc.secret_ref);
        let Some(value) = value else { continue };
        out.push(AccountCredential {
            id: acc.id.clone(),
            label: Some(acc.label.clone()).filter(|l| !l.is_empty()),
            secret_ref: acc.secret_ref.clone(),
            api_key: if cookie_kind { None } else { Some(value.clone()) },
            cookie: if cookie_kind { Some(value) } else { None },
        });
    }
    if out.is_empty() && cookie_kind {
        if let Some(v) = legacy_cookie_value(id) {
            out.push(AccountCredential {
                id: legacy_cookie_account_id(id),
                label: None,
                secret_ref: legacy_cookie_ref(id),
                api_key: None,
                cookie: Some(v),
            });
        }
    }
    if out.is_empty() {
        // 环境变量兜底（与 is_configured 同源）：cookie 类优先平台 Cookie 变量。
        if cookie_kind {
            if let Some(v) = resolve_cookie(id, cookie_env_names(id)) {
                out.push(AccountCredential {
                    id: "env".into(),
                    label: Some("环境变量".into()),
                    // 环境变量兜底：没有 keyring 条目可写回（续期不落盘）
                    secret_ref: String::new(),
                    api_key: None,
                    cookie: Some(v),
                });
            }
        } else if let Some(v) = env_api_key(settings, id) {
            out.push(AccountCredential {
                id: "env".into(),
                label: Some("环境变量".into()),
                secret_ref: String::new(),
                api_key: Some(v),
                cookie: None,
            });
        }
    }
    out
}

/// 环境变量兜底取 API key：`env_key_names` + 各家 registry 分支自己的名单
/// （kimi / command-code / 智谱），与 [`is_configured`] 同源。
pub fn env_api_key(settings: &Settings, id: ProviderId) -> Option<String> {
    if let Some(v) = resolve_api_key(settings, id) {
        return Some(v);
    }
    match id {
        ProviderId::KimiCode => kimi_code::env_fallback(),
        ProviderId::CommandCode => command_code::env_fallback(),
        ProviderId::GlmZai | ProviderId::GlmZhipu => zhipu::env_fallback(id),
        _ => None,
    }
}

/// 该 provider 是否**已添加**（凭据可解析）：
/// - API key / Cookie 类：`credentials()` 非空（配置账号或环境变量兜底）；
/// - OAuth 类：`oauth` 账号表非空；
/// - registry 构建分支里另有 env 兜底的几家（kimi / command-code / 智谱）
///   与其保持同源。
///
/// 三处口径共用本判定：菜单「添加供应商」打 ✓、供应商管理只列已添加的、
/// [`display_list`] 门控条/面板/托盘实际显示的行——**没添加过的 provider 既不
/// 进设置列表也不上条**（不再出"未配置"错误芯片）。
pub fn is_configured(settings: &Settings, id: ProviderId) -> bool {
    if id.uses_oauth_accounts() {
        return !crate::oauth::list_accounts(id).is_empty();
    }
    if !credentials(settings, id).is_empty() {
        return true;
    }
    // registry 构建分支里经 env_fallback 兜底的几家（判定与其保持同源）。
    env_api_key(settings, id).is_some()
}

/// 条/面板/托盘**实际显示**的 provider = 启用 ∩ 已添加。
/// `is_cfg` 注入判定（单测不碰真实 keyring）；生产入口见 [`display_list`]。
pub fn display_list_with(
    settings: &Settings,
    is_cfg: impl Fn(ProviderId) -> bool,
) -> Vec<ProviderId> {
    settings
        .enabled_providers()
        .into_iter()
        .filter(|id| is_cfg(*id))
        .collect()
}

/// 生产入口：按 [`is_configured`] 门控的显示集合（`strip::content` 每拍调用）。
pub fn display_list(settings: &Settings) -> Vec<ProviderId> {
    display_list_with(settings, |id| is_configured(settings, id))
}

/// 统一 HTTP 客户端参数（对应 dsh 的 curl -m 10 / 15s 壳超时）。
pub fn make_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .timeout_connect(Duration::from_secs(10))
        .user_agent(concat!("quota-bar/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// 按当前配置构建启用的 provider 列表（凭证在此解析，网络在 `run` 里）。
///
/// **多账号**：API key / Cookie 类每家按已配置账号逐个建实例（每个实例带账号
/// 行键，见 [`AccountProvider`]）；OAuth 类（Antigravity / Codex）只建一个实例
/// ——它们自己在 `fetch_multi` 里逐账号出行。未配置的 provider 也建一个"空
/// 凭据"实例（CLI `json` 里仍能看到原因行；显示层由 `display_list` 门掉）。
pub fn registry(settings: &Settings) -> Vec<Box<dyn QuotaProvider>> {
    let mut list: Vec<Box<dyn QuotaProvider>> = Vec::new();
    for id in ProviderId::ALL {
        if !settings.is_enabled(id) {
            continue;
        }
        if id.uses_oauth_accounts() {
            // OAuth：账号表在 `oauth` 存储里，provider 自己逐账号出行。
            match id {
                ProviderId::Antigravity => {
                    list.push(Box::new(antigravity::AntigravityProvider::new()))
                }
                ProviderId::Codex => list.push(Box::new(codex::CodexProvider)),
                _ => {}
            }
            continue;
        }
        let creds = credentials(settings, id);
        if creds.is_empty() {
            // 未配置：保留一条原因行（`no-key` / `no-cookie`），显示层不会看到。
            list.push(build_single(id, None, None, None));
            continue;
        }
        for cred in creds {
            let secret_ref = (!cred.secret_ref.is_empty()).then_some(cred.secret_ref.as_str());
            let inner = build_single(id, cred.api_key, cred.cookie, secret_ref);
            list.push(Box::new(AccountProvider::wrap(inner, cred.id, cred.label)));
        }
    }
    list
}

/// 单账号 provider 构造（凭据已解析；`None` = 未配置）。
/// `cookie_ref` = 该账号的 keyring 引用（会话类 provider 续期后写回用）。
fn build_single(
    id: ProviderId,
    api_key: Option<String>,
    cookie: Option<String>,
    cookie_ref: Option<&str>,
) -> Box<dyn QuotaProvider> {
    match id {
        ProviderId::Mimo => Box::new(mimo::MimoProvider::new(api_key, cookie)),
        ProviderId::MimoSg => Box::new(mimo::MimoProvider::sg(api_key, cookie)),
        ProviderId::OpenCodeGo => Box::new(opencode_go::OpenCodeGoProvider::new(api_key)),
        ProviderId::KimiCode => Box::new(kimi_code::KimiCodeProvider::new(
            api_key,
            std::env::var("KIMI_AUTH_COOKIE")
                .ok()
                .filter(|c| !c.trim().is_empty()),
        )),
        ProviderId::GlmZai => Box::new(zhipu::GlmProvider::zai(api_key)),
        ProviderId::GlmZhipu => Box::new(zhipu::GlmProvider::zhipu(api_key)),
        ProviderId::CommandCode => Box::new(command_code::CommandCodeProvider::new(api_key)),
        ProviderId::DeepSeek => Box::new(deepseek::DeepSeekProvider::new(api_key)),
        ProviderId::OpenRouter => Box::new(openrouter::OpenRouterProvider::new(api_key)),
        // 未接入 fetch 的两家：占位错误行（CLI json 可见"尚未接入"）。
        ProviderId::Claude | ProviderId::Grok => {
            Box::new(PlaceholderProvider { id })
        }
        // StepFun 的凭据是会话 cookie（API key 查不了额度）。
        ProviderId::StepFun => Box::new(stepfun::StepFunProvider::new(
            cookie,
            cookie_ref.map(|r| r.to_string()),
        )),
        // OAuth 类不走这里（registry 上游已分流）。
        ProviderId::Antigravity | ProviderId::Codex => Box::new(PlaceholderProvider { id }),
    }
}

/// 尚未接入 fetch 的 provider：恒定"未接入"错误行（绝不 panic）。
struct PlaceholderProvider {
    id: ProviderId,
}

impl QuotaProvider for PlaceholderProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    fn fetch(&self, _http: &ureq::Agent) -> ProviderSnapshot {
        ProviderSnapshot::error(self.id, "unsupported", "该供应商尚未接入额度查询")
    }
}

/// 并行执行一轮抓取，按构建顺序（= `ProviderId::ALL` 顺序）返回结果。
/// 每家走 `fetch_multi`（多账号 provider 逐账号多行，普通 provider 默认单行），
/// 行序 = 构建序 × 各家 Vec 序；worker panic → 占位错误行（row_key 留空，
/// 存储侧按归一 key 归并）；某家返回空 Vec → 补一条 `empty` 错误行防"整行消失"。
pub fn run(providers: Vec<Box<dyn QuotaProvider>>) -> Vec<ProviderSnapshot> {
    let base = make_agent();
    let jobs: Vec<_> = providers
        .into_iter()
        .map(|p| {
            let id = p.id();
            let agent = base.clone();
            (id, std::thread::spawn(move || p.fetch_multi(&agent)))
        })
        .collect();
    // join 顺序 = 构建顺序，保证 rows 顺序稳定。
    let mut rows: Vec<ProviderSnapshot> = Vec::new();
    for (id, handle) in jobs {
        match handle.join() {
            Ok(mut batch) => {
                if batch.is_empty() {
                    batch.push(ProviderSnapshot::error(id, "empty", "无结果"));
                }
                rows.extend(batch);
            }
            Err(_) => rows.push(ProviderSnapshot::error(id, "panic", "抓取线程异常")),
        }
    }
    rows
}

/// 一次性查询（CLI `json` 与轮询共用入口）。
pub fn query_all(settings: &Settings) -> Vec<ProviderSnapshot> {
    run(registry(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_builds_enabled_core_only() {
        // 测试环境不应读到真实凭证，全部应以"未配置"错误行返回而不 panic
        let settings = Settings::default();
        let providers = registry(&settings);
        assert_eq!(providers.len(), 3); // mimo + opencode-go + antigravity（mimo-sg 默认关）
        assert_eq!(providers[0].id(), ProviderId::Mimo);
        assert_eq!(providers[1].id(), ProviderId::OpenCodeGo);
        assert_eq!(providers[2].id(), ProviderId::Antigravity);
    }

    #[test]
    fn cookie_env_names_match_registry_expectations() {
        assert_eq!(cookie_env_names(ProviderId::Mimo), &["MIMO_USAGE_COOKIE"]);
        assert_eq!(
            cookie_env_names(ProviderId::MimoSg),
            &["MIMO_SG_USAGE_COOKIE", "MIMO_USAGE_COOKIE"]
        );
        // 非 Cookie 类 provider 恒空表（is_configured 的 Cookie 分支只对 MiMo 系开启）。
        assert!(cookie_env_names(ProviderId::DeepSeek).is_empty());
        assert!(cookie_env_names(ProviderId::Antigravity).is_empty());
    }

    /// 显示集合 = 启用 ∩ 已添加（判定注入 → 恒确定，不碰 keyring）。
    #[test]
    fn display_list_is_enabled_and_configured_intersection() {
        let mut s = Settings::default(); // 启用核心三家
        // 判定：只有 mimo/opencode-go 已添加 → AGY 被门掉
        let list = display_list_with(&s, |id| {
            matches!(id, ProviderId::Mimo | ProviderId::OpenCodeGo)
        });
        assert_eq!(list, vec![ProviderId::Mimo, ProviderId::OpenCodeGo]);
        // 一家都没添加 → 空表（条隐藏、托盘保留）
        assert!(display_list_with(&s, |_| false).is_empty());
        // 判定全过 → 全部启用项（顺序 = ALL 序）
        assert_eq!(
            display_list_with(&s, |_| true),
            vec![
                ProviderId::Mimo,
                ProviderId::OpenCodeGo,
                ProviderId::Antigravity
            ]
        );
        // 已添加但停用 → 不显示；停用后再启用一个已添加的 → 出现
        s.set_enabled(ProviderId::Mimo, false);
        assert_eq!(
            display_list_with(&s, |id| id == ProviderId::Mimo),
            Vec::<ProviderId>::new()
        );
        s.set_enabled(ProviderId::DeepSeek, true);
        assert_eq!(
            display_list_with(&s, |id| id == ProviderId::DeepSeek),
            vec![ProviderId::DeepSeek]
        );
    }

    #[test]
    fn resolve_key_prefers_keyring_then_env() {
        let mut settings = Settings::default();
        settings.add_account(
            ProviderId::DeepSeek,
            "",
            "provider/deepseek/api-key".into(),
        );
        // keyring 里没存值 → 落到环境变量（测试里通常也没设 → None，不 panic）
        let _ = resolve_api_key(&settings, ProviderId::DeepSeek);
        // 无 secret_ref、无 env → None
        assert_eq!(resolve_api_key(&settings, ProviderId::OpenCodeGo), None);
    }

    /// 账号凭据解析：配置账号逐个成条（keyring 无值 → 跳过）；空表回落 env 兜底。
    #[test]
    fn credentials_resolve_per_account_and_fall_back_to_env() {
        let mut settings = Settings::default();
        // 配置了两个账号，但 keyring 里都没有值 → 全部跳过 → 回落 env（测试机无该变量 → 空）
        settings.add_account(ProviderId::OpenCodeGo, "主", "provider/opencode-go/api-key/1".into());
        settings.add_account(ProviderId::OpenCodeGo, "备", "provider/opencode-go/api-key/2".into());
        let creds = credentials(&settings, ProviderId::OpenCodeGo);
        assert!(
            creds.iter().all(|c| c.id != "env"),
            "有配置账号时不再回落 env"
        );
        // OAuth 类恒空（账号在 oauth 存储里）
        assert!(credentials(&settings, ProviderId::Antigravity).is_empty());
        assert!(credentials(&settings, ProviderId::Codex).is_empty());
        // 无配置账号 + 无 env → 空（未配置）
        assert!(credentials(&settings, ProviderId::OpenRouter).is_empty());
    }

    /// 账号包装：行键带账号后缀、展示名盖上（多账号行的统一形态）。
    #[test]
    fn account_provider_stamps_row_key_and_label() {
        let inner = Box::new(mimo::MimoProvider::new(None, None));
        let wrapped = AccountProvider::wrap(inner, "3f2a1b0c".into(), Some("主号".into()));
        let snap = wrapped.fetch(&make_agent());
        assert_eq!(snap.row_key, "mimo:3f2a1b0c");
        assert_eq!(snap.row_account_id(), Some("3f2a1b0c"));
        assert_eq!(snap.row_provider(), ProviderId::Mimo);
        assert_eq!(snap.account_label.as_deref(), Some("主号"));
        // fetch_multi 默认单行，同样被盖上账号上下文
        let rows = wrapped.fetch_multi(&make_agent());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row_key, "mimo:3f2a1b0c");
    }

    #[test]
    fn default_fetch_multi_is_single_fetch_row() {
        // 非多账号 provider 不覆写 fetch_multi → 默认 = [fetch]（结构等价）。
        // 用无 key/无 cookie 的 mimo：fetch 在触网前即返回"未配置"错误行
        //（具体 code 归 mimo 侧管，这里只测结构，不硬编码）。
        let p = mimo::MimoProvider::new(None, None);
        let agent = make_agent();
        let multi = p.fetch_multi(&agent);
        let single = p.fetch(&agent);
        assert_eq!(multi.len(), 1, "默认实现恰一行");
        assert!(
            multi[0].error.is_some(),
            "无凭证 mock 必为错误行（不发网络）"
        );
        // 逐字段等价（fetched_at 取同一秒时全等；跨秒只比关键字段，避免 flaky）
        assert_eq!(multi[0].provider, single.provider);
        assert_eq!(multi[0].error, single.error);
        assert_eq!(multi[0].message, single.message);
        assert_eq!(multi[0].row_key, single.row_key, "默认行 row_key 为空（归一）");
        assert!(multi[0].row_key.is_empty(), "单账号行不带多账号键");
    }

    #[test]
    fn run_flattens_batches_and_backfills_empty_rows() {
        // 用 mock provider 验证 run 的 flatten/防空/panic 兜底（不发网络）。
        struct Batch {
            id: ProviderId,
            rows: Vec<ProviderSnapshot>,
            panic: bool,
        }
        impl QuotaProvider for Batch {
            fn id(&self) -> ProviderId {
                self.id
            }
            fn fetch(&self, _http: &ureq::Agent) -> ProviderSnapshot {
                ProviderSnapshot::error(self.id, "mock", "单行")
            }
            fn fetch_multi(&self, _http: &ureq::Agent) -> Vec<ProviderSnapshot> {
                if self.panic {
                    panic!("mock worker panic");
                }
                self.rows.clone()
            }
        }
        let mk = |id: ProviderId, n: usize, key: &str| -> Box<dyn QuotaProvider> {
            let rows = (0..n)
                .map(|i| {
                    let mut s = ProviderSnapshot::error(id, "ok-row", format!("row{i}"));
                    s.row_key = key.to_string();
                    s
                })
                .collect();
            Box::new(Batch { id, rows, panic: false })
        };
        // 三家：2行 + 0行（防空补 empty）+ 1行；顺序保持
        let out = run(vec![
            mk(ProviderId::Mimo, 2, "mimo"),
            mk(ProviderId::OpenCodeGo, 0, ""),
            mk(ProviderId::Antigravity, 1, "antigravity:abc"),
        ]);
        assert_eq!(out.len(), 4, "2 + 补的1条 empty + 1");
        assert_eq!(out[0].row_key, "mimo");
        assert_eq!(out[1].row_key, "mimo");
        assert_eq!(out[2].error.as_deref(), Some("empty"), "空 Vec → empty 兜底行");
        assert_eq!(out[2].message.as_deref(), Some("无结果"));
        assert_eq!(out[2].provider, ProviderId::OpenCodeGo);
        assert_eq!(out[3].row_key, "antigravity:abc");
        assert_eq!(out[3].row_account_id(), Some("abc"));

        // panic → 占位行（row_key 空 = 归一 key）
        let panicker: Box<dyn QuotaProvider> = Box::new(Batch {
            id: ProviderId::Mimo,
            rows: Vec::new(),
            panic: true,
        });
        let out = run(vec![panicker]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].error.as_deref(), Some("panic"));
        assert!(out[0].row_key.is_empty(), "panic 兜底行 row_key 留空");
        assert_eq!(out[0].row_key(), "mimo", "空 → 归一 provider key");
    }
}
