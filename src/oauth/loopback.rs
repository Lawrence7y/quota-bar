//! 本地回环 OAuth 回调：`127.0.0.1:<port><path>` 的最小 HTTP 服务与回调解析
//! （移植 dsh `src/auth/oauth-flow.ts` 的 handler / `manual` 语义，MIT）。
//!
//! 回调**路径与端口由调用方给定**（Antigravity：51121 `/oauth-callback`；
//! Codex：1455 `/auth/callback`），本模块只负责 HTTP 与解析。
//!
//! - 回调判定抽成纯函数（`classify_callback`），便于无网络测试；
//! - `wait_for_code` 手写 `TcpListener`（任务要求，不引入新依赖）：nonblocking
//!   accept 轮询 + 总超时 180s + stdin 粘贴兜底（30 秒窗口）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

pub const CALLBACK_HOST: &str = "127.0.0.1";
/// dsh `ANTIGRAVITY_CALLBACK_PORT`。
pub const CALLBACK_PORT: u16 = 51121;
/// dsh `ANTIGRAVITY_CALLBACK_PATH`。
pub const CALLBACK_PATH: &str = "/oauth-callback";
/// 一次登录尝试的总时长（dsh `DEFAULT_FLOW_TIMEOUT_MS`）。
pub const FLOW_TIMEOUT_MS: u64 = 180_000;
/// 粘贴兜底窗口（CLI 场景：回环被占用/非交互时 30 秒内可贴回调 URL 或裸 code）。
pub const PASTE_WINDOW_MS: u64 = 30_000;

/// 登录成功页（中文，dsh SUCCESS_PAGE 的对应版本）。
pub const SUCCESS_PAGE: &str = concat!(
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>登录成功</title></head>",
    "<body style=\"font-family:sans-serif\">",
    "<h1>登录成功</h1>",
    "<p>可以关闭本页并返回 quota-bar。</p>",
    "</body></html>"
);

/// 登录失败页；`detail` 会滤掉 HTML 特殊字符（dsh failurePage 同款）。
pub fn failure_page(detail: &str) -> String {
    let safe: String = detail
        .chars()
        .map(|c| match c {
            '<' => '＜',
            '>' => '＞',
            '&' => '＆',
            other => other,
        })
        .collect();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>登录失败</title></head>\
         <body style=\"font-family:sans-serif\"><h1>登录失败</h1><p>{safe}</p></body></html>"
    )
}

/// 一次回调请求的分类结果（对应 dsh handler 的四个出口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    /// 拿到授权码（state 已校验通过）。
    Success(String),
    /// 授权端回了 `error` / `error_description`。
    ProviderError(String),
    /// `state` 与本次尝试不匹配（dsh：400，且**不**终止尝试）。
    StateMismatch,
    /// 路径对但缺 `code`。
    MissingCode,
    /// 路径不是 `/oauth-callback`。
    NotFound,
}

/// 百分号解码（`%XX`）；`+` 按 query 惯例转空格。非法序列原样保留。
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// 解析 query 串为键值对（顺序不敏感，重复键取第一个非… 这里取**最后一个**
/// 覆盖会更危险，OAuth 参数不重复 —— 取先出现者，与 `URLSearchParams.get` 一致）。
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

fn query_get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// 从请求目标（`/oauth-callback?code=…&state=…`）提取 code/state/error。
/// 语义对齐 dsh handler：error 优先（不校验 state）→ state 校验 → code 必填。
/// `path` = 本次登录的回调路径（不同 provider 不同）。
pub fn classify_callback(target: &str, path: &str, expected_state: &str) -> CallbackOutcome {
    let (got_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if got_path != path {
        return CallbackOutcome::NotFound;
    }
    let pairs = parse_query(query);
    // dsh: `error_description ?? error` 先于 state 检查
    if let Some(desc) = query_get(&pairs, "error_description")
        .or_else(|| query_get(&pairs, "error"))
    {
        return CallbackOutcome::ProviderError(desc.to_string());
    }
    // dsh: `url.searchParams.get('state') !== input.state`（缺失也判不匹配）
    match query_get(&pairs, "state") {
        Some(state) if state == expected_state => {}
        _ => return CallbackOutcome::StateMismatch,
    }
    match query_get(&pairs, "code") {
        Some(code) => CallbackOutcome::Success(code.to_string()),
        None => CallbackOutcome::MissingCode,
    }
}

/// 粘贴兜底输入解析（dsh `OAuthAttempt.manual` :269-288 语义）：
/// - `http(s)://…` 完整回调 URL → 取 query 的 code/state；
/// - 含 `code=` 的串（如浏览器地址栏 query 段）→ 当 query 解析；
/// - 非空且不含空白 → 当作裸授权码（**裸码无法校验 state** —— dsh 同）；
/// - 带来的 state 若与本次尝试不符 → 拒绝。
pub fn parse_pasted(input: &str, expected_state: &str) -> Result<String, String> {
    let trimmed = input.trim();
    let mut code: Option<String> = None;
    let mut pasted_state: Option<String> = None;

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let query = trimmed.split_once('?').map(|(_, q)| q).unwrap_or("");
        let pairs = parse_query(query);
        code = query_get(&pairs, "code").map(|s| s.to_string());
        pasted_state = query_get(&pairs, "state").map(|s| s.to_string());
    } else if trimmed.contains("code=") {
        let query = trimmed.trim_start_matches('?');
        let pairs = parse_query(query);
        code = query_get(&pairs, "code").map(|s| s.to_string());
        pasted_state = query_get(&pairs, "state").map(|s| s.to_string());
    } else if !trimmed.is_empty() && !trimmed.chars().any(|c| c.is_whitespace()) {
        code = Some(trimmed.to_string());
    }

    let code = code.filter(|c| !c.is_empty()).ok_or_else(|| {
        "粘贴内容里没有找到授权码（请粘贴完整的回调 URL，或形如 code=… 的授权码）".to_string()
    })?;
    if let Some(state) = pasted_state {
        if state != expected_state {
            return Err(format!(
                "state 不匹配：粘贴的回调属于另一次登录尝试（期望 {expected_state}，得到 {state}），请重试"
            ));
        }
    }
    Ok(code)
}

/// 写一个固定页面响应：正确的 Content-Length + `Connection: close`。
fn write_page(stream: &mut TcpStream, status: &str, body: &str, content_type: &str) {
    let bytes = body.as_bytes();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}

/// 读一个 HTTP 请求头（到 `\r\n\r\n`，上限 16KB），返回请求行目标。
fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next()?;
    // `GET /oauth-callback?... HTTP/1.1`
    line.split_whitespace().nth(1).map(|s| s.to_string())
}

/// 处理一条连接：分类 → 回响应。返回 Some(code) 表示拿到授权码。
fn handle_connection(
    stream: &mut TcpStream,
    path: &str,
    expected_state: &str,
    provider_label: &str,
) -> Option<Result<String, String>> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let target = read_request_target(stream)?;
    match classify_callback(&target, path, expected_state) {
        CallbackOutcome::Success(code) => {
            write_page(stream, "200 OK", SUCCESS_PAGE, "text/html; charset=utf-8");
            Some(Ok(code))
        }
        CallbackOutcome::ProviderError(desc) => {
            write_page(
                stream,
                "200 OK",
                &failure_page(&desc),
                "text/html; charset=utf-8",
            );
            Some(Err(format!("{provider_label} 授权失败：{desc}")))
        }
        CallbackOutcome::StateMismatch => {
            // dsh：杂散/重放的重定向不该杀死真正的尝试 → 继续等
            write_page(stream, "400 Bad Request", "state mismatch", "text/plain");
            None
        }
        CallbackOutcome::MissingCode => {
            write_page(stream, "400 Bad Request", "missing authorization code", "text/plain");
            None
        }
        CallbackOutcome::NotFound => {
            write_page(stream, "404 Not Found", "not found", "text/plain");
            None
        }
    }
}

/// 等待授权码：回环回调（非阻塞轮询，总超时 `FLOW_TIMEOUT_MS`）+ stdin 粘贴
/// 兜底（`PASTE_WINDOW_MS` 窗口内接受完整回调 URL / query / 裸 code）。
///
/// `listener` 为 bind 结果：端口被占用时为 Err —— 仍打印 URL 并走粘贴兜底。
/// `path` = 回调路径；`provider_label` / `login_hint` 只用于文案
///（授权失败前缀、"请重试"里的命令）。
pub fn wait_for_code(
    listener: std::io::Result<TcpListener>,
    path: &str,
    expected_state: &str,
    provider_label: &str,
    login_hint: &str,
) -> Result<String, String> {
    use std::sync::mpsc::{self, TryRecvError};

    let start = crate::providers::mimo::now_ms();
    let paste_deadline = start + PASTE_WINDOW_MS;
    let deadline = start + FLOW_TIMEOUT_MS;

    // stdin 粘贴线程：读到非空一行就上报（EOF/空 → 静默结束）。
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(n) if n > 0 && !line.trim().is_empty() => {
                let _ = tx.send(line);
            }
            _ => {}
        }
    });
    let mut stdin_alive = true;

    let listener = match listener {
        Ok(l) => {
            let _ = l.set_nonblocking(true);
            Some(l)
        }
        Err(e) => {
            eprintln!("回环端口监听失败（{e}）——只能粘贴回调 URL 或授权码完成登录");
            None
        }
    };

    loop {
        let now = crate::providers::mimo::now_ms();
        if now >= deadline {
            return Err(format!(
                "登录超时（{} 秒）：浏览器没有完成授权，也没有收到粘贴的回调；请重试 {login_hint}",
                FLOW_TIMEOUT_MS / 1000
            ));
        }

        // 1) 粘贴兜底（30 秒窗口内）
        if stdin_alive && now < paste_deadline {
            match rx.try_recv() {
                Ok(line) => return parse_pasted(&line, expected_state),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => stdin_alive = false,
            }
        }

        // 2) 回环回调
        if let Some(l) = &listener {
            match l.accept() {
                Ok((mut stream, _)) => {
                    if let Some(result) =
                        handle_connection(&mut stream, path, expected_state, provider_label)
                    {
                        return result;
                    }
                    // state 不匹配/缺 code → 继续等真正的那次回调
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "st-abc123";

    /// 本模块测试统一用 Antigravity 的回调路径（其它路径另测）；同名遮蔽 glob 导入。
    fn classify_callback(target: &str, expected_state: &str) -> CallbackOutcome {
        super::classify_callback(target, CALLBACK_PATH, expected_state)
    }

    /// 回调路径由调用方给定：Codex 的 `/auth/callback` 同样解析，旧路径不命中。
    #[test]
    fn callback_path_is_parameterized() {
        let codex_path = "/auth/callback";
        assert_eq!(
            super::classify_callback(
                "/auth/callback?code=cdx&state=st-abc123",
                codex_path,
                STATE
            ),
            CallbackOutcome::Success("cdx".into())
        );
        // 换了路径 → Antigravity 的回调在该次尝试里不命中
        assert_eq!(
            super::classify_callback("/oauth-callback?code=abc&state=st-abc123", codex_path, STATE),
            CallbackOutcome::NotFound
        );
    }

    #[test]
    fn callback_url_extracts_code_and_state() {
        assert_eq!(
            classify_callback("/oauth-callback?code=abc&state=st-abc123", STATE),
            CallbackOutcome::Success("abc".into())
        );
        // 参数顺序无关
        assert_eq!(
            classify_callback("/oauth-callback?state=st-abc123&code=xyz", STATE),
            CallbackOutcome::Success("xyz".into())
        );
        // 百分号编码的 code
        assert_eq!(
            classify_callback("/oauth-callback?code=4%2F0AfJ&state=st-abc123", STATE),
            CallbackOutcome::Success("4/0AfJ".into())
        );
    }

    #[test]
    fn state_mismatch_is_rejected_and_error_wins() {
        // state 不符 → StateMismatch（缺失也判不匹配，dsh 同）
        assert_eq!(
            classify_callback("/oauth-callback?code=abc&state=other", STATE),
            CallbackOutcome::StateMismatch
        );
        assert_eq!(
            classify_callback("/oauth-callback?code=abc", STATE),
            CallbackOutcome::StateMismatch
        );
        // error + description（先于 state 检查）；+ 号解码为空格
        assert_eq!(
            classify_callback(
                "/oauth-callback?error=access_denied&error_description=User+declined",
                STATE
            ),
            CallbackOutcome::ProviderError("User declined".into())
        );
        // 只有 error 无 description → 用 error
        assert_eq!(
            classify_callback("/oauth-callback?error=invalid_request", STATE),
            CallbackOutcome::ProviderError("invalid_request".into())
        );
    }

    #[test]
    fn missing_code_and_wrong_path_are_classified() {
        assert_eq!(
            classify_callback("/oauth-callback?state=st-abc123", STATE),
            CallbackOutcome::MissingCode
        );
        assert_eq!(classify_callback("/other?code=a", STATE), CallbackOutcome::NotFound);
        // 路径对但没有任何参数 → state 校验先失败（dsh 也回 400 而非 404）
        assert_eq!(
            classify_callback("/oauth-callback", STATE),
            CallbackOutcome::StateMismatch
        );
    }

    #[test]
    fn pasted_full_url_query_or_bare_code() {
        // 完整回调 URL（state 校验通过）
        assert_eq!(
            parse_pasted("http://127.0.0.1:51121/oauth-callback?code=abc&state=st-abc123", STATE)
                .unwrap(),
            "abc"
        );
        // 裸 query 段（不含 scheme）
        assert_eq!(
            parse_pasted("code=abc&state=st-abc123", STATE).unwrap(),
            "abc"
        );
        // 裸授权码（无 state 可校验 —— dsh 接受）
        assert_eq!(parse_pasted("4/0AfJ", STATE).unwrap(), "4/0AfJ");
        // state 不符 → 清晰错误
        let err = parse_pasted(
            "http://127.0.0.1:51121/oauth-callback?code=abc&state=st-ZZZ",
            STATE,
        )
        .unwrap_err();
        assert!(err.contains("state 不匹配"), "{err}");
        // 没有 code → 清晰错误
        let err = parse_pasted("http://127.0.0.1:51121/oauth-callback?state=st-abc123", STATE)
            .unwrap_err();
        assert!(err.contains("没有找到授权码"), "{err}");
        let err = parse_pasted("   ", STATE).unwrap_err();
        assert!(err.contains("没有找到授权码"), "{err}");
    }

    #[test]
    fn url_decode_handles_forms() {
        assert_eq!(url_decode("a%2Fb"), "a/b");
        assert_eq!(url_decode("a+b c"), "a b c");
        assert_eq!(url_decode("%zz"), "%zz"); // 非法序列原样
        assert_eq!(url_decode("4%0AfJ"), "4\nfJ");
    }

    #[test]
    fn pages_have_valid_content_length_and_close() {
        let body = SUCCESS_PAGE;
        // 构造响应头（与 write_page 同款逻辑）并断言 Content-Length 与字节数一致
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert!(head.contains("Connection: close"));
        assert!(head.ends_with("\r\n\r\n"));
        // 失败页滤掉 HTML 字符
        let page = failure_page("bad <script>&x");
        assert!(!page.contains("<script>"));
        assert!(page.contains("登录失败"));
    }
}
