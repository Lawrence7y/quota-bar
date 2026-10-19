//! 凭据存储：Windows Credential Manager 为主，DPAPI 本地加密文件为回退。
//!
//! - 主路径：`keyring`（服务名 `quota-bar`）。
//! - 回退路径：Credential Manager 不可用时，当前用户 DPAPI（`CryptProtectData`，
//!   `CRYPTPROTECT_UI_FORBIDDEN`）加密后存 `%LOCALAPPDATA%\QuotaBar\secrets\<sha256>.bin`。
//! - `get` 先主路径，失败且回退文件存在时读回退；`delete` 双删，忽略不存在。
//! - OAuth 会话（access/refresh token JSON）也走这里：`session/{provider}`。
//!   （会话是多字段结构，整体 JSON 序列化后存成一条凭据。）

use std::path::PathBuf;

/// secret_ref 形如 `provider/mimo/api-key`、`session/antigravity`。
pub fn put(secret_ref: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("拒绝保存空凭据".into());
    }
    match keyring_put(secret_ref, value) {
        Ok(()) => {
            // 主路径成功后清理可能残留的回退文件，避免两处并存。
            if let Ok(path) = fallback_file(secret_ref) {
                let _ = std::fs::remove_file(path);
            }
            Ok(())
        }
        Err(primary) => dpapi_put(secret_ref, value).map_err(|_| primary),
    }
}

pub fn get(secret_ref: &str) -> Result<String, String> {
    match keyring_get(secret_ref) {
        Ok(value) => Ok(value),
        Err(primary) => dpapi_get(secret_ref).map_err(|_| primary),
    }
}

pub fn delete(secret_ref: &str) -> Result<(), String> {
    let keyring_result = keyring_delete(secret_ref);
    let fallback_gone = match fallback_file(secret_ref) {
        Ok(path) => match std::fs::remove_file(path) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        },
        Err(_) => true,
    };
    if keyring_result.is_ok() || fallback_gone {
        Ok(())
    } else {
        keyring_result
    }
}

/// 便捷：读取，不存在时返回 None（而不是 Err —— "没配"不是错误）。
pub fn get_opt(secret_ref: &str) -> Option<String> {
    get(secret_ref).ok().filter(|v| !v.trim().is_empty())
}

pub fn provider_api_key_ref(provider: &str) -> String {
    format!("provider/{provider}/api-key")
}

/// 平台类 Cookie（MiMo 三接口 / 未来 Kimi 的 kimi-auth 等）。
pub fn provider_cookie_ref(provider: &str) -> String {
    format!("provider/{provider}/cookie")
}

pub fn session_ref(provider: &str) -> String {
    format!("session/{provider}")
}

fn keyring_put(secret_ref: &str, password: &str) -> Result<(), String> {
    let entry = keyring::Entry::new("quota-bar", secret_ref).map_err(|e| e.to_string())?;
    entry.set_password(password).map_err(|e| e.to_string())
}

fn keyring_get(secret_ref: &str) -> Result<String, String> {
    let entry = keyring::Entry::new("quota-bar", secret_ref).map_err(|e| e.to_string())?;
    entry.get_password().map_err(|e| e.to_string())
}

fn keyring_delete(secret_ref: &str) -> Result<(), String> {
    let entry = keyring::Entry::new("quota-bar", secret_ref).map_err(|e| e.to_string())?;
    entry.delete_credential().map_err(|e| e.to_string())
}

/// 回退文件路径：对 secret_ref 单向哈希，路径不泄露 provider/profile。
fn fallback_file(secret_ref: &str) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"quota-bar-dpapi-fallback-v1");
    hasher.update([0]);
    hasher.update(secret_ref.as_bytes());
    let name: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(data_dir().join("secrets").join(format!("{name}.bin")))
}

pub fn data_dir() -> PathBuf {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("QuotaBar")
}

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

fn dpapi_put(secret_ref: &str, password: &str) -> Result<(), String> {
    let path = fallback_file(secret_ref)?;
    let blob = dpapi_protect(password.as_bytes())?;
    atomic_write(&path, &blob)
}

fn dpapi_get(secret_ref: &str) -> Result<String, String> {
    let path = fallback_file(secret_ref)?;
    let blob = std::fs::read(path).map_err(|e| e.to_string())?;
    let plain = dpapi_unprotect(&blob)?;
    String::from_utf8(plain).map_err(|_| "DPAPI 回退数据损坏".to_string())
}

/// 当前用户 DPAPI 加密（`CRYPTPROTECT_UI_FORBIDDEN` 无弹窗；默认即当前用户 scope）。
fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if plain.is_empty() {
        return Err("拒绝加密空凭据".into());
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR(std::ptr::null()),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|e| format!("DPAPI 加密失败：{e}"))?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err("DPAPI 加密返回空结果".into());
        }
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as _));
        Ok(bytes)
    }
}

fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if blob.is_empty() {
        return Err("DPAPI 回退数据为空".into());
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|e| format!("DPAPI 解密失败：{e}"))?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err("DPAPI 解密返回空结果".into());
        }
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as _));
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_secret() {
        assert!(put("provider/x/api-key", "   ").is_err());
    }

    #[test]
    fn fallback_path_is_hashed_not_plaintext() {
        let path = fallback_file("provider/mimo/api-key").unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(".bin"));
        assert!(!name.contains("mimo"));
        assert_eq!(name.len(), 64 + 4);
    }

    #[test]
    fn ref_helpers_shape() {
        assert_eq!(provider_api_key_ref("mimo"), "provider/mimo/api-key");
        assert_eq!(provider_cookie_ref("mimo"), "provider/mimo/cookie");
        assert_eq!(session_ref("antigravity"), "session/antigravity");
    }
}
