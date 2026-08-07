//! Web 工具（文档 §17、§8.1 P1）。
//!
//! - `web_search`：DuckDuckGo HTML 端点（免费、无需 API key，社区 skills 通用
//!   方案，参考 pi-web-access 的无 key 思路）；结果只用于发现来源，
//!   不调用 LLM/Answers endpoint（§17：避免隐藏模型成本）。
//! - `web_fetch`：reqwest 限制 redirect、响应体大小和 timeout；HTML 用
//!   html2text 转换；正文有界（§8.4：48 KiB）。
//! - 不打开浏览器、不自动 fetch 全部结果、不调用 summary model（§17）。
//! - DDG 可能对异常流量返回人机验证页：此时明确报错，不静默降级（§17）。

use std::net::IpAddr;

use futures_util::StreamExt;
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use crate::tool::outcome::{ArtifactRef, ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

/// web_fetch 正文预算（§8.4：正文 48 KiB）。
pub const FETCH_BODY_BUDGET: usize = 48 * 1024;
/// HTML 原始体积上限（转换前）。
pub const FETCH_RAW_LIMIT: usize = 2 * 1024 * 1024;
/// web_search 响应体上限。
pub const SEARCH_RAW_LIMIT: usize = 2 * 1024 * 1024;
/// 请求超时。
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 浏览器 UA（DDG HTML 端点对无 UA/简单 UA 请求返回人机验证页）。
const DDG_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// DDG HTML 端点（免费、无需 API key）。
const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    pub query: String,
    /// 结果数（默认 5，上限 20）。
    #[serde(default = "default_count")]
    pub count: u32,
    /// freshness（"pd_1d"/"pd_1w"/"pd_1m"/"pd_1y";透传给 DDG 的 df 参数）。
    #[serde(default)]
    pub freshness: Option<String>,
    /// 限制搜索域（拼接为 site: 过滤）。
    #[serde(default)]
    pub domains: Option<Vec<String>>,
}

fn default_count() -> u32 {
    5
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WebFetchArgs {
    pub url: String,
}

/// 测试专用：允许访问私有地址（生产默认拒绝 SSRF）。
fn allow_private_web_targets() -> bool {
    if ALLOW_PRIVATE_WEB_TARGETS.load(std::sync::atomic::Ordering::SeqCst) {
        return true;
    }
    std::env::var("TPI_WEB_FETCH_ALLOW_PRIVATE")
        .map(|value| value == "1")
        .unwrap_or(false)
}

static ALLOW_PRIVATE_WEB_TARGETS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 集成测试专用：切换是否允许 fetch 私有地址（避免并行测试污染环境变量）。
#[doc(hidden)]
pub fn set_allow_private_web_targets_for_tests(allow: bool) {
    ALLOW_PRIVATE_WEB_TARGETS.store(allow, std::sync::atomic::Ordering::SeqCst);
}

fn web_fetch_client() -> reqwest::Client {
    let mut builder =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::custom(|attempt| {
            // P0-10：redirect 逐跳校验（此前 Policy::limited 自动跟随，
            // 私有/loopback 目标直接可达）。
            if let Err(error) = redirect_allowed(attempt.url()) {
                attempt.error(format!("blocked redirect target: {error}"))
            } else {
                attempt.follow()
            }
        }));
    if allow_private_web_targets() {
        builder = builder.no_proxy();
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// 校验 fetch 目标 URL（§17：仅 HTTP(S)，拒绝 loopback/私网/链路本地）。
pub fn validate_fetch_url(url_str: &str) -> Result<Url, String> {
    let url = Url::parse(url_str.trim()).map_err(|error| format!("invalid url: {error}"))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("unsupported scheme: {scheme}"));
    }
    let host = url.host_str().ok_or_else(|| "missing host".to_string())?;
    if !allow_private_web_targets() && is_blocked_host(host) {
        return Err(format!("blocked host: {host}"));
    }
    Ok(url)
}

fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // url crate 的 host_str() 对 IPv6 返回带括号形式（如 "[::1]"、
    // "[::ffff:7f00:1]"）；必须先去掉括号才能 parse 成 IpAddr——
    // 否则所有字面 IPv6（含 ::1、mapped-v6）都会绕过检查。
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return is_blocked_ip(ip);
    }
    // 字面 IPv6 可能带 []，Url 已解析 host 时不含括号。
    false
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            // P0-10：IPv4-mapped IPv6（::ffff:a.b.c.d）按映射的 v4 判定——
            // 此前只查 v6 字面范围，`::ffff:127.0.0.1` / `::ffff:192.168.1.1`
            // 直接放行（SSRF 绕过）。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(v4));
            }
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let segments = v6.segments();
            // link-local fe80::/10
            if (segments[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // unique local fc00::/7
            if (segments[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            false
        }
    }
}

/// P0-10：redirect 目标逐跳校验（policy 闭包与 web_fetch 共用）。
/// 校验 scheme 与 host；`allow_private_web_targets()` 开启时放行。
fn redirect_allowed(url: &Url) -> Result<(), String> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("unsupported scheme: {scheme}"));
    }
    let host = url.host_str().ok_or_else(|| "missing host".to_string())?;
    if !allow_private_web_targets() && is_blocked_host(host) {
        return Err(format!("blocked host: {host}"));
    }
    Ok(())
}

async fn read_bounded_bytes(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut total = 0usize;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("read_failed: {error}"))?;
        total += chunk.len();
        if total > limit {
            return Err(format!("body_too_large: exceeded {limit} bytes"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// web_search（§17：只用于发现来源；结果摘要不是最终证据）。
///
/// 免费方案：DuckDuckGo HTML 端点（无需 API key，零配置）。
/// DDG 可能对异常流量返回人机验证页——此时返回明确错误，不静默降级。
pub async fn web_search(args: WebSearchArgs, _ctx: &ToolContext) -> ToolOutcome {
    let mut query = args.query.clone();
    if let Some(domains) = &args.domains {
        for domain in domains {
            query.push_str(&format!(" site:{domain}"));
        }
    }
    let count = args.count.clamp(1, 20);
    let mut url = format!("{DDG_HTML_ENDPOINT}?q={}", urlencode(&query));
    if let Some(df) = args.freshness.as_deref().and_then(ddg_freshness) {
        url.push_str(&format!("&df={df}"));
    }

    let client = reqwest::Client::new();
    let response = match client
        .get(&url)
        .header(reqwest::header::USER_AGENT, DDG_USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/html")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return ToolOutcome::failed(
                "web_search",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_search\nerror: request_failed\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let status = response.status();
    let body_bytes = match read_bounded_bytes(response, SEARCH_RAW_LIMIT).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("body_too_large") => {
            return ToolOutcome::failed(
                "web_search",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_search\nerror: body_too_large\n\n响应体超过 {SEARCH_RAW_LIMIT} 字节限制。"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
        Err(error) => {
            return ToolOutcome::failed(
                "web_search",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_search\nerror: read_failed\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };
    let body = String::from_utf8_lossy(&body_bytes).into_owned();
    if !status.is_success() {
        return ToolOutcome::failed(
            "web_search",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!("status: failed\ntool: web_search\nerror: http_{status}\n\n{body}"),
                effect: None,
                artifact: None,
            },
        );
    }

    if is_ddg_bot_challenge(&body) {
        return ToolOutcome::failed(
            "web_search",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: failed\ntool: web_search\nerror: bot_challenge\n\nDuckDuckGo 返回了人机验证页（疑似流量异常）。稍后重试，或换用其他查询。"
                    .into(),
                effect: None,
                artifact: None,
            },
        );
    }

    let hits = parse_ddg_results(&body);
    if hits.is_empty() {
        return ToolOutcome::failed(
            "web_search",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: "status: failed\ntool: web_search\nerror: no_results\n\n没有找到结果（或结果页结构与预期不符）。".into(),
                effect: None,
                artifact: None,
            },
        );
    }

    let mut output = String::from("status: succeeded\ntool: web_search\n");
    output.push_str(&format!(
        "query: {}\nresults: {}\n\n",
        args.query,
        hits.len().min(count as usize)
    ));
    for (index, hit) in hits.iter().take(count as usize).enumerate() {
        output.push_str(&format!(
            "{}. {}\n   url: {}\n   snippet: {}\n\n",
            index + 1,
            hit.title,
            hit.url,
            hit.snippet,
        ));
    }
    ToolOutcome::succeeded("web_search", output).with_metadata(ToolMetadata {
        tool: "web_search".into(),
        target: Some(args.query),
        ..Default::default()
    })
}

/// DDG 的 df 参数（Brave freshness 语法 → DDG）。
fn ddg_freshness(value: &str) -> Option<&'static str> {
    match value {
        "pd_1d" => Some("d"),
        "pd_1w" => Some("w"),
        "pd_1m" => Some("m"),
        "pd_1y" => Some("y"),
        _ => None,
    }
}

/// DDG HTML 端点的人机验证页特征（anomaly challenge / bot challenge）。
pub fn is_ddg_bot_challenge(html: &str) -> bool {
    html.contains("anomaly-modal")
        || html.contains("challenge-form")
        || html.contains("bot-captcha")
        || html.contains("Unfortunately, bots use DuckDuckGo too")
}

/// 解析出的 DDG 结果（免费端点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdgHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 解析 DDG HTML 端点返回的结果页（纯函数，便于离线测试）。
///
/// 结构（html.duckduckgo.com 无 JS 版，多年稳定）：
/// `<div class="result ...">` 每块一个结果；广告块含 `result--ad`；
/// 标题 `<a class="result__a" href="//duckduckgo.com/l/?uddg=...">`；
/// 摘要 `<a class="result__snippet" href="...">`。链接经 uddg 参数还原。
pub fn parse_ddg_results(html: &str) -> Vec<DdgHit> {
    let mut hits = Vec::new();
    for block in html.split("class=\"result ") {
        if block.find("result__a").is_none() {
            continue;
        }
        if block.contains("result--ad") {
            continue; // 广告（§17：不展示广告结果）。
        }
        let Some((title, raw_href)) = extract_link(block, "result__a") else {
            continue;
        };
        let url = decode_ddg_href(raw_href);
        if url.is_empty() {
            continue;
        }
        let snippet = extract_link(block, "result__snippet")
            .map(|(text, _)| text)
            .unwrap_or_default();
        hits.push(DdgHit {
            title,
            url,
            snippet,
        });
    }
    hits
}

/// 在结果块中提取 class 含 `class_name` 的 `<a>` 的文本与 href。
fn extract_link(block: &str, class_name: &str) -> Option<(String, String)> {
    let class_marker = format!("class=\"{class_name}\"");
    let marker = block.find(&class_marker)?;
    let anchor_start = block[..marker].rfind("<a ")?;
    let after_class = &block[marker + class_marker.len()..];
    let tag_end = after_class.find('>')?;
    let attrs = &after_class[..tag_end];
    // href 可能出现在 class 之前（<a href=".." class="result__a"）或之后。
    let href = extract_href_attr(&format!("{} {}", &block[anchor_start..marker], attrs));
    // `>` 的绝对位置 = marker 之后 + tag_end；内容从其后一个字节开始。
    let inner_start = marker + class_marker.len() + tag_end + 1;
    let inner = &block[inner_start..];
    let close = inner.find("</a>")?;
    let text = strip_tags(&inner[..close]);
    Some((text, href))
}

fn extract_href_attr(segment: &str) -> String {
    let Some(start) = segment.find("href=\"") else {
        return String::new();
    };
    let rest = &segment[start + 6..];
    let end = rest.find('"').unwrap_or(rest.len());
    rest[..end].to_string()
}

/// 去掉标签、解码常见 HTML 实体、折叠空白。
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let mut decoded = decode_html_entities(&out);
    decoded = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    decoded.trim().to_string()
}

/// 解码常见 HTML 实体（&amp; &lt; &gt; &quot; &#39; &nbsp; &#<n>; &#x<n>;）。
fn decode_html_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(end) = text[i..].find(';')
        {
            let entity = &text[i + 1..i + end];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(ch) = decoded {
                out.push(ch);
                i += end + 1;
                continue;
            }
        }
        // i < bytes.len() 保证此处必有一个字符；防御性处理（不 panic）。
        let Some(ch) = text[i..].chars().next() else {
            tracing::error!("decode_html_entities: 字节索引越界（内部不变量破坏）");
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 还原 DDG 跳转链接：`//duckduckgo.com/l/?uddg=<urlencoded>&rut=...` → 真实 URL。
/// 非跳转链接原样返回（补全协议相对链接）。
fn decode_ddg_href(href: String) -> String {
    let decoded_entities = decode_html_entities(&href);
    let decoded = percent_decode(&decoded_entities);
    if let Some(uddg_start) = decoded.find("uddg=") {
        let value = &decoded[uddg_start + 5..];
        let value = value.split('&').next().unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    if let Some(rest) = decoded.strip_prefix("//") {
        return format!("https://{rest}");
    }
    decoded
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// web_fetch（§17：限制 redirect、响应体大小和 timeout；HTML 转换）。
pub async fn web_fetch(args: WebFetchArgs, ctx: &ToolContext) -> ToolOutcome {
    let url = match validate_fetch_url(&args.url) {
        Ok(url) => url,
        Err(error) => {
            return ToolOutcome::failed(
                "web_fetch",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_fetch\nerror: ssrf_blocked\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };

    let client = web_fetch_client();

    // P0-10：DNS 预解析——域名解析出的任何地址命中私有/loopback 都拒绝。
    // 字面 IP 已在 validate_fetch_url 校验；这里防“域名解析到私有地址”的
    // SSRF 绕过（DNS rebinding 需要连接后二次验证，不在本次范围，§17 注释）。
    if !allow_private_web_targets()
        && let Some(host) = url.host_str()
        // 字面 IP（含带括号的 IPv6）已由 validate_fetch_url 校验。
        && host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_err()
    {
        let port = url.port_or_known_default().unwrap_or(80);
        if let Ok(addresses) = tokio::net::lookup_host((host, port)).await {
            let blocked = addresses
                .map(|addr| addr.ip())
                .find(|ip| is_blocked_ip(*ip));
            if let Some(ip) = blocked {
                return ToolOutcome::failed(
                    "web_fetch",
                    ModelPayload {
                        status: ToolStatus::Failed,
                        program: None,
                        exit_code: None,
                        duration_ms: 0,
                        output: format!(
                            "status: failed\ntool: web_fetch\nerror: ssrf_blocked\n\n域名 {host} 解析到被拦截地址 {ip}"
                        ),
                        effect: None,
                        artifact: None,
                    },
                );
            }
        }
        // 解析失败（NXDOMAIN 等）由请求阶段报错，不在此拦截。
    }

    let response = match client.get(&args.url).timeout(FETCH_TIMEOUT).send().await {
        Ok(response) => response,
        Err(error) => {
            return ToolOutcome::failed(
                "web_fetch",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_fetch\nerror: request_failed\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };

    if let Err(error) = validate_fetch_url(response.url().as_str()) {
        return ToolOutcome::failed(
            "web_fetch",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!(
                    "status: failed\ntool: web_fetch\nerror: ssrf_blocked\n\nredirect blocked: {error}"
                ),
                effect: None,
                artifact: None,
            },
        );
    }

    let final_url = response.url().to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let bytes = match read_bounded_bytes(response, FETCH_RAW_LIMIT).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("body_too_large") => {
            return ToolOutcome::failed(
                "web_fetch",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_fetch\nerror: body_too_large\n\n响应体超过 {FETCH_RAW_LIMIT} 字节限制（§17）。"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
        Err(error) => {
            return ToolOutcome::failed(
                "web_fetch",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_fetch\nerror: read_failed\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };

    let text = convert_response_body(&bytes, &content_type);

    // 有界正文（§8.4：48 KiB）。
    let truncated = text.len() > FETCH_BODY_BUDGET;
    let mut body = text.clone();
    if truncated {
        body.truncate(FETCH_BODY_BUDGET);
    }

    // artifact 记录完整正文。
    let mut artifact = None;
    if let Ok(mut writer) = crate::session::artifact::ArtifactWriter::create(
        &ctx.artifacts_root,
        &ctx.session_id,
        "web_fetch",
        if content_type.contains("html") || looks_like_html(&bytes) {
            "text/plain"
        } else {
            &content_type
        },
    ) && writer.write("body", text.as_bytes()).is_ok()
        && let Ok(record) = writer.finish()
    {
        artifact = Some(ArtifactRef {
            session: ctx.session_id.clone(),
            id: record.id,
        });
    }

    let output = format!(
        "status: succeeded\ntool: web_fetch\nurl: {final_url}\nhttp: {status}\ncontent_type: {}\ntitle: {}{}\n\n{}",
        content_type,
        extract_title(&body),
        if truncated {
            format!(
                "\ntruncated: true ({} bytes shown of larger body)",
                FETCH_BODY_BUDGET
            )
        } else {
            String::new()
        },
        body,
    );
    let mut outcome = ToolOutcome::succeeded("web_fetch", output);
    // §8.4：artifact 引用进结构化字段与模型可见文本（完整正文的唯一读取入口）。
    if let Some(reference) = &artifact {
        outcome.model_payload.artifact = Some(reference.clone());
        outcome
            .model_payload
            .output
            .push_str(&format!("\nartifact: {reference}"));
    }
    outcome.artifacts = artifact.into_iter().collect();
    outcome.session_metadata = ToolMetadata {
        tool: "web_fetch".into(),
        target: Some(args.url),
        ..Default::default()
    };
    outcome
}

pub fn convert_response_body(bytes: &[u8], content_type: &str) -> String {
    let is_html = content_type.contains("html") || looks_like_html(bytes);
    if is_html {
        html2text::from_read(bytes, usize::MAX).unwrap_or_else(|error| {
            format!("status: failed\ntool: web_fetch\nerror: html_convert_failed\n\n{error}")
        })
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let head = String::from_utf8_lossy(head).to_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<head")
}

fn extract_title(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_loopback_urls() {
        assert!(validate_fetch_url("http://127.0.0.1/").is_err());
        assert!(validate_fetch_url("http://localhost/").is_err());
        // 字面 IPv6 回环（host_str 带括号，此前 parse 失败绕过检查）。
        assert!(validate_fetch_url("http://[::1]/").is_err());
    }

    #[test]
    fn blocks_private_network_urls() {
        assert!(validate_fetch_url("http://192.168.1.1/").is_err());
        assert!(validate_fetch_url("http://10.0.0.5/").is_err());
    }

    /// P0-10：IPv4-mapped IPv6（`::ffff:a.b.c.d`）必须按映射的 v4 地址判定——
    /// 此前 v6 分支只查 loopback/unspecified/fe80/fc00，`::ffff:127.0.0.1`
    /// 与 `::ffff:192.168.1.1` 直接放行（SSRF 绕过）。
    #[test]
    fn blocks_ipv4_mapped_ipv6_loopback_and_private() {
        assert!(
            validate_fetch_url("http://[::ffff:127.0.0.1]/").is_err(),
            "::ffff:127.0.0.1 必须拦截"
        );
        assert!(
            validate_fetch_url("http://[::ffff:192.168.1.1]/").is_err(),
            "::ffff:192.168.1.1 必须拦截"
        );
        assert!(
            validate_fetch_url("http://[::ffff:10.0.0.5]/").is_err(),
            "::ffff:10.0.0.5 必须拦截"
        );
        assert!(
            validate_fetch_url("http://[::ffff:169.254.169.254]/").is_err(),
            "::ffff:169.254.169.254（metadata）必须拦截"
        );
        // 映射到公共地址的 v6 不受影响。
        assert!(validate_fetch_url("http://[::ffff:8.8.8.8]/").is_ok());
    }

    /// P0-10：redirect 目标也必须逐跳通过校验（此前 Policy::limited(5)
    /// 自动跟随 redirect，私有/loopback 目标直接可达）。
    #[test]
    fn redirect_target_is_ssrf_checked() {
        // 逐跳验证函数：redirect policy 与 web_fetch 共用同一校验。
        let public: Url = "https://example.com/page".parse().unwrap();
        assert!(redirect_allowed(&public).is_ok());
        let private: Url = "http://127.0.0.1:8080/ssrf".parse().unwrap();
        assert!(redirect_allowed(&private).is_err());
        let mapped: Url = "http://[::ffff:10.0.0.5]/".parse().unwrap();
        assert!(redirect_allowed(&mapped).is_err());
    }

    #[test]
    fn allows_public_https_urls() {
        assert!(validate_fetch_url("https://example.com/page").is_ok());
    }

    #[test]
    fn converts_html_body() {
        let html = b"<html><body><h1>Hello</h1></body></html>";
        let text = convert_response_body(html, "text/html");
        assert!(text.contains("Hello"));
    }

    // ---- DDG 免费端点解析（§17 无 key 方案）----

    /// 真实端点 fixture：6 个块（1 广告 + 5 普通），5 个 snippet，含 uddg 链接。
    fn fixture() -> String {
        include_str!("../../tests/fixtures/ddg_results.html").to_string()
    }

    #[test]
    fn parses_ddg_fixture_skips_ads_and_decodes_links() {
        let hits = parse_ddg_results(&fixture());
        // fixture：1 广告块 + 4 完整普通结果 + 1 截断残片（第 5 个结果的头部）。
        assert_eq!(hits.len(), 4, "广告块与不完整块必须被过滤，剩 4 个真实结果");
        // uddg 重定向参数必须还原为真实 URL（百分号解码 + 实体解码）。
        let rust = hits.iter().find(|h| h.title.contains("Rust")).unwrap();
        assert_eq!(rust.url, "https://rust-lang.org/");
        // 摘要非空。
        assert!(
            hits.iter().all(|h| !h.snippet.is_empty()),
            "普通结果都应有 snippet"
        );
        // 标题不含标签与实体。
        for hit in &hits {
            assert!(
                !hit.title.contains('<') && !hit.title.contains("&amp;"),
                "标题必须已清理: {}",
                hit.title
            );
            assert!(
                hit.url.starts_with("https://") || hit.url.starts_with("http://"),
                "URL 必须为绝对地址: {}",
                hit.url
            );
        }
    }

    #[test]
    fn parses_ddg_empty_and_challenge_pages() {
        assert!(parse_ddg_results("<html><body>nothing</body></html>").is_empty());
        assert!(parse_ddg_results("no result blocks at all").is_empty());
        assert!(is_ddg_bot_challenge("<div class=\"anomaly-modal\">"));
        assert!(!is_ddg_bot_challenge(fixture().as_str()));
    }

    #[test]
    fn decode_ddg_href_handles_plain_and_protocol_relative() {
        assert_eq!(
            decode_ddg_href(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=x".into()
            ),
            "https://example.com/a?b=1"
        );
        assert_eq!(
            decode_ddg_href("//example.org/page".into()),
            "https://example.org/page"
        );
        assert_eq!(
            decode_ddg_href("https://plain.example/x".into()),
            "https://plain.example/x"
        );
    }

    #[test]
    fn strip_tags_decodes_entities_and_normalizes_whitespace() {
        assert_eq!(
            strip_tags("  Foo <b>bar</b> &amp; baz\nqux  "),
            "Foo bar & baz qux"
        );
        assert_eq!(
            strip_tags("caf\u{e9} &#x2764; &quot;x&quot; &#39;y&#39;"),
            "caf\u{e9} \u{2764} \"x\" 'y'"
        );
    }

    #[test]
    fn ddg_freshness_maps_brave_syntax() {
        assert_eq!(ddg_freshness("pd_1d"), Some("d"));
        assert_eq!(ddg_freshness("pd_1w"), Some("w"));
        assert_eq!(ddg_freshness("pd_1m"), Some("m"));
        assert_eq!(ddg_freshness("pd_1y"), Some("y"));
        assert_eq!(ddg_freshness("2025-01-01..2025-02-01"), None);
        assert_eq!(ddg_freshness(""), None);
    }
}
