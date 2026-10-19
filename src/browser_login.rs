//! Dedicated Chromium login window. Only this app's temporary profile is read.
use crate::model::{ProviderId, ProviderSnapshot};
use crate::providers::QuotaProvider;
use serde_json::{Value, json};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn browser_path() -> Option<PathBuf> {
    for (root, relative) in [
        ("ProgramFiles(x86)", "Microsoft/Edge/Application/msedge.exe"),
        ("ProgramFiles", "Microsoft/Edge/Application/msedge.exe"),
        ("LOCALAPPDATA", "Microsoft/Edge/Application/msedge.exe"),
        ("ProgramFiles", "Google/Chrome/Application/chrome.exe"),
        ("LOCALAPPDATA", "Google/Chrome/Application/chrome.exe"),
    ] {
        if let Some(root) = std::env::var_os(root) {
            let path = PathBuf::from(root).join(relative);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn platform(id: ProviderId) -> Result<(&'static str, &'static str), String> {
    match id {
        ProviderId::Mimo | ProviderId::MimoSg => Ok((
            "https://platform.xiaomimimo.com/",
            "platform.xiaomimimo.com",
        )),
        ProviderId::StepFun => Ok(("https://platform.stepfun.com/", "platform.stepfun.com")),
        _ => Err("该供应商不使用网页登录".into()),
    }
}

fn cookie_header(result: &Value, host: &str) -> Option<String> {
    let cookies = result.get("cookies")?.as_array()?;
    let fields: Vec<String> = cookies
        .iter()
        .filter_map(|c| {
            let domain = c.get("domain")?.as_str()?.trim_start_matches('.');
            if host != domain && !host.ends_with(&format!(".{domain}")) {
                return None;
            }
            // Reject unrelated public suffix cookies and header injection.
            if !domain.contains('.') {
                return None;
            }
            let name = c.get("name")?.as_str()?;
            let value = c.get("value")?.as_str()?;
            if name.is_empty()
                || name.contains(['\r', '\n', ';', '='])
                || value.contains(['\r', '\n', ';'])
            {
                return None;
            }
            Some(format!("{name}={value}"))
        })
        .collect();
    (!fields.is_empty()).then(|| fields.join("; "))
}

fn read_cookies(http: &ureq::Agent, port: u16, url: &str, host: &str) -> Option<String> {
    let targets: Value = http
        .get(&format!("http://127.0.0.1:{port}/json/list"))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    let ws_url = targets
        .as_array()?
        .iter()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))?
        .get("webSocketDebuggerUrl")?
        .as_str()?;
    if !ws_url.starts_with(&format!("ws://127.0.0.1:{port}/")) {
        return None;
    }
    let stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().ok()?,
        Duration::from_secs(2),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let (mut ws, _) = tungstenite::client(ws_url, stream).ok()?;
    ws.send(tungstenite::Message::Text(
        json!({
            "id": 1, "method": "Network.getCookies", "params": { "urls": [url] }
        })
        .to_string()
        .into(),
    ))
    .ok()?;
    // Limit unsolicited events; do not consume unbounded browser output.
    for _ in 0..32 {
        let msg = ws.read().ok()?;
        if let tungstenite::Message::Text(text) = msg {
            let value: Value = serde_json::from_str(&text).ok()?;
            if value.get("id").and_then(Value::as_u64) == Some(1) {
                let header = cookie_header(value.get("result")?, host);
                let _ = ws.close(None);
                return header;
            }
        }
    }
    None
}

fn verify(id: ProviderId, cookie: &str, http: &ureq::Agent) -> ProviderSnapshot {
    match id {
        ProviderId::StepFun => {
            crate::providers::stepfun::StepFunProvider::new(Some(cookie.into()), None).fetch(http)
        }
        ProviderId::MimoSg => {
            crate::providers::mimo::MimoProvider::sg(None, Some(cookie.into())).fetch(http)
        }
        _ => crate::providers::mimo::MimoProvider::new(None, Some(cookie.into())).fetch(http),
    }
}

/// Validate against the real platform before storing credentials. Saving happens
/// after validation and reloads settings so concurrent menu changes are retained.
pub fn login_blocking(id: ProviderId) -> Result<(), String> {
    let (url, host) = platform(id)?;
    let browser =
        browser_path().ok_or("未找到 Edge 或 Chrome，请使用菜单中的「手动粘贴 Cookie」")?;
    let profile = crate::secrets::data_dir()
        .join("browser-login")
        .join(format!(
            "{}-{}",
            id.as_str(),
            crate::oauth::pkce::random_token(12)
        ));
    std::fs::create_dir_all(&profile).map_err(|_| "无法创建登录浏览器配置目录")?;
    // Port 0 lets Chromium select its own unused loopback port.
    let mut child = std::process::Command::new(browser)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0")
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-mode")
        .arg(format!("--app={url}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| "无法打开网页登录窗口，请使用「手动粘贴 Cookie」")?;
    let http = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(3))
        .build();
    let quota_http = crate::providers::make_agent();
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut last_candidate = String::new();
    let mut last_check = Instant::now() - Duration::from_secs(15);
    let mut outcome = Err("网页登录超时，请重试；也可手动粘贴 Cookie".into());
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            outcome = Err("登录窗口已关闭，已取消登录".into());
            break;
        }
        if !profile.join("DevToolsActivePort").is_file()
            && deadline.saturating_duration_since(Instant::now()) < Duration::from_secs(275) {
            outcome = Err("无法连接网页登录窗口，请改用「添加供应商 → 手动粘贴 Cookie」".into());
            break;
        }
        let port = std::fs::read_to_string(profile.join("DevToolsActivePort"))
            .ok()
            .and_then(|s| s.lines().next()?.parse::<u16>().ok());
        if let Some(cookie) = port.and_then(|port| read_cookies(&http, port, url, host)) {
            let plausible = match id {
                ProviderId::StepFun => crate::providers::stepfun::device_id_of(
                    &crate::providers::stepfun::normalize_token(&cookie),
                )
                .is_some(),
                _ => cookie.contains("serviceToken="),
            };
            if plausible
                && (cookie != last_candidate || last_check.elapsed() >= Duration::from_secs(15))
            {
                last_candidate = cookie.clone();
                last_check = Instant::now();
                let snapshot = verify(id, &cookie, &quota_http);
                if snapshot.error.is_none() || snapshot.balance.is_some() {
                    let saved = if id == ProviderId::StepFun {
                        crate::providers::stepfun::normalize_token(&cookie)
                    } else {
                        cookie
                    };
                    let mut settings = crate::config::Settings::load();
                    let label = format!("网页登录 {}", settings.accounts(id).len() + 1);
                    outcome = crate::accounts::add(&mut settings, id, &label, &saved).map(|_| {
                        settings.save();
                    });
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    // Close only the browser process created for this login, never the user's browser.
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an installed Chromium browser; uses only a local fixture"]
    fn dedicated_browser_captures_cookie_without_manual_copy() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = done.clone();
        let server = std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nSet-Cookie: quota_fixture=captured; Path=/; HttpOnly\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let temp_root = std::env::temp_dir().canonicalize().unwrap();
        let profile = temp_root.join(format!(
            "quota-bar-cdp-test-{}",
            crate::oauth::pkce::random_token(12)
        ));
        std::fs::create_dir_all(&profile).unwrap();
        let mut browser = std::process::Command::new(browser_path().expect("Edge or Chrome"))
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--remote-debugging-port=0")
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let http = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut captured = false;
        while Instant::now() < deadline {
            let port = std::fs::read_to_string(profile.join("DevToolsActivePort"))
                .ok()
                .and_then(|s| s.lines().next()?.parse::<u16>().ok());
            if let Some(cookie) = port.and_then(|p| read_cookies(&http, p, &url, "127.0.0.1")) {
                if cookie == "quota_fixture=captured" {
                    captured = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = browser.kill();
        let _ = browser.wait();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = server.join();
        // Verify the resolved target before deleting a Windows directory tree.
        if let Ok(resolved) = profile.canonicalize() {
            if resolved.starts_with(&temp_root) && resolved != temp_root {
                let _ = std::fs::remove_dir_all(resolved);
            }
        }
        assert!(
            captured,
            "CDP should read the HttpOnly fixture cookie from our dedicated profile"
        );
    }

    #[test]
    fn cookie_capture_is_scoped_to_the_platform_and_rejects_injection() {
        let result = json!({"cookies": [
            {"domain":"platform.stepfun.com", "name":"Oasis-Token", "value":"fixture"},
            {"domain":".stepfun.com", "name":"shared", "value":"ok"},
            {"domain":"evilstepfun.com", "name":"unrelated", "value":"no"},
            {"domain":".com", "name":"suffix", "value":"no"},
            {"domain":"platform.stepfun.com", "name":"bad", "value":"bad\r\nInjected: header"}
        ]});
        assert_eq!(
            cookie_header(&result, "platform.stepfun.com").as_deref(),
            Some("Oasis-Token=fixture; shared=ok")
        );
        assert!(cookie_header(&result, "platform.xiaomimimo.com").is_none());
    }
}
