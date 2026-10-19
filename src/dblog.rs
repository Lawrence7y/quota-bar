//! 滚动文件日志（%LOCALAPPDATA%\QuotaBar\debug.log）——发布版常开，必须有尺寸上限。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// 单个日志上限；超过轮转成 `debug.log.1`（只留最近一份历史）。
const MAX_LOG_BYTES: u64 = 1 << 20; // 1 MiB
const SIZE_CHECK_EVERY: u32 = 64;

static WRITES: AtomicU32 = AtomicU32::new(0);

pub fn path() -> PathBuf {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("QuotaBar").join("debug.log")
}

pub fn log(msg: &str) {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[allow(clippy::manual_is_multiple_of)]
    let check_size = WRITES.fetch_add(1, Ordering::Relaxed) % SIZE_CHECK_EVERY == 0;
    if check_size {
        rotate_if_too_big(&p);
    }
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) else {
        return;
    };
    let _ = writeln!(f, "{} {msg}", unix_secs());
}

fn rotate_if_too_big(p: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(p) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let old = p.with_extension("log.1");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(p, &old);
}

fn unix_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_target_sits_next_to_the_log() {
        let p = PathBuf::from(r"C:\x\QuotaBar\debug.log");
        let rotated = p.with_extension("log.1");
        assert_eq!(
            rotated.file_name().and_then(|f| f.to_str()),
            Some("debug.log.1")
        );
        assert_eq!(rotated.parent(), p.parent());
    }
}
