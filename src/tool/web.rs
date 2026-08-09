//! 有界且默认拒绝私网目标的 Web 工具。
//!
//! - `web_search`：DuckDuckGo HTML 端点（免费、无需 API key，社区 skills 通用
//!   方案，参考 pi-web-access 的无 key 思路）；结果只用于发现来源，
//!   不调用 LLM/Answers endpoint，避免隐藏模型成本。
//! - `web_fetch`：reqwest 限制 redirect、响应体大小和 timeout；HTML 用
//!   html2text 转换；正文上限为 48 KiB。
//! - 不打开浏览器、不自动 fetch 全部结果、不调用 summary model。
//! - DDG 可能对异常流量返回人机验证页：此时明确报错，不静默降级。

use std::net::{IpAddr, SocketAddr};

use futures_util::StreamExt;
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use crate::tool::outcome::{ArtifactRef, ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

/// 把抓取正文按 `FETCH_BODY_BUDGET` 有界化。
///
/// BUG-002：此前 `String::truncate` 按裸字节截断，多字节字符边界处 panic；
/// 现在统一走 `util::truncate_to_char_boundary`（合法 UTF-8，绝不 panic）。
/// 返回 (截断后正文, 是否截断)。
fn bounded_body(text: &str) -> (String, bool) {
    let truncated = text.len() > FETCH_BODY_BUDGET;
    if !truncated {
        return (text.to_string(), false);
    }
    let mut body = text.to_string();
    crate::util::truncate_to_char_boundary(&mut body, FETCH_BODY_BUDGET);
    (body, true)
}
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
/// Bing HTML 端点（免费回退引擎；可能对异常流量返回人机验证页）。
const BING_ENDPOINT: &str = "https://www.bing.com/search";
/// Yahoo HTML 端点（免费第三回退引擎；对异常流量容忍度较高）。
const YAHOO_ENDPOINT: &str = "https://search.yahoo.com/search";

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

#[derive(Clone, Copy)]
enum TargetPolicy {
    PublicOnly,
    AllowPrivateForTest,
}

impl TargetPolicy {
    fn allows_private(self) -> bool {
        matches!(self, Self::AllowPrivateForTest)
    }
}

/// 校验 fetch 目标 URL（§17：仅 HTTP(S)，拒绝 loopback/私网/链路本地）。
pub fn validate_fetch_url(url_str: &str) -> Result<Url, String> {
    let url = Url::parse(url_str.trim()).map_err(|error| format!("invalid url: {error}"))?;
    validate_url(url, TargetPolicy::PublicOnly)
}

fn validate_url(url: Url, policy: TargetPolicy) -> Result<Url, String> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("unsupported scheme: {scheme}"));
    }
    let host = url.host_str().ok_or_else(|| "missing host".to_string())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("credentials in url are not allowed".to_string());
    }
    if !policy.allows_private() && is_blocked_host(host) {
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
            let [a, b, c, _] = v4.octets();
            a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224
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
            // multicast ff00::/8、文档 2001:db8::/32、discard-only 100::/64、
            // benchmarking 2001:2::/48 都不是公开 Web 目标。
            if (segments[0] & 0xff00) == 0xff00
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x0100
                    && segments[1] == 0
                    && segments[2] == 0
                    && segments[3] == 0)
                || (segments[0] == 0x2001 && segments[1] == 0x0002 && segments[2] == 0)
            {
                return true;
            }
            false
        }
    }
}

/// 为单次请求解析并固定 DNS 结果。客户端禁用代理和自动重定向，确保安全检查的
/// 地址就是实际连接的地址；每个重定向目标都会重新执行本流程。
async fn send_fetch_hop(url: &Url, policy: TargetPolicy) -> Result<reqwest::Response, String> {
    validate_url(url.clone(), policy)?;
    let host = url.host_str().ok_or_else(|| "missing host".to_string())?;
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "missing port".to_string())?;

    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if bare_host.parse::<IpAddr>().is_err() {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((bare_host, port))
            .await
            .map_err(|error| format!("dns_failed: {error}"))?
            .collect();
        if addresses.is_empty() {
            return Err("dns_failed: no addresses returned".to_string());
        }
        if !policy.allows_private()
            && let Some(blocked) = addresses.iter().find(|addr| is_blocked_ip(addr.ip()))
        {
            return Err(format!(
                "ssrf_blocked: domain {host} resolved to blocked address {}",
                blocked.ip()
            ));
        }
        builder = builder.resolve_to_addrs(bare_host, &addresses);
    }

    let client = builder
        .build()
        .map_err(|error| format!("client_build_failed: {error}"))?;
    client
        .get(url.clone())
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("request_failed: {error}"))
}

async fn fetch_following_safe_redirects(
    initial_url: Url,
    policy: TargetPolicy,
    ctx: &ToolContext,
) -> Result<(Url, reqwest::Response), String> {
    const MAX_REDIRECTS: usize = 5;
    let mut current = initial_url;
    for redirect_count in 0..=MAX_REDIRECTS {
        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err("cancelled".to_string()),
            response = send_fetch_hop(&current, policy) => response?,
        };
        if !response.status().is_redirection() {
            return Ok((current, response));
        }
        if redirect_count == MAX_REDIRECTS {
            return Err(format!("too_many_redirects: exceeded {MAX_REDIRECTS}"));
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| "invalid_redirect: missing Location header".to_string())?
            .to_str()
            .map_err(|_| "invalid_redirect: Location is not valid text".to_string())?;
        let next = current
            .join(location)
            .map_err(|error| format!("invalid_redirect: {error}"))?;
        current = validate_url(next, policy)?;
    }
    Err("too_many_redirects: redirect state exhausted unexpectedly".to_string())
}

async fn read_bounded_bytes(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("body_too_large: exceeded {limit} bytes"));
    }
    let mut stream = response.bytes_stream();
    let mut total = 0usize;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("read_failed: {error}"))?;
        total = total
            .checked_add(chunk.len())
            .ok_or_else(|| "body_too_large: size overflow".to_string())?;
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
/// web_search（§17：只用于发现来源；结果摘要不是最终证据）。
///
/// 免费方案：主引擎 DuckDuckGo HTML 端点；失败（人机验证/无结果/HTTP
/// 错误）时依次回退 Bing、Yahoo HTML 端点——不同网络环境下至少一个可用
/// （本机实测 DDG/Bing 可能同时返回人机验证页，Yahoo 通常可用）。
/// 全部引擎都失败时给出汇总错误，不产生乱码输出。
pub async fn web_search(args: WebSearchArgs, ctx: &ToolContext) -> ToolOutcome {
    if let Err(error) = validate_search_args(&args) {
        return failed_web_outcome("web_search", "invalid_arguments", error);
    }
    let count = args.count.clamp(1, 20);
    let query = build_search_query(&args);
    let freshness = args.freshness.as_deref();

    // §PointerHit 4：搜索前检查取消（用户 Esc 后不再发起新请求）。
    if ctx.cancel.is_cancelled() {
        return cancelled_outcome("web_search");
    }
    let ddg = tokio::select! {
        _ = ctx.cancel.cancelled() => return cancelled_outcome("web_search"),
        result = search_ddg(&query, freshness) => result,
    };
    match ddg {
        Ok(hits) => {
            if ctx.cancel.is_cancelled() {
                cancelled_outcome("web_search")
            } else {
                search_succeeded(&args, hits, count)
            }
        }
        Err(ddg_error) => {
            tracing::warn!(error = %ddg_error, "web_search: DDG 失败，回退 Bing");
            if ctx.cancel.is_cancelled() {
                return cancelled_outcome("web_search");
            }
            let bing = tokio::select! {
                _ = ctx.cancel.cancelled() => return cancelled_outcome("web_search"),
                result = search_bing(&query, freshness) => result,
            };
            match bing {
                Ok(hits) => {
                    if ctx.cancel.is_cancelled() {
                        cancelled_outcome("web_search")
                    } else {
                        search_succeeded(&args, hits, count)
                    }
                }
                Err(bing_error) => {
                    tracing::warn!(error = %bing_error, "web_search: Bing 失败，回退 Yahoo");
                    if ctx.cancel.is_cancelled() {
                        return cancelled_outcome("web_search");
                    }
                    let yahoo = tokio::select! {
                        _ = ctx.cancel.cancelled() => return cancelled_outcome("web_search"),
                        result = search_yahoo(&query, freshness) => result,
                    };
                    match yahoo {
                        Ok(hits) => {
                            if ctx.cancel.is_cancelled() {
                                cancelled_outcome("web_search")
                            } else {
                                search_succeeded(&args, hits, count)
                            }
                        }
                        Err(yahoo_error) => ToolOutcome::failed(
                            "web_search",
                            ModelPayload {
                                status: ToolStatus::Failed,
                                program: None,
                                exit_code: None,
                                duration_ms: 0,
                                output: format!(
                                    "status: failed\ntool: web_search\nerror: all_engines_failed\n\nDuckDuckGo: {ddg_error}\nBing: {bing_error}\nYahoo: {yahoo_error}"
                                ),
                                effect: None,
                                artifact: None,
                            },
                        ),
                    }
                }
            }
        }
    }
}

fn failed_web_outcome(tool: &str, code: &str, detail: impl std::fmt::Display) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        ModelPayload {
            status: ToolStatus::Failed,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: format!("status: failed\ntool: {tool}\nerror: {code}\n\n{detail}"),
            effect: None,
            artifact: None,
        },
    )
}

/// 取消的 web 工具结果（§PointerHit：Esc 后及时返回 Cancelled 而非继续等待）。
fn cancelled_outcome(tool: &str) -> ToolOutcome {
    ToolOutcome::failed(
        tool,
        ModelPayload {
            status: ToolStatus::Cancelled,
            program: None,
            exit_code: None,
            duration_ms: 0,
            output: "status: cancelled
tool: {tool}
error: cancelled

已取消。"
                .replace("{tool}", tool),
            effect: None,
            artifact: None,
        },
    )
}

fn validate_search_args(args: &WebSearchArgs) -> Result<(), String> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err("query must not be empty".to_string());
    }
    if query.len() > 4096 {
        return Err("query exceeds 4096 bytes".to_string());
    }
    if let Some(freshness) = args.freshness.as_deref()
        && ddg_freshness(freshness).is_none()
    {
        return Err(format!("unsupported freshness: {freshness}"));
    }
    if let Some(domains) = &args.domains {
        if domains.len() > 20 {
            return Err("at most 20 domains are allowed".to_string());
        }
        for domain in domains {
            let domain = domain.trim().trim_end_matches('.');
            if domain.is_empty()
                || domain.len() > 253
                || domain.starts_with('-')
                || domain.ends_with('-')
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
            {
                return Err(format!("invalid domain filter: {domain}"));
            }
        }
    }
    Ok(())
}

/// 拼接查询（原始 query + site: 域过滤）。
fn build_search_query(args: &WebSearchArgs) -> String {
    let mut query = args.query.trim().to_string();
    if let Some(domains) = &args.domains {
        for domain in domains {
            query.push_str(&format!(" site:{}", domain.trim().trim_end_matches('.')));
        }
    }
    query
}

/// 成功输出（DDG/Bing 结果格式一致）。
fn search_succeeded(args: &WebSearchArgs, hits: Vec<DdgHit>, count: u32) -> ToolOutcome {
    let hits: Vec<DdgHit> = hits
        .into_iter()
        .filter(|hit| {
            Url::parse(&hit.url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        })
        .take(count as usize)
        .collect();
    let mut output = String::from("status: succeeded\ntool: web_search\n");
    output.push_str(&format!(
        "query: {}\nresults: {}\n\n",
        args.query,
        hits.len()
    ));
    for (index, hit) in hits.iter().enumerate() {
        let item = format!(
            "{}. {}\n   url: {}\n   snippet: {}\n\n",
            index + 1,
            truncate_chars(&hit.title, 200),
            truncate_chars(&hit.url, 2048),
            truncate_chars(&hit.snippet, 500),
        );
        if output.len().saturating_add(item.len()) > FETCH_BODY_BUDGET {
            output.push_str("truncated: true (output budget reached)\n");
            break;
        }
        output.push_str(&item);
    }
    ToolOutcome::succeeded("web_search", output).with_metadata(ToolMetadata {
        tool: "web_search".into(),
        target: Some(args.query.clone()),
        ..Default::default()
    })
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

/// 通用 HTML GET：浏览器 UA + timeout + 有界读取 + HTTP 状态检查。
async fn fetch_search_html(url: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let response = match client
        .get(url)
        .header(reqwest::header::USER_AGENT, DDG_USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/html")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return Err(format!("request_failed: {error}")),
    };
    let status = response.status();
    let body_bytes = match read_bounded_bytes(response, SEARCH_RAW_LIMIT).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("body_too_large") => {
            return Err(format!(
                "body_too_large: 响应体超过 {SEARCH_RAW_LIMIT} 字节限制"
            ));
        }
        Err(error) => return Err(format!("read_failed: {error}")),
    };
    let body = String::from_utf8_lossy(&body_bytes).into_owned();
    if !status.is_success() {
        return Err(format!("http_{status}"));
    }
    Ok(body)
}

/// DDG 引擎：无结果/人机验证/错误时返回 Err（触发 Bing 回退）。
async fn search_ddg(query: &str, freshness: Option<&str>) -> Result<Vec<DdgHit>, String> {
    let mut url = format!("{DDG_HTML_ENDPOINT}?q={}", urlencode(query));
    if let Some(df) = freshness.and_then(ddg_freshness) {
        url.push_str(&format!("&df={df}"));
    }
    let body = fetch_search_html(&url).await?;
    if is_ddg_bot_challenge(&body) {
        return Err("bot_challenge: DuckDuckGo 返回人机验证页（疑似流量异常）".into());
    }
    let hits = parse_ddg_results(&body);
    if hits.is_empty() {
        return Err("no_results: DuckDuckGo 无结果（或页面结构变化）".into());
    }
    Ok(hits)
}

/// Bing 引擎：无结果/人机验证/错误时返回 Err。
async fn search_bing(query: &str, freshness: Option<&str>) -> Result<Vec<DdgHit>, String> {
    let mut url = format!("{BING_ENDPOINT}?q={}&count=10&setlang=en", urlencode(query));
    if let Some(qft) = freshness.and_then(bing_freshness) {
        url.push_str(&format!("&qft={}", urlencode(qft)));
    }
    let body = fetch_search_html(&url).await?;
    if is_bing_captcha(&body) {
        return Err("bot_challenge: Bing 返回人机验证页（疑似流量异常）".into());
    }
    let hits = parse_bing_results(&body);
    if hits.is_empty() {
        return Err("no_results: Bing 无结果（或页面结构变化）".into());
    }
    Ok(hits)
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

/// Bing 的 qft 参数（过去 N 天；`interval="N"` 由调用方 urlencode）。
fn bing_freshness(value: &str) -> Option<&'static str> {
    match value {
        "pd_1d" => Some(r#"interval="1""#),
        "pd_1w" => Some(r#"interval="7""#),
        "pd_1m" => Some(r#"interval="30""#),
        "pd_1y" => Some(r#"interval="365""#),
        _ => None,
    }
}

/// Yahoo 引擎：无结果/人机验证/错误时返回 Err。
async fn search_yahoo(query: &str, freshness: Option<&str>) -> Result<Vec<DdgHit>, String> {
    let mut url = format!("{YAHOO_ENDPOINT}?p={}", urlencode(query));
    if let Some(age) = freshness.and_then(yahoo_freshness) {
        url.push_str(&format!("&age={age}"));
    }
    let body = fetch_search_html(&url).await?;
    if is_yahoo_captcha(&body) {
        return Err("bot_challenge: Yahoo 返回人机验证页（疑似流量异常）".into());
    }
    let hits = parse_yahoo_results(&body);
    if hits.is_empty() {
        return Err("no_results: Yahoo 无结果（或页面结构变化）".into());
    }
    Ok(hits)
}

/// Yahoo 的 age 参数（1d/1w/1m/1y）。
fn yahoo_freshness(value: &str) -> Option<&'static str> {
    match value {
        "pd_1d" => Some("1d"),
        "pd_1w" => Some("1w"),
        "pd_1m" => Some("1m"),
        "pd_1y" => Some("1y"),
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

// ---- Bing HTML 端点（回退引擎）----

/// Bing HTML 端点的人机验证页特征（Cloudflare Turnstile challenge）。
pub fn is_bing_captcha(html: &str) -> bool {
    html.contains("verifyEndpoint")
        || html.contains("captchaSuccessPostMessage")
        || html.contains("b_captcha")
        || html.contains("CAPTCHA")
}

/// 解析 Bing HTML 结果页（`<li class="b_algo">` 块，多年稳定）。
///
/// - 广告块（`b_ad`）过滤；
/// - 标题 `<h2><a href="...">Title</a></h2>`；
/// - 摘要 `<p>snippet</p>`；
/// - Bing 重定向链接 `bing.com/ck/a?...&u=<base64url>` 还原真实 URL。
pub fn parse_bing_results(html: &str) -> Vec<DdgHit> {
    let mut hits = Vec::new();
    for block in html.split("b_algo").skip(1) {
        if block.contains("b_ad") || block.contains("b_ad2") {
            continue; // 广告（§17：不展示广告结果）。
        }
        let Some((title, raw_href)) = extract_first_anchor(block) else {
            continue;
        };
        let url = decode_bing_href(&raw_href);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = extract_first_paragraph(block).unwrap_or_default();
        hits.push(DdgHit {
            title,
            url,
            snippet,
        });
    }
    hits
}

/// 提取块内第一个 `<a href="...">text</a>`（Bing 的 `<h2><a>` 标题）。
fn extract_first_anchor(block: &str) -> Option<(String, String)> {
    let anchor_start = block.find("<a ")?;
    let after = &block[anchor_start..];
    let tag_end = after.find('>')?;
    let href = extract_href_attr(&after[..tag_end]);
    let inner = &after[tag_end + 1..];
    let close = inner.find("</a>")?;
    let text = strip_tags(&inner[..close]);
    Some((text, href))
}

/// 提取块内第一个 `<p ...>text</p>` 的文本（Bing 摘要）。
fn extract_first_paragraph(block: &str) -> Option<String> {
    let start = block.find("<p")?;
    let after = &block[start..];
    let tag_end = after.find('>')?;
    let inner = &after[tag_end + 1..];
    let close = inner.find("</p>")?;
    let text = strip_tags(&inner[..close]);
    if text.is_empty() { None } else { Some(text) }
}

/// 还原 Bing 跳转链接：`https://www.bing.com/ck/a?...&u=<base64url>&ntb=1`。
/// 普通直链原样返回。
fn decode_bing_href(href: &str) -> String {
    if let Some(start) = href.find("u=") {
        let value = &href[start + 2..];
        let value = value.split('&').next().unwrap_or("");
        // u= 值先做 URL 百分号解码（Bing 会对 padding 的 `=` 编码为 %3d），
        // 再 base64url 解码得到真实 URL。
        let value = percent_decode(value);
        if let Some(decoded) = base64url_decode(&value) {
            let decoded = decode_html_entities(&decoded);
            if decoded.starts_with("http://") || decoded.starts_with("https://") {
                return decoded;
            }
        }
    }
    let decoded = decode_html_entities(href);
    if let Some(rest) = decoded.strip_prefix("//") {
        return format!("https://{rest}");
    }
    decoded
}

/// base64url 解码（无 padding 或带 `=` padding、URL-safe 字母表；失败返回 None）。
fn base64url_decode(value: &str) -> Option<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    for byte in value.bytes() {
        if byte == b'=' {
            break; // padding（仅允许在末尾）
        }
        let index = ALPHABET.iter().position(|&c| c == byte)?;
        bits = (bits << 6) | index as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    String::from_utf8(out).ok()
}
// ---- Yahoo HTML 端点（第三回退引擎）----

/// Yahoo HTML 端点的人机验证页特征。
pub fn is_yahoo_captcha(html: &str) -> bool {
    html.contains("yid-captcha")
        || html.contains("Please verify you are a human")
        || html.contains("captcha")
        || html.contains("challenge")
}

/// 解析 Yahoo HTML 结果页。
///
/// 结构：`<li><div class="dd algo ...">` 每块一个结果；标题
/// `<div class="compTitle..."><a ... href="https://r.search.yahoo.com/.../RU=<enc>/RK=...">`；
/// 摘要 `<div class="compText"><p ...><span ...>snippet</span>`。
pub fn parse_yahoo_results(html: &str) -> Vec<DdgHit> {
    let mut hits = Vec::new();
    for block in html.split("dd algo").skip(1) {
        // 跳过广告/推广块（Yahoo 广告通常含 promo/ad 标记）。
        if block.contains("promo") || block.contains("algo-ad") {
            continue;
        }
        let Some((title, raw_href)) = extract_yahoo_title(block) else {
            continue;
        };
        let url = decode_yahoo_href(&raw_href);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = extract_yahoo_snippet(block).unwrap_or_default();
        hits.push(DdgHit {
            title,
            url,
            snippet,
        });
    }
    hits
}

/// 提取 Yahoo 标题块的链接（compTitle 下的 `<a href>`）与标题文本
/// （`<h3 class="title">` 内第一个 span 文本）。
fn extract_yahoo_title(block: &str) -> Option<(String, String)> {
    let marker = block.find("compTitle")?;
    let rest = &block[marker..];
    let anchor_start = rest.find("<a ")?;
    let after = &rest[anchor_start..];
    let tag_end = after.find('>')?;
    let href = extract_href_attr(&after[..tag_end]);
    // 标题在 h3.title 的 span 内（a 内还有 favicon/域名 span，不能整体取）。
    let h3 = rest.find("<h3")?;
    let seg = &rest[h3..];
    let span_start = seg.find("<span")?;
    let after_span = &seg[span_start..];
    let span_tag_end = after_span.find('>')?;
    let inner = &after_span[span_tag_end + 1..];
    let close = inner.find("</span>")?;
    let text = strip_tags(&inner[..close]);
    Some((text, href))
}

/// 提取 Yahoo 摘要（compText 块内第一个非空 p/span 文本）。
fn extract_yahoo_snippet(block: &str) -> Option<String> {
    let start = block.find("compText")?;
    let after = &block[start..];
    // 依次尝试 p 与 span 容器，取第一个非空文本。
    for container in ["<p", "<span"] {
        let Some(c_start) = after.find(container) else {
            continue;
        };
        let seg = &after[c_start..];
        let Some(tag_end) = seg.find('>') else {
            continue;
        };
        let inner = &seg[tag_end + 1..];
        let close_marker = if container == "<p" { "</p>" } else { "</span>" };
        let Some(close) = inner.find(close_marker) else {
            continue;
        };
        let text = strip_tags(&inner[..close]);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// 还原 Yahoo 跳转链接：`https://r.search.yahoo.com/.../RU=<urlencoded>/RK=...`。
/// 非跳转链接原样返回。
fn decode_yahoo_href(href: &str) -> String {
    let decoded = decode_html_entities(href);
    if let Some(ru) = decoded.find("/RU=") {
        let value = &decoded[ru + 4..];
        let value = value.split('/').next().unwrap_or("");
        let value = percent_decode(value);
        if value.starts_with("http://") || value.starts_with("https://") {
            return value;
        }
    }
    if let Some(rest) = decoded.strip_prefix("//") {
        return format!("https://{rest}");
    }
    decoded
}

/// web_fetch（§17：限制 redirect、响应体大小和 timeout；HTML 转换）。
pub async fn web_fetch(args: WebFetchArgs, ctx: &ToolContext) -> ToolOutcome {
    web_fetch_with_policy(args, ctx, TargetPolicy::PublicOnly).await
}

/// Integration-test entry point. Production callers cannot select this policy
/// through configuration or environment state.
#[doc(hidden)]
pub async fn web_fetch_allowing_private_for_test(
    args: WebFetchArgs,
    ctx: &ToolContext,
) -> ToolOutcome {
    web_fetch_with_policy(args, ctx, TargetPolicy::AllowPrivateForTest).await
}

async fn web_fetch_with_policy(
    args: WebFetchArgs,
    ctx: &ToolContext,
    policy: TargetPolicy,
) -> ToolOutcome {
    let url = match Url::parse(args.url.trim())
        .map_err(|error| format!("invalid url: {error}"))
        .and_then(|url| validate_url(url, policy))
    {
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

    let (final_url, response) = match fetch_following_safe_redirects(url, policy, ctx).await {
        Ok(result) => result,
        Err(error) if error == "cancelled" => return cancelled_outcome("web_fetch"),
        Err(error) => {
            let code = if error.contains("blocked") {
                "ssrf_blocked"
            } else if error.starts_with("too_many_redirects")
                || error.starts_with("invalid_redirect")
            {
                "redirect_failed"
            } else {
                "request_failed"
            };
            return failed_web_outcome("web_fetch", code, error);
        }
    };
    let final_url = final_url.to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let read_result = tokio::select! {
        _ = ctx.cancel.cancelled() => return cancelled_outcome("web_fetch"),
        result = read_bounded_bytes(response, FETCH_RAW_LIMIT) => result,
    };
    let bytes = match read_result {
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

    let text = match convert_response_body(&bytes, &content_type) {
        Ok(text) => text,
        Err(error) => return failed_web_outcome("web_fetch", "html_convert_failed", error),
    };

    // 有界正文（§8.4：48 KiB；BUG-002：UTF-8 安全截断，不 panic）。
    let (body, truncated) = bounded_body(&text);

    // artifact 记录完整正文。
    let artifact_mime = if content_type.contains("html") || looks_like_html(&bytes) {
        "text/plain"
    } else if content_type.is_empty() {
        "application/octet-stream"
    } else {
        &content_type
    };
    let mut writer = match crate::session::artifact::ArtifactWriter::create(
        &ctx.artifacts_root,
        &ctx.session_id,
        "web_fetch",
        artifact_mime,
    ) {
        Ok(writer) => writer,
        Err(error) => return failed_web_outcome("web_fetch", "artifact_create_failed", error),
    };
    if let Err(error) = writer.write("body", text.as_bytes()) {
        return failed_web_outcome("web_fetch", "artifact_write_failed", error);
    }
    let record = match writer.finish() {
        Ok(record) => record,
        Err(error) => return failed_web_outcome("web_fetch", "artifact_finalize_failed", error),
    };
    let artifact = ArtifactRef {
        session: ctx.session_id.clone(),
        id: record.id,
    };

    let outcome_status = if status.is_success() {
        "succeeded"
    } else {
        "failed"
    };
    let http_error = if status.is_success() {
        String::new()
    } else {
        "error: http_status\n".to_string()
    };
    let output = format!(
        "status: {outcome_status}\ntool: web_fetch\n{http_error}url: {final_url}\nhttp: {status}\ncontent_type: {}\ntitle: {}{}\n\n{}",
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
    let mut outcome = if status.is_success() {
        ToolOutcome::succeeded("web_fetch", output)
    } else {
        ToolOutcome::failed(
            "web_fetch",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output,
                effect: None,
                artifact: None,
            },
        )
    };
    // §8.4：artifact 引用进结构化字段与模型可见文本（完整正文的唯一读取入口）。
    outcome.model_payload.artifact = Some(artifact.clone());
    outcome
        .model_payload
        .output
        .push_str(&format!("\nartifact: {artifact}"));
    outcome.artifacts = vec![artifact];
    outcome.session_metadata = ToolMetadata {
        tool: "web_fetch".into(),
        target: Some(args.url),
        ..Default::default()
    };
    outcome
}

pub fn convert_response_body(bytes: &[u8], content_type: &str) -> Result<String, String> {
    let is_html = content_type.contains("html") || looks_like_html(bytes);
    if is_html {
        html2text::from_read(bytes, 120).map_err(|error| error.to_string())
    } else {
        Ok(String::from_utf8_lossy(bytes).to_string())
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
        assert!(validate_fetch_url("http://100.64.0.1/").is_err());
        assert!(validate_fetch_url("http://192.0.2.1/").is_err());
        assert!(validate_fetch_url("http://224.0.0.1/").is_err());
        assert!(validate_fetch_url("http://[2001:db8::1]/").is_err());
        assert!(validate_fetch_url("http://[ff02::1]/").is_err());
    }

    #[test]
    fn rejects_credentials_in_fetch_urls() {
        assert!(validate_fetch_url("https://user:secret@example.com/").is_err());
    }

    #[test]
    fn validates_search_arguments() {
        let valid = WebSearchArgs {
            query: "rust".into(),
            count: 5,
            freshness: Some("pd_1w".into()),
            domains: Some(vec!["rust-lang.org".into()]),
        };
        assert!(validate_search_args(&valid).is_ok());
        let mut invalid = valid.clone();
        invalid.freshness = Some("yesterday".into());
        assert!(validate_search_args(&invalid).is_err());
        invalid = valid;
        invalid.domains = Some(vec!["example.com site:attacker.test".into()]);
        assert!(validate_search_args(&invalid).is_err());
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
        assert!(validate_url(public, TargetPolicy::PublicOnly).is_ok());
        let private: Url = "http://127.0.0.1:8080/ssrf".parse().unwrap();
        assert!(validate_url(private, TargetPolicy::PublicOnly).is_err());
        let mapped: Url = "http://[::ffff:10.0.0.5]/".parse().unwrap();
        assert!(validate_url(mapped, TargetPolicy::PublicOnly).is_err());
    }

    #[test]
    fn allows_public_https_urls() {
        assert!(validate_fetch_url("https://example.com/page").is_ok());
    }

    #[test]
    fn converts_html_body() {
        let html = b"<html><body><h1>Hello</h1></body></html>";
        let text = convert_response_body(html, "text/html").unwrap();
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

    // ---- Bing 回退引擎 ----

    /// 标准 b_algo 结构：普通结果（含 ck/a 重定向）+ 广告 + 协议相对链接。
    fn bing_fixture() -> String {
        r##"<html><body>
<li class="b_algo"><h2><a href="https://www.bing.com/ck/a?x=1&u=aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw%3d%3d&ntb=1">Rust Programming Language</a></h2><div class="b_caption"><p>Systems programming language focused on <b>safety</b>.</p></div></li>
<li class="b_algo b_ad"><h2><a href="https://ad.example/">Sponsored</a></h2><p>ad text</p></li>
<li class="b_algo"><h2><a href="https://doc.rust-lang.org/book/">The Rust Book</a></h2><div class="b_caption"><p>Learn Rust with the official book.</p></div></li>
<li class="b_algo"><h2><a href="//example.com/protocol-relative">Rel</a></h2><p>proto</p></li>
</body></html>
"##
        .to_string()
    }

    #[test]
    fn parses_bing_fixture_skips_ads_and_decodes_redirects() {
        let hits = parse_bing_results(&bing_fixture());
        // 广告过滤后剩 3 个真实结果。
        assert_eq!(hits.len(), 3, "广告块必须被过滤: {hits:?}");
        let rust = hits
            .iter()
            .find(|h| h.title.contains("Rust Programming"))
            .unwrap();
        // ck/a 的 u= base64url 还原为真实 URL（含 %3d padding 与实体解码）。
        assert_eq!(rust.url, "https://rust-lang.org/");
        assert_eq!(
            rust.snippet,
            "Systems programming language focused on safety."
        );
        // 协议相对链接补全。
        assert_eq!(hits[2].url, "https://example.com/protocol-relative");
        assert!(!hits.iter().any(|h| h.url.contains("bing.com/ck")));
    }

    #[test]
    fn bing_challenge_page_is_detected() {
        // 真实 captcha 页特征（Cloudflare Turnstile challenge）。
        let real = r#"var CfConfig ={"lang":"en","verifyEndpoint":"https://www.bing.com/challenge/verify?partner=7","captchaSuccessPostMessage":"verificationComplete"};"#;
        assert!(is_bing_captcha(real));
        assert!(!is_bing_captcha(&bing_fixture()));
    }

    // ---- Yahoo 第三回退引擎 ----

    /// 真实端点 fixture：3 个 dd algo 结果块（含 r.search.yahoo.com 重定向）。
    fn yahoo_fixture() -> String {
        include_str!("../../tests/fixtures/yahoo_results.html").to_string()
    }

    #[test]
    fn parses_yahoo_fixture_decodes_redirects() {
        let hits = parse_yahoo_results(&yahoo_fixture());
        assert!(!hits.is_empty(), "必须解析出结果");
        // RU= 参数还原真实 URL（percent 解码）。
        for hit in &hits {
            assert!(
                hit.url.starts_with("https://") || hit.url.starts_with("http://"),
                "URL 必须为绝对地址: {}",
                hit.url
            );
            assert!(
                !hit.url.contains("r.search.yahoo.com"),
                "重定向链接必须还原: {}",
                hit.url
            );
            assert!(!hit.title.is_empty(), "标题非空");
            assert!(
                !hit.title.contains("https://") && !hit.title.contains("›"),
                "标题不得混入 URL 路径: {}",
                hit.title
            );
            assert!(!hit.snippet.is_empty(), "摘要非空: {}", hit.title);
        }
    }

    #[test]
    fn yahoo_captcha_page_is_detected() {
        let real = r#"<div id="yid-captcha">Please verify you are a human</div>"#;
        assert!(is_yahoo_captcha(real));
        assert!(!is_yahoo_captcha(&yahoo_fixture()));
    }

    #[test]
    fn decode_yahoo_href_handles_redirect_and_plain() {
        // 真实格式：/RU=https%3a%2f%2fwww.rust-lang.org%2ftools%2finstall/RK=2/RS=xxx
        assert_eq!(
            decode_yahoo_href(
                "https://r.search.yahoo.com/_ylt=abc/RU=https%3a%2f%2frust-lang.org%2f/RK=2/RS=xyz"
            ),
            "https://rust-lang.org/"
        );
        assert_eq!(
            decode_yahoo_href("https://plain.example/page"),
            "https://plain.example/page"
        );
    }

    #[test]
    fn yahoo_freshness_maps_brave_syntax() {
        assert_eq!(yahoo_freshness("pd_1d"), Some("1d"));
        assert_eq!(yahoo_freshness("pd_1w"), Some("1w"));
        assert_eq!(yahoo_freshness("pd_1m"), Some("1m"));
        assert_eq!(yahoo_freshness("pd_1y"), Some("1y"));
        assert_eq!(yahoo_freshness("nonsense"), None);
    }

    #[test]
    fn base64url_decode_round_trips() {
        // "https://rust-lang.org/" 的 base64url（无 padding）。
        assert_eq!(
            base64url_decode("aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw").as_deref(),
            Some("https://rust-lang.org/")
        );
        // 非法字符 → None（不 panic）。
        assert_eq!(base64url_decode("!!!not-base64!!"), None);
    }

    #[test]
    fn bing_freshness_maps_brave_syntax() {
        assert_eq!(bing_freshness("pd_1d"), Some(r#"interval="1""#));
        assert_eq!(bing_freshness("pd_1w"), Some(r#"interval="7""#));
        assert_eq!(bing_freshness("pd_1m"), Some(r#"interval="30""#));
        assert_eq!(bing_freshness("pd_1y"), Some(r#"interval="365""#));
        assert_eq!(bing_freshness("nonsense"), None);
    }
    /// BUG-002 回归：超过 48 KiB 且截断点落在多字节字符中间的正文
    /// 必须被 UTF-8 安全截断（不 panic、不产生 replacement char）。
    #[test]
    fn bounded_body_truncates_cjk_at_char_boundary_without_panic() {
        // 20k 个“中”（3 字节/字）= 60000 字节 > 48 KiB；边界落在字符中间。
        let text = "中".repeat(20_000);
        let (body, truncated) = bounded_body(&text);
        assert!(truncated, "必须标记截断");
        assert!(
            body.len() <= FETCH_BODY_BUDGET,
            "正文超预算: {}",
            body.len()
        );
        assert!(
            std::str::from_utf8(body.as_bytes()).is_ok(),
            "截断后必须是合法 UTF-8"
        );
        assert!(!body.contains('\u{FFFD}'), "不得产生 replacement char");
        assert!(body.ends_with('中'), "应保留可读尾部");
    }

    /// BUG-002：预算内正文原样返回。
    #[test]
    fn bounded_body_short_text_is_unchanged() {
        let (body, truncated) = bounded_body("你好 world");
        assert!(!truncated);
        assert_eq!(body, "你好 world");
    }
}
