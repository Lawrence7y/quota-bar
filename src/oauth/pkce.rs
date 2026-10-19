//! PKCE（RFC 7636）与随机会话令牌。
//! 移植 dsh `src/auth/pkce.ts`（MIT）：32 字节随机 verifier → base64url，
//! S256 challenge = base64url(sha256(verifier))。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};

/// 一对 PKCE 凭据：verifier 只留在本地，challenge 发给授权端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// 高熵秘密（43–128 字符），token 交换时作 `code_verifier`。
    pub verifier: String,
    /// `code_challenge`（S256 摘要），授权时带上。
    pub challenge: String,
}

/// Base64url 无填充（RFC 4648 §5）。
pub fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 随机字节（base64url）。`getrandom` 失败时用时间熵回退填充（不 panic ——
/// 正常环境 getrandom 在 Windows 上不会失败，这里只是纪律兜底）。
fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    if getrandom::getrandom(&mut buf).is_ok() {
        return buf;
    }
    // 回退：时间戳 + 地址空间混合的 xorshift（仅应急）。
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | (&buf as *const _ as u64);
    for slot in buf.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *slot = (seed >> 24) as u8;
    }
    buf
}

/// URL 安全随机令牌（OAuth `state` 默认 16 字节）。
pub fn random_token(bytes: usize) -> String {
    base64url(&random_bytes(bytes))
}

/// S256 挑战：`base64url(sha256(ASCII(verifier)))`（无填充）。
pub fn s256_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url(&hasher.finalize())
}

/// 生成一对 PKCE（32 字节 → 43 字符 verifier，落在 RFC 7636 §4.1 的 43–128 内）。
pub fn create_pkce() -> PkcePair {
    let verifier = random_token(32);
    let challenge = s256_challenge(&verifier);
    PkcePair {
        verifier,
        challenge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B 官方向量。
    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn rfc7636_appendix_b_vector() {
        assert_eq!(s256_challenge(RFC_VERIFIER), RFC_CHALLENGE);
    }

    #[test]
    fn created_pair_is_valid_s256_and_in_length_bounds() {
        let pair = create_pkce();
        // verifier 43–128 字符且是 base64url 字母表（无 +/=/填充）
        assert!(pair.verifier.len() >= 43 && pair.verifier.len() <= 128);
        assert!(pair
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // challenge 恒 43 字符（sha256 = 32 字节 → base64url）
        assert_eq!(pair.challenge.len(), 43);
        // challenge 必须是 verifier 的 S256
        assert_eq!(s256_challenge(&pair.verifier), pair.challenge);
    }

    #[test]
    fn random_tokens_are_unique_url_safe() {
        let a = random_token(16);
        let b = random_token(16);
        assert_eq!(a.len(), 22); // 16 字节 → 22 字符
        assert_ne!(a, b, "随机令牌不应重复");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
