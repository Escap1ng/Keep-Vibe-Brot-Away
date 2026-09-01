//! Provider 抽象:openai-compatible / google(gemini)/ anthropic(claude)
//! 全部走 SSE 流式,支持自定义 BaseURL。错误作为流内的 item 返回。

use futures_util::Stream;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::pin::Pin;

pub type BoxTextStream = Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub system: String,
    pub user: String,
    pub max_output_tokens: u32,
}

/// 统一入口:发流式请求,返回 SSE 文本增量流
pub async fn stream_chat(
    client: &reqwest::Client,
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    key: &str,
    req: &ChatRequest,
) -> anyhow::Result<BoxTextStream> {
    match provider {
        "google" => stream_google(client, model, base_url, key, req).await,
        "anthropic" => stream_anthropic(client, model, base_url, key, req).await,
        // 默认 openai 兼容
        _ => stream_openai(client, model, base_url, key, req).await,
    }
}

fn user_messages(req: &ChatRequest) -> Vec<Value> {
    vec![
        json!({ "role": "system", "content": req.system }),
        json!({ "role": "user", "content": req.user }),
    ]
}

/// OpenAI-compatible:POST {base}/chat/completions,data: {"choices":[{"delta":{"content":..}}]}
async fn stream_openai(
    client: &reqwest::Client,
    model: &str,
    base_url: Option<&str>,
    key: &str,
    req: &ChatRequest,
) -> anyhow::Result<BoxTextStream> {
    let base = base_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let url = format!("{base}/chat/completions");

    let mut body = json!({
        "model": model,
        "messages": user_messages(req),
        "stream": true,
        "max_tokens": req.max_output_tokens,
    });
    // DeepSeek 等部分实现要求 max_tokens 必须 ≥1;若模型不支持可走兼容参数
    let _ = &mut body;

    let resp = client
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI 兼容端点 {status}(model={model}, base={base}):{text}");
    }

    let lines = sse_lines(resp);
    Ok(Box::pin(lines.filter_map(move |line_res| {
        let line_res = line_res.map(|line| {
            // data: {...}
            let payload = line.strip_prefix("data:").unwrap_or(&line);
            let payload = payload.trim();
            if payload == "[DONE]" {
                return String::new();
            }
            match serde_json::from_str::<Value>(payload) {
                Ok(v) => v
                    .pointer("/choices/0/delta/content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string(),
                Err(_) => String::new(),
            }
        });
        async move { Some(line_res) }
    })))
}

/// Google Gemini:POST {base}/v1beta/models/{model}:streamGenerateContent?alt=sse
/// data: {"candidates":[{"content":{"parts":[{"text":..}]}}]}
async fn stream_google(
    client: &reqwest::Client,
    model: &str,
    base_url: Option<&str>,
    key: &str,
    req: &ChatRequest,
) -> anyhow::Result<BoxTextStream> {
    let base = base_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com".into());
    let url = format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse&key={key}");

    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": req.user }] }],
        "systemInstruction": { "parts": [{ "text": req.system }] },
        "generationConfig": { "max_output_tokens": req.max_output_tokens }
    });

    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Google 端点 {status}(model={model}, base={base}):{text}");
    }

    let lines = sse_lines(resp);
    Ok(Box::pin(lines.filter_map(move |line_res| {
        let line_res = line_res.map(|line| {
            let payload = line.strip_prefix("data:").unwrap_or(&line).trim();
            match serde_json::from_str::<Value>(payload) {
                Ok(v) => v
                    .pointer("/candidates/0/content/parts")
                    .and_then(|parts| parts.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<String>()
                    })
                    .unwrap_or_default(),
                Err(_) => String::new(),
            }
        });
        async move { Some(line_res) }
    })))
}

/// Anthropic:POST {base}/v1/messages,headers x-api-key/anthropic-version
/// event: content_block_delta → data: {"type":"content_block_delta","delta":{"text":..}}
async fn stream_anthropic(
    client: &reqwest::Client,
    model: &str,
    base_url: Option<&str>,
    key: &str,
    req: &ChatRequest,
) -> anyhow::Result<BoxTextStream> {
    let base = base_url
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.anthropic.com".into());
    let url = format!("{base}/v1/messages");

    let body = json!({
        "model": model,
        "max_tokens": req.max_output_tokens,
        "system": req.system,
        "messages": [{ "role": "user", "content": req.user }],
        "stream": true
    });

    let resp = client
        .post(&url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic 端点 {status}(model={model}, base={base}):{text}");
    }

    let lines = sse_lines(resp);
    Ok(Box::pin(lines.filter_map(move |line_res| {
        let line_res = line_res.map(|line| {
            let payload = line.strip_prefix("data:").unwrap_or(&line).trim();
            match serde_json::from_str::<Value>(payload) {
                Ok(v) => v
                    .pointer("/delta/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                Err(_) => String::new(),
            }
        });
        async move { Some(line_res) }
    })))
}

/// 将响应字节流切分为 SSE data:… 行
fn sse_lines(resp: reqwest::Response) -> impl Stream<Item = anyhow::Result<String>> {
    let byte_stream = resp.bytes_stream();
    futures_util::stream::unfold(
        (byte_stream, String::new()),
        |(mut bytes, mut buf)| async move {
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(chunk) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        if let Some(pos) = buf.find('\n') {
                            let line: String = buf[..pos].trim().to_string();
                            buf.drain(..=pos);
                            if line.starts_with("data:") {
                                return Some((Ok(line), (bytes, buf)));
                            }
                            continue;
                        }
                    }
                    Err(e) => {
                        return Some((Err(anyhow::anyhow!("流解码失败:{e}")), (bytes, buf)))
                    }
                }
            }
            let tail = buf.trim().to_string();
            if tail.starts_with("data:") {
                return Some((Ok(tail), (bytes, String::new())));
            }
            None
        },
    )
}
