//! GLM Coding Plan 两个渠道的 provider 结构体（Z.AI 国际站 / 智谱国内站）。
//! 抓取与解析全部在共享的 [`crate::providers::glm`]（`glm::fetch`）；
//! 本文件只负责：凭证组装、origin 常量（dsh `glm-zai.ts` / `glm-zhipu.ts` 端点）
//! 与 dsh 补充环境变量兜底（`model.rs::env_key_names` 没覆盖 ZAI 系名字）。

use crate::model::ProviderId;
use crate::providers::{glm, QuotaProvider};

/// GLM 额度 provider：`GlmZai` / `GlmZhipu` 共用一个结构体，只差 id 与 origin。
pub struct GlmProvider {
    id: ProviderId,
    /// 渠道 origin（dsh 端点为准：Z.AI `https://api.z.ai`、智谱 `https://open.bigmodel.cn`）。
    pub origin: &'static str,
    api_key: Option<String>,
}

impl GlmProvider {
    /// 任意 origin（测试用；`fetch` 会先过 `glm::origin_allowed` 白名单）。
    pub fn new(id: ProviderId, origin: &'static str, api_key: Option<String>) -> Self {
        Self { id, origin, api_key }
    }

    /// Z.AI 国际站（dsh `glm-zai.ts`：`https://api.z.ai/api/monitor/usage/quota/limit`）。
    pub fn zai(api_key: Option<String>) -> Self {
        Self::new(ProviderId::GlmZai, "https://api.z.ai", api_key)
    }

    /// 智谱国内站（dsh `glm-zhipu.ts`：`https://open.bigmodel.cn/api/monitor/usage/quota/limit`）。
    pub fn zhipu(api_key: Option<String>) -> Self {
        Self::new(ProviderId::GlmZhipu, "https://open.bigmodel.cn", api_key)
    }
}

/// dsh `refs` 补充候选（`resolve_api_key` 未命中时由 registry 兜底调用）。
/// - zai：`ZAI_API_KEY` / `Z_AI_API_KEY` / `GLM_ZAI_API_KEY`（dsh glm-zai.ts）
/// - zhipu：`ZHIPU_API_KEY` / `ZHIPUAI_API_KEY` / `GLM_API_KEY` / `BIGMODEL_API_KEY`
pub fn env_fallback(id: ProviderId) -> Option<String> {
    let names: &[&str] = match id {
        ProviderId::GlmZai => &["ZAI_API_KEY", "Z_AI_API_KEY", "GLM_ZAI_API_KEY"],
        ProviderId::GlmZhipu => &[
            "ZHIPU_API_KEY",
            "ZHIPUAI_API_KEY",
            "GLM_API_KEY",
            "BIGMODEL_API_KEY",
        ],
        _ => &[],
    };
    names
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

impl QuotaProvider for GlmProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    fn fetch(&self, http: &ureq::Agent) -> crate::model::ProviderSnapshot {
        glm::fetch(http, self.id, self.origin, self.api_key.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_glm_constructors_pin_dsh_endpoints() {
        let zai = GlmProvider::zai(Some("k".into()));
        assert_eq!(zai.id(), ProviderId::GlmZai);
        assert_eq!(zai.origin, "https://api.z.ai");
        let zhipu = GlmProvider::zhipu(Some("k".into()));
        assert_eq!(zhipu.id(), ProviderId::GlmZhipu);
        assert_eq!(zhipu.origin, "https://open.bigmodel.cn");
    }

    #[test]
    fn ext_glm_no_key_is_reported_before_any_network() {
        // key 缺失 → fetch 在发请求之前就返回 no-key（这里不会也不应触网）。
        let http = crate::providers::make_agent();
        let snap = GlmProvider::zai(None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        assert!(snap.message.as_deref().unwrap().contains("setkey glm-zai"));
        let snap = GlmProvider::zhipu(None).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
        assert!(snap.message.as_deref().unwrap().contains("ZHIPU_API_KEY"));
        // 空白 key 同样视为未配置。
        let snap = GlmProvider::zhipu(Some("   ".into())).fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("no-key"));
    }

    #[test]
    fn ext_glm_rejects_origin_outside_allowlist_without_network() {
        let http = crate::providers::make_agent();
        let snap = GlmProvider::new(
            ProviderId::GlmZhipu,
            "https://evil.example.com",
            Some("k".into()),
        )
        .fetch(&http);
        assert_eq!(snap.error.as_deref(), Some("endpoint"));
        assert!(snap
            .message
            .as_deref()
            .unwrap()
            .contains("evil.example.com"));
    }
}
