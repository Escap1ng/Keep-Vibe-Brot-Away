//! 本地 HTTP 端点(127.0.0.1 随机端口 + 随机 token 鉴权):
//! - POST /kvba/hello:适配器用主 token 换取事件上报 token
//! - POST /kvba/events:事件上报(header X-KVBA-Token: eventToken)
//! 端点信息写入 <dataDir>/endpoint.json 供适配器读取。

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use rand::RngCore;
use serde_json::json;
use tauri::Manager;

use crate::events::{EventStore, NormalizedEvent};
use crate::state::{AppState, EndpointInfo};

#[derive(Clone)]
struct Tokens {
    hello: String,
    event: String,
}

type Shared = (Arc<AppState>, Arc<Tokens>);

/// 启动服务(异步任务;端口随机,信息落盘)
pub fn start_server(handle: &tauri::AppHandle) {
    let state = handle.state::<Arc<AppState>>().inner().clone();
    tauri::async_runtime::spawn(async move {
        let hello = rand_token();
        let event_token = rand_token();
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("HTTP 端口绑定失败:{e}");
                return;
            }
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let info = EndpointInfo {
            port,
            hello_token: hello.clone(),
            event_token: event_token.clone(),
        };
        let _ = std::fs::write(
            state.data_dir.join("endpoint.json"),
            serde_json::to_string(&info).unwrap_or_default(),
        );
        log::info!("KVBA 本地端点 127.0.0.1:{port} 已就绪");

        let router = Router::new()
            .route("/kvba/hello", post(hello_handler))
            .route("/kvba/events", post(events_handler))
            .with_state((state, Arc::new(Tokens { hello, event: event_token })));
        if let Err(e) = axum::serve(listener, router).await {
            log::error!("HTTP 服务退出:{e}");
        }
    });
}

fn rand_token() -> String {
    let mut b = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

async fn hello_handler(
    State((_state, tokens)): State<Shared>,
    headers: HeaderMap,
) -> impl IntoResponse {
    match headers.get("X-KVBA-Token").and_then(|v| v.to_str().ok()) {
        Some(t) if t == tokens.hello => Json(json!({ "token": tokens.event })).into_response(),
        _ => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    }
}

async fn events_handler(
    State((state, tokens)): State<Shared>,
    headers: HeaderMap,
    ev: Json<NormalizedEvent>,
) -> impl IntoResponse {
    if headers.get("X-KVBA-Token").and_then(|v| v.to_str().ok()) != Some(tokens.event.as_str()) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    let ev = ev.0;
    let store = EventStore::new();
    match store.ingest(&state, ev) {
        Ok(sanitized) => {
            // 项目首次出现事件 → 生成 facts 首版
            crate::memory::kick_facts_if_needed(&state, &sanitized.project_path);
            // 会话结束 → 增量记忆合并(有消费记录时)
            if sanitized.event_type == "session.ended" {
                crate::memory::session_merge(&state, &sanitized.project_path);
            }
            // 触发门控评估(异步管线)
            pipeline_run(&state, false);
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("bad event:{e}")).into_response(),
    }
}

/// 管线结局(供"来一道"命令回传真实结果)
#[derive(Debug)]
pub enum PipelineOutcome {
    CardCreated,
    Rejected(String, String), // (层级, 原因)
    Paused,
    NoProject,
}

/// 门控评估管线:聚合近 90s 事件批 → 纯函数评估 → (G7) LLM 分析 → 入队 → 胶囊展开
/// 锁纪律:任务内部只在同步子作用域持有锁,任何 await 点都不存在未释放的守卫。
pub fn pipeline_run(state: &Arc<AppState>, force: bool) {
    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        let outcome = run_pipeline_body(state, force).await;
        if let PipelineOutcome::Rejected(l, r) = &outcome {
            log::debug!("自动触发被拦下[{}]:{}", l, r);
        }
    });
}

pub async fn run_pipeline_body(state: Arc<AppState>, force: bool) -> PipelineOutcome {
    let t0 = std::time::Instant::now();

    // 阶段 1:同步快照(锁内;作用域结束即释放)
    let snapshot = {
        let core = state.core.blocking_lock();
        if core.paused && !force {
            log::debug!("暂停中,跳过门控");
            return PipelineOutcome::Paused;
        }
        let Some(key) = core.active_project.clone() else {
            log::debug!("无活动项目,跳过门控");
            return PipelineOutcome::NoProject;
        };
        let Some(live) = core.projects.get(&key) else {
            return PipelineOutcome::NoProject;
        };
        let project_path = live
            .events
            .front()
            .map(|e| e.project_path.clone())
            .unwrap_or_default();
        let now = crate::state::now_ts();
        let batch: Vec<NormalizedEvent> = live
            .events
            .iter()
            .filter(|e| now - e.ts < 90)
            .cloned()
            .collect();
        let queue_len = core.queue.iter().filter(|c| c.status == "pending").count();
        let pending: Vec<_> = core.queue.iter().cloned().collect();
        let last_card_at = core.last_card_at;
        let facts = live.facts.clone();
        let cooldowns = core.cooldowns.clone();
        let history = core.history.clone();
        let last_event_id = live.events.back().map(|e| e.id.clone()).unwrap_or_default();
        crate::state::PipelineSnapshot {
            project_path,
            now,
            batch,
            queue_len,
            pending,
            last_card_at,
            facts,
            cooldowns,
            history,
            last_event_id,
        }
    };

    let mut snapshot = snapshot;
    let ctx = crate::gate::GateContext {
        events: std::mem::take(&mut snapshot.batch),
        now: snapshot.now,
        queue_len: snapshot.queue_len,
        last_card_at: snapshot.last_card_at,
        cooldowns: &snapshot.cooldowns,
        facts: snapshot.facts.as_ref(),
        pending_cards: &snapshot.pending,
        history: &snapshot.history,
        force,
    };
    let (verdict, scoring) = crate::gate::evaluate(&ctx);
    let elapsed = t0.elapsed().as_millis() as u64;

    log::info!(
        "门控评估完成:events={} queue={} pass={}",
        snapshot.batch.len(),
        snapshot.queue_len,
        matches!(verdict, crate::gate::GateVerdict::Pass(_))
    );
    // 阶段 2:决策日志(同步作用域)
    {
        let mut core = state.core.blocking_lock();
        let entry = crate::gate::DecisionEntry {
            ts: crate::state::now_ts(),
            level: match &verdict {
                crate::gate::GateVerdict::Pass(_) => "G7".into(),
                crate::gate::GateVerdict::Reject(l, _) => l.as_str().into(),
            },
            pass: matches!(verdict, crate::gate::GateVerdict::Pass(_)),
            reason: match &verdict {
                crate::gate::GateVerdict::Pass(p) => p.reason.clone(),
                crate::gate::GateVerdict::Reject(_, r) => r.clone(),
            },
            branch: match &verdict {
                crate::gate::GateVerdict::Pass(p) => Some(format!("{:?}", p.branch)),
                _ => None,
            },
            score: Some(scoring.score),
            elapsed_ms: elapsed,
            project: snapshot.project_path.clone(),
        };
        core.decision_log.push_back(entry.clone());
        while core.decision_log.len() > 200 {
            core.decision_log.pop_front();
        }
        crate::state::save_core_locked(&state.data_dir, &core);
        crate::state::emit(&state.app, "decision.new", &entry);
    }

    let pass = match verdict {
        crate::gate::GateVerdict::Pass(p) => p,
        crate::gate::GateVerdict::Reject(level, reason) => {
            log::debug!("门控拒绝 {}:{}", level.as_str(), reason);
            return PipelineOutcome::Rejected(level.as_str().into(), reason);
        }
    };

    // G7 执行:仅此处调用 LLM(单次 ≤5k)。未配置 → 模板卡降级;预算超限 → 攒批(不调用)
    let analysis = if crate::llm::is_configured(&state) {
        match crate::llm::analyze_pass(&state, &pass).await {
            Ok(out) => Some(out),
            Err(e) => {
                log::warn!("LLM 分析失败:{e}");
                return PipelineOutcome::Rejected(
                    "G7".to_string(),
                    format!("LLM 分析失败:{e}"),
                );
            }
        }
    } else {
        Some(crate::llm::AnalysisOut::fallback(&pass, &snapshot.project_path))
    };

    let inserted = crate::queue::push_pass_card(
        &state.app,
        &state,
        &pass,
        analysis.unwrap(),
        snapshot.last_event_id,
    );
    if inserted {
        PipelineOutcome::CardCreated
    } else {
        PipelineOutcome::Rejected(
            "G1".to_string(),
            "队列已满或同主题重复,卡片未入队".into(),
        )
    }
}
