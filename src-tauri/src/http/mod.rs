//! 本地 HTTP 端点(127.0.0.1 随机端口 + 随机 token 鉴权):
//!
//! - `POST /kvba/hello`:适配器用主 token 换取事件上报 token
//! - `POST /kvba/events`:事件上报(header `X-KVBA-Token: eventToken`)
//!
//! 端点信息写入 `<dataDir>/endpoint.json` 供适配器读取。

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
        // 内存里留一份真实端点(设置面板诊断用),文件只是给外部适配器发现用
        let _ = state.endpoint.set(info.clone());
        let endpoint_file = state.data_dir.join("endpoint.json");
        let published = serde_json::to_string(&info)
            .map_err(|e| e.to_string())
            .and_then(|text| std::fs::write(&endpoint_file, text).map_err(|e| e.to_string()));
        // 写失败 = 所有适配器都再也发现不了端点,必须留痕而不是静默降级
        if let Err(e) = published {
            log::error!(
                "写入 {} 失败:{e} —— 适配器将无法发现本地端点(KVBA 本体仍可用,端口 {port})",
                endpoint_file.display()
            );
        }
        log::info!("KVBA 本地端点 127.0.0.1:{port} 已就绪");

        let router = Router::new()
            .route("/kvba/hello", post(hello_handler))
            .route("/kvba/events", post(events_handler))
            .with_state((
                state,
                Arc::new(Tokens {
                    hello,
                    event: event_token,
                }),
            ));
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
    match EventStore::ingest(&state, ev) {
        Ok(Some(sanitized)) => {
            // 项目首次出现事件 → 生成 facts 首版
            crate::memory::kick_facts_if_needed(&state, &sanitized.project_path);
            // 会话结束 → 增量记忆合并(有消费记录时)
            if sanitized.event_type == "session.ended" {
                crate::memory::session_merge(&state, &sanitized.project_path);
            }
            // 自动路径以 Agent 回合为边界。普通事件只参与聚合，不再逐条竞争发卡；
            // watcher 暂无 turn.ended，保留旧路径直到它能提供可靠边界。
            match crate::workflow::automatic_route(&sanitized) {
                crate::workflow::AutomaticRoute::WorkUnit => {
                    crate::workflow::schedule_boundary(&state, &sanitized)
                }
                crate::workflow::AutomaticRoute::LegacyWatcher => pipeline_run(&state, false),
                crate::workflow::AutomaticRoute::AwaitBoundary => {}
            }
            Json(json!({ "ok": true })).into_response()
        }
        Ok(None) => {
            // bridge 与 watcher 同开时同一动作会报两遍:静默 ACK,不重复计信号
            Json(json!({ "ok": true, "duplicate": true })).into_response()
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
        let mut core = state.core.blocking_lock();
        if core.paused && !force {
            log::debug!("暂停中,跳过门控");
            return PipelineOutcome::Paused;
        }
        let Some(key) = core.active_project.clone() else {
            log::debug!("无活动项目,跳过门控");
            return PipelineOutcome::NoProject;
        };
        // 开场破冰:该 session 还没递过话,就允许它旁路一次 G2/G6。
        // **判定即记账** —— 一次评估只破冰一次,否则每条事件都拿着旁路令牌,
        // 频控形同虚设(那正是规格 1.3 节要避免的"弹窗广告")。
        // force(手动"来一道")不吃这张令牌:用户主动请求不该把会话的开场机会用掉。
        // 单独成块:这里要可变借用 projects,不能和下面的只读快照共用作用域。
        let opener = {
            match core.projects.get_mut(&key) {
                Some(live) => {
                    let session = live
                        .events
                        .back()
                        .map(|e| e.session_id.clone())
                        .unwrap_or_default();
                    let hit = !force
                        && !session.is_empty()
                        && live.opener_session.as_deref() != Some(session.as_str());
                    if hit {
                        live.opener_session = Some(session);
                    }
                    hit
                }
                None => false,
            }
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
            .filter(|e| now - e.ts < crate::gate::ACTIVITY_WINDOW_SECS)
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
            opener,
            last_event_id,
        }
    };

    // 阶段 2:纯函数评估(快照各字段独立借用,batch 直接移交所有权)
    let batch_len = snapshot.batch.len();
    let ctx = crate::gate::GateContext {
        events: snapshot.batch,
        now: snapshot.now,
        queue_len: snapshot.queue_len,
        last_card_at: snapshot.last_card_at,
        cooldowns: &snapshot.cooldowns,
        facts: snapshot.facts.as_ref(),
        pending_cards: &snapshot.pending,
        history: &snapshot.history,
        force,
        opener: snapshot.opener,
    };
    let (verdict, scoring) = crate::gate::evaluate(&ctx);
    let elapsed = t0.elapsed().as_millis() as u64;

    log::info!(
        "门控评估完成:events={} queue={} pass={}",
        batch_len,
        snapshot.queue_len,
        matches!(verdict, crate::gate::GateVerdict::Pass(_))
    );

    // 阶段 2:门控拒绝 → 立即记录;通过 → 待 G7 真实结局后记录(日志与结果一致)
    let pass = match verdict {
        crate::gate::GateVerdict::Pass(p) => p,
        crate::gate::GateVerdict::Reject(level, reason) => {
            log::debug!("门控拒绝 {}:{}", level.as_str(), reason);
            log_decision(
                &state,
                &snapshot.project_path,
                level.as_str(),
                false,
                &reason,
                None,
                Some(scoring.score),
                elapsed,
            );
            return PipelineOutcome::Rejected(level.as_str().into(), reason);
        }
    };

    // G7 执行:仅此处调用 LLM(单次 ≤5k)。未配置 → 模板卡降级;预算超限 → 攒批(暂存待补发)
    let analysis = if crate::llm::is_configured(&state) {
        match crate::llm::analyze_pass(&state, &pass).await {
            Ok(out) => out,
            Err(e) => {
                log::warn!("LLM 分析失败:{e}");
                let budgeted = e.starts_with(crate::llm::budget::BUDGET_EXCEEDED_PREFIX);
                if budgeted {
                    // 攒批:保存门控通过快照,额度恢复后由维护任务补发分析
                    let mut core = state.core.blocking_lock();
                    crate::llm::budget::push_batch(
                        &mut core,
                        crate::state::BatchItem {
                            pass: pass.clone(),
                            project_path: snapshot.project_path.clone(),
                            ref_event_id: snapshot.last_event_id.clone(),
                            created_at: crate::state::now_ts(),
                        },
                    );
                    crate::state::save_core_locked(&state.data_dir, &core);
                }
                let reason = if budgeted {
                    "预算耗尽 → 已攒批,额度恢复后补发".to_string()
                } else {
                    // 必须带上真实原因:端点/模型/密钥错配是最常见故障,
                    // 只写"LLM 分析失败"等于把诊断成本全推给控制台。
                    let mut detail: String = e.chars().take(180).collect();
                    if detail.chars().count() < e.chars().count() {
                        detail.push('…');
                    }
                    format!("LLM 分析失败:{detail}")
                };
                log_decision(
                    &state,
                    &snapshot.project_path,
                    "G7",
                    false,
                    &reason,
                    Some(&pass.branch),
                    Some(scoring.score),
                    elapsed,
                );
                return PipelineOutcome::Rejected(
                    "G7".to_string(),
                    if budgeted {
                        "已攒批(今日预算耗尽)".into()
                    } else {
                        e
                    },
                );
            }
        }
    } else {
        crate::llm::AnalysisOut::fallback(&pass, &snapshot.project_path)
    };

    // 出稿来源要在入队前取走:analysis 会被 move 进 push_pass_card。
    // 不记这一位,「G7 pass=true 卡片生成并入队」对 LLM 出稿和模板兜底完全同词,
    // 绿灯永远无法证伪 —— 实测曾有 5 张卡里 4 张是模板原文而无人察觉。
    let from_llm = analysis.from_llm;
    let queued =
        crate::queue::push_pass_card(&state.app, &state, &pass, analysis, snapshot.last_event_id);
    // 三种结局必须分开陈述:混成"队列已满或同主题重复"会把人引向错误的自救动作
    let (log_reason, level, user_reason) = match queued {
        crate::queue::InsertOutcome::Inserted => (
            if from_llm {
                "卡片生成并入队(LLM 出稿)"
            } else {
                "卡片生成并入队(模板兜底,LLM 未出稿)"
            },
            "G7",
            String::new(),
        ),
        crate::queue::InsertOutcome::QueueFull => (
            "入队被拒:队列已满(pending=3)",
            "G1",
            "队列已满:先回答或跳过一张卡片,再来一道".into(),
        ),
        crate::queue::InsertOutcome::Duplicate => (
            "入队被拒:正文与 20min 内已递出的卡片雷同",
            "G6",
            "这个话题刚递过同款内容(近 20min)。等一会儿再来,或先让 Agent 干点新活".into(),
        ),
    };
    log_decision(
        &state,
        &snapshot.project_path,
        level,
        matches!(queued, crate::queue::InsertOutcome::Inserted),
        log_reason,
        Some(&pass.branch),
        Some(scoring.score),
        elapsed,
    );
    match queued {
        crate::queue::InsertOutcome::Inserted => PipelineOutcome::CardCreated,
        _ => PipelineOutcome::Rejected(level.into(), user_reason),
    }
}

/// 写一条决策日志:内存滚动 + JSONL 单行追加(超阈值才整写修剪),
/// 不再走 save_core_locked 全量重写五个文件,消除"每事件一次全量落盘"的写放大。
// 参数即 DecisionEntry 的字段,调用点均在管线同一函数内;抽结构体反而更啰嗦
#[allow(clippy::too_many_arguments)]
fn log_decision(
    state: &Arc<AppState>,
    project: &str,
    level: &str,
    pass: bool,
    reason: &str,
    branch: Option<&crate::gate::Branch>,
    score: Option<i32>,
    elapsed_ms: u64,
) {
    let entry = crate::gate::DecisionEntry {
        ts: crate::state::now_ts(),
        level: level.into(),
        pass,
        reason: reason.into(),
        branch: branch.map(|b| format!("{b:?}")),
        score,
        elapsed_ms,
        project: project.into(),
    };
    {
        let mut core = state.core.blocking_lock();
        core.decision_log.push_back(entry.clone());
        while core.decision_log.len() > crate::gate::DECISION_LOG_KEEP {
            core.decision_log.pop_front();
        }
    }
    if let Ok(line) = serde_json::to_string(&entry) {
        use std::io::Write;
        let path = state.data_dir.join("decision-log.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{line}");
        }
        // 滚动修剪:超过阈值行数时整写为最近 DECISION_LOG_KEEP 行
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let n = raw.lines().count();
            if n > crate::gate::DECISION_LOG_TRIM_AT {
                let keep: String = raw
                    .lines()
                    .skip(n - crate::gate::DECISION_LOG_KEEP)
                    .map(|x| format!("{x}\n"))
                    .collect();
                let _ = std::fs::write(&path, keep);
            }
        }
    }
    crate::state::emit(&state.app, "decision.new", &entry);
}
