//! Web 工具（文档 §17、§8.1 P1）。
//!
//! - `web_search`：Brave Web Search API；只调用标准 Web Search endpoint，
//!   不调用其 LLM/Answers endpoint（§17：避免隐藏模型成本）。
//! - `web_fetch`：reqwest 限制 redirect、响应体大小和 timeout；HTML 用
//!   html2text 转换；正文有界（§8.4：48 KiB）。
//! - 不打开浏览器、不自动 fetch 全部结果、不调用 summary model（§17）。
//! - 未配置 Brave key 时明确 `unavailable`，不自动切换到抓取结果页面。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::ToolContext;
use crate::tool::outcome::{ArtifactRef, ModelPayload, ToolMetadata, ToolOutcome, ToolStatus};

/// web_fetch 正文预算（§8.4：正文 48 KiB）。
pub const FETCH_BODY_BUDGET: usize = 48 * 1024;
/// HTML 原始体积上限（转换前）。
pub const FETCH_RAW_LIMIT: usize = 2 * 1024 * 1024;
/// 请求超时。
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    pub query: String,
    /// 结果数（默认 5，上限 20）。
    #[serde(default = "default_count")]
    pub count: u32,
    /// freshness（如 "pd_1w" 或 ISO 日期；透传给 Brave）。
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

/// web_search（§17：只用于发现来源；结果摘要不是最终证据）。
pub async fn web_search(args: WebSearchArgs, ctx: &ToolContext) -> ToolOutcome {
    let api_key_env = &ctx.web_brave_key_env;
    let api_key = std::env::var(api_key_env).ok();
    let Some(api_key) = api_key else {
        return ToolOutcome::failed(
            "web_search",
            ModelPayload {
                status: ToolStatus::Failed,
                program: None,
                exit_code: None,
                duration_ms: 0,
                output: format!(
                    "status: failed\ntool: web_search\nerror: unavailable\n\n未配置 Brave key（环境变量 {api_key_env}）。不自动切换到其他服务（§17）。"
                ),
                effect: None,
                artifact: None,
            },
        );
    };

    let mut query = args.query.clone();
    if let Some(domains) = &args.domains {
        for domain in domains {
            query.push_str(&format!(" site:{domain}"));
        }
    }
    let count = args.count.clamp(1, 20);
    let mut url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencode(&query),
        count
    );
    if let Some(freshness) = &args.freshness {
        url.push_str(&format!("&freshness={}", urlencode(freshness)));
    }

    let client = reqwest::Client::new();
    let response = match client
        .get(&url)
        .header("X-Subscription-Token", &api_key)
        .header("Accept", "application/json")
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
    let body = match response.text().await {
        Ok(body) => body,
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

    let parsed: Result<BraveResponse, _> = serde_json::from_str(&body);
    let results = match parsed {
        Ok(parsed) => parsed.web.results,
        Err(error) => {
            return ToolOutcome::failed(
                "web_search",
                ModelPayload {
                    status: ToolStatus::Failed,
                    program: None,
                    exit_code: None,
                    duration_ms: 0,
                    output: format!(
                        "status: failed\ntool: web_search\nerror: parse_failed\n\n{error}"
                    ),
                    effect: None,
                    artifact: None,
                },
            );
        }
    };

    let mut output = String::from("status: succeeded\ntool: web_search\n");
    output.push_str(&format!(
        "query: {}\nresults: {}\n\n",
        args.query,
        results.len()
    ));
    for (index, result) in results.iter().enumerate() {
        output.push_str(&format!(
            "{}. {}\n   url: {}\n   snippet: {}\n   age: {}\n\n",
            index + 1,
            result.title,
            result.url,
            result.description,
            result.age.as_deref().unwrap_or("-"),
        ));
    }
    ToolOutcome::succeeded("web_search", output).with_metadata(ToolMetadata {
        tool: "web_search".into(),
        target: Some(args.query),
        ..Default::default()
    })
}

#[derive(Deserialize)]
struct BraveResponse {
    web: BraveWeb,
}

#[derive(Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    age: Option<String>,
}

/// web_fetch（§17：限制 redirect、响应体大小和 timeout；HTML 转换）。
pub async fn web_fetch(args: WebFetchArgs, ctx: &ToolContext) -> ToolOutcome {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| e.to_string())
        .unwrap_or_else(|_| reqwest::Client::new());

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
    let final_url = response.url().to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // §17：限制响应体大小（转换前原始体积）。
    let bytes = match response.bytes().await {
        Ok(bytes) if bytes.len() <= FETCH_RAW_LIMIT => bytes,
        Ok(_) => {
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

    // §17：text/plain/Markdown 直接读取；HTML 用 html2text 转换。
    let is_html = content_type.contains("html") || looks_like_html(&bytes);
    let text = if is_html {
        match html2text::from_read(bytes.as_ref(), usize::MAX) {
            Ok(text) => text,
            Err(error) => {
                return ToolOutcome::failed(
                    "web_fetch",
                    ModelPayload {
                        status: ToolStatus::Failed,
                        program: None,
                        exit_code: None,
                        duration_ms: 0,
                        output: format!(
                            "status: failed\ntool: web_fetch\nerror: html_convert_failed\n\n{error}"
                        ),
                        effect: None,
                        artifact: None,
                    },
                );
            }
        }
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };

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
        if is_html { "text/plain" } else { &content_type },
    ) {
        writer.write("body", text.as_bytes());
        if let Ok(record) = writer.finish() {
            artifact = Some(ArtifactRef {
                session: ctx.session_id.clone(),
                id: record.id,
            });
        }
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
    outcome.artifacts = artifact.into_iter().collect();
    outcome.session_metadata = ToolMetadata {
        tool: "web_fetch".into(),
        target: Some(args.url),
        ..Default::default()
    };
    outcome
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
    // html2text 输出通常以 title 开头；截取第一行（有界）。
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}
