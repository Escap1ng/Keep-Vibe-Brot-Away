//! Provider 抽象:openai-compatible / google(gemini)/ anthropic(claude)
//! 全部走 SSE 流式,支持自定义 BaseURL。错误作为流内的 item 返回。

use futures_util::Stream;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
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
    // DeepSeek V4 默认开启思考模式:输出预算会先被 reasoning_content 吃掉,而本解析器
    // 只读 delta.content → 聚合为空 → 判定「LLM 分析失败」。对 DeepSeek 端点显式禁用思考。
    if base.to_lowercase().contains("deepseek") {
        body["thinking"] = json!({ "type": "disabled" });
    }

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

    let lines = sse_lines(resp.bytes_stream());
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

    let lines = sse_lines(resp.bytes_stream());
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

    let lines = sse_lines(resp.bytes_stream());
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

/// 将 SSE 字节流切分为 `data:…` 行。
///
/// 泛型化只为可测:字节块来源可以是 reqwest 的响应流,也可以是测试里手搓的
/// `Vec<Vec<u8>>`。真实故障(单块多行只吐一行)只有喂固定分块序列才能复现,
/// 绑死 `reqwest::Response` 就永远测不到。
///
/// 两个必须成立、且旧实现都违反了的性质:
/// 1. **一个网络块里可能有任意多条 `data:` 行,必须全部吐出。** 旧实现每次 poll
///    只返回一行,其余压在缓冲里等下一个块;流一结束这些残余就被整坨交给
///    `serde_json` → 解析失败 → 静默丢弃。实测表现为响应稳定停在 JSON 中间。
/// 2. **按字节缓冲、按 `b'\n'` 切行。** UTF-8 自同步,0x0A 只可能是行终止符、
///    绝不可能是多字节序列的后续字节,所以这样切永远不会劈开汉字。
///    旧实现先 `from_utf8_lossy` 再找换行,跨块的多字节字符直接变成 U+FFFD。
fn sse_lines<S, B, E>(src: S) -> impl Stream<Item = anyhow::Result<String>> + Send
where
    S: Stream<Item = Result<B, E>> + Send + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    futures_util::stream::unfold(
        (src, Vec::<u8>::new(), VecDeque::<String>::new()),
        |(mut bytes, mut buf, mut ready)| async move {
            // 先把已切好的整行吐完,再等下一个网络块
            if let Some(line) = ready.pop_front() {
                return Some((Ok(line), (bytes, buf, ready)));
            }
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(chunk) => {
                        buf.extend_from_slice(chunk.as_ref());
                        drain_data_lines(&mut buf, &mut ready);
                        if let Some(line) = ready.pop_front() {
                            return Some((Ok(line), (bytes, buf, ready)));
                        }
                        // 本块没有完整行,继续累积
                    }
                    Err(e) => {
                        return Some((Err(anyhow::anyhow!("流解码失败:{e}")), (bytes, buf, ready)))
                    }
                }
            }
            // 流结束:最后一块可能不带结尾换行,按行冲刷残留
            if !buf.is_empty() {
                let tail = std::mem::take(&mut buf);
                for seg in tail.split(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(seg).trim().to_string();
                    if line.starts_with("data:") {
                        ready.push_back(line);
                    }
                }
            }
            let line = ready.pop_front()?;
            Some((Ok(line), (bytes, buf, ready)))
        },
    )
}

/// 从字节缓冲里切出所有完整行,保留 `data:` 开头的;非完整行留在缓冲等下一块
fn drain_data_lines(buf: &mut Vec<u8>, ready: &mut VecDeque<String>) {
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
        // 去掉刚吃进来的那个 \n;行内其余字节必为完整 UTF-8 序列
        let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
            .trim()
            .to_string();
        if line.starts_with("data:") {
            ready.push_back(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把若干「网络块」包成流。每个元素模拟一次 TCP 写,
    /// 分块边界完全由测试控制 —— 这正是真实故障的触发条件。
    fn chunks(v: Vec<&str>) -> impl Stream<Item = Result<Vec<u8>, std::io::Error>> + Send {
        futures_util::stream::iter(
            v.into_iter()
                .map(|s| Ok(s.as_bytes().to_vec()))
                .collect::<Vec<_>>(),
        )
    }

    async fn lines_of(body: &str) -> Vec<String> {
        sse_lines(chunks(vec![body]))
            .map(|r| r.expect("SSE 行不应报错"))
            .collect::<Vec<_>>()
            .await
    }

    fn delta_content(line: &str) -> String {
        let payload = line.strip_prefix("data:").unwrap_or(line).trim();
        serde_json::from_str::<Value>(payload)
            .ok()
            .and_then(|v| {
                v.pointer("/choices/0/delta/content")
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    /// 服务器把最后几个 delta 与 `[DONE]` 攒进**同一个** TCP 写里发出,是 SSE 的常态。
    /// 此时 `sse_lines` 必须把块内每一条 `data:` 行都吐出来。
    #[tokio::test]
    async fn multi_line_chunk_must_emit_every_data_line() {
        let d1 = r#"data: {"choices":[{"delta":{"content":"{\"framework\":\"Vue 3\""}}]}"#;
        let d2 =
            r#"data: {"choices":[{"delta":{"content":",\"techStack\":[\"Vite\",\"Rust\"]}"}}]}"#;
        let body = format!("{d1}\n\n{d2}\n\ndata: [DONE]\n\n");

        let lines = lines_of(&body).await;
        assert_eq!(
            lines.len(),
            3,
            "一个块里有 3 条 data 行,应全部吐出;实际只拿到 {}\n实际内容:{lines:#?}",
            lines.len()
        );

        // 端到端判据:增量拼回去必须是完整可解析的 JSON(真实故障就断在这里)
        let joined: String = lines.iter().map(|l| delta_content(l)).collect();
        let facts: Value = serde_json::from_str(&joined)
            .unwrap_or_else(|e| panic!("拼回的正文不是合法 JSON:{e}\n正文:{joined:?}"));
        assert_eq!(facts["framework"], "Vue 3");
        assert_eq!(facts["techStack"].as_array().unwrap().len(), 2);
    }

    /// 尾块没有结尾换行时(连接直接关闭),缓冲区里剩下的多行同样不能丢
    #[tokio::test]
    async fn final_chunk_without_trailing_newline_must_not_lose_lines() {
        let d1 = r#"data: {"choices":[{"delta":{"content":"AB"}}]}"#;
        let d2 = r#"data: {"choices":[{"delta":{"content":"CD"}}]}"#;
        let body = format!("{d1}\n\n{d2}");

        let lines = lines_of(&body).await;
        let joined: String = lines.iter().map(|l| delta_content(l)).collect();
        assert_eq!(
            joined, "ABCD",
            "尾块多行被整坨交给解析器就会丢内容:{lines:#?}"
        );
    }

    /// 一个 UTF-8 多字节字符被劈在两个块之间时,不能解码成 U+FFFD。
    /// 注意:这里必须在**字节层**切,不能先 lossy 解码再传 —— 那等于测试自己把数据破坏了。
    #[tokio::test]
    async fn multibyte_char_split_across_chunks_must_survive() {
        let full = r#"data: {"choices":[{"delta":{"content":"框架"}}]}"#;
        let bytes = full.as_bytes();
        // 「框」占 3 字节,故意切在它内部
        let at = full.find('框').expect("样本里应有「框」");
        let cut = at + 1;

        let a = bytes[..cut].to_vec();
        let b = bytes[cut..].to_vec();
        let src =
            futures_util::stream::iter(vec![Ok(a), Ok(b)] as Vec<Result<Vec<u8>, std::io::Error>>);

        let lines: Vec<String> = sse_lines(src)
            .map(|r| r.expect("SSE 行不应报错"))
            .collect()
            .await;
        let joined: String = lines.iter().map(|l| delta_content(l)).collect();
        assert_eq!(
            joined, "框架",
            "跨块多字节被 lossy 解码会留下替换符:{joined:?}"
        );
    }
}
