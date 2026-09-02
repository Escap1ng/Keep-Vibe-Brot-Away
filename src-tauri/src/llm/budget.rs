//! 每日 LLM 调用预算:默认 80 次/日,超限进入"攒批"降级
//! (门控通过的项暂存 core.pending_batch,额度恢复后由维护任务补发分析)。

use std::sync::Arc;

use crate::state::{local_date, now_ts, AppState, BatchItem, Core, DailyBudget};

/// 预算耗尽的专用错误前缀(上层据此判定"进入攒批"而非普通失败)
pub const BUDGET_EXCEEDED_PREFIX: &str = "[budget-exceeded]";

/// 攒批上限与保鲜期
pub const BATCH_MAX: usize = 20;
pub const BATCH_TTL_SECS: i64 = 24 * 3600;

#[derive(Debug)]
pub enum BudgetErr {
    Exceeded,
}

/// 检查并登记一次调用(原子:core 锁内完成计数,锁外落盘)
pub fn check_then_inc(state: &Arc<AppState>) -> Result<(), BudgetErr> {
    let (budget, granted) = {
        let mut core = state.core.blocking_lock();
        if core.budget.date != local_date() {
            // 跨日:归零额度、重置"预算耗尽提示"标记,并丢弃已攒的陈旧批次
            core.budget = DailyBudget::default();
            core.budget_toast_shown = false;
            core.pending_batch
                .retain(|b| now_ts() - b.created_at < BATCH_TTL_SECS);
        }
        // 统计计数同步跨日归并
        core.stats.roll_to_today();
        let granted = if core.budget.used >= core.settings.daily_budget {
            core.budget.rejected += 1;
            false
        } else {
            core.budget.used += 1;
            true
        };
        (core.budget.clone(), granted)
    };
    let _ = std::fs::write(
        state.data_dir.join("budget.json"),
        serde_json::to_string(&budget).unwrap_or_default(),
    );
    if granted {
        Ok(())
    } else {
        Err(BudgetErr::Exceeded)
    }
}

/// 攒一批:登记门控通过但没调成 LLM 的项(同主题去重,超上限丢最旧)
pub fn push_batch(core: &mut Core, item: BatchItem) {
    core.pending_batch
        .retain(|b| now_ts() - b.created_at < BATCH_TTL_SECS);
    let fp = |b: &BatchItem| crate::gate::fingerprint(b.pass.branch, &b.pass.theme_hint, "");
    let mine = fp(&item);
    if core.pending_batch.iter().any(|b| fp(b) == mine) {
        return;
    }
    if core.pending_batch.len() >= BATCH_MAX {
        core.pending_batch.remove(0);
    }
    log::info!(
        "攒批:暂存门控通过项(主题={},累计 {} 项)",
        item.pass.theme_hint,
        core.pending_batch.len() + 1
    );
    core.pending_batch.push(item);
}

/// 尝试消费一条攒批(维护任务每 60s 调用):
/// 未暂停 + 队列不满 + 有额度 + 不在 60s 弹卡频控内 → 取出最旧项异步分析并入队。
pub fn try_drain_batch(state: &Arc<AppState>) {
    let item = {
        let core = state.core.blocking_lock();
        if core.paused || core.pending_batch.is_empty() {
            return;
        }
        let quota_ok =
            core.budget.date == local_date() && core.budget.used < core.settings.daily_budget;
        let queue_ok =
            core.queue.iter().filter(|c| c.status == "pending").count() < crate::queue::MAX_PENDING;
        let freq_ok = core
            .last_card_at
            .map(|t| now_ts() - t >= crate::gate::LAST_CARD_WINDOW_SECS)
            .unwrap_or(true);
        if !quota_ok || !queue_ok || !freq_ok {
            return;
        }
        Some(core.pending_batch[0].clone())
    };
    let Some(item) = item else { return };
    // 先移出缓冲,防止并发重复消费;预算再次不足时会放回
    {
        let mut core = state.core.blocking_lock();
        core.pending_batch
            .retain(|b| !(b.created_at == item.created_at && b.project_path == item.project_path));
    }
    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        let pass = item.pass.clone();
        match crate::llm::analyze_pass_for_project(&state, &pass, &item.project_path).await {
            Ok(analysis) => {
                let inserted = crate::queue::push_pass_card(
                    &state.app,
                    &state,
                    &pass,
                    analysis,
                    item.ref_event_id.clone(),
                );
                log::info!(
                    "攒批消费:{}(主题={})",
                    if inserted {
                        "成功入队"
                    } else {
                        "未入队(队列满/重复)"
                    },
                    pass.theme_hint
                );
            }
            Err(e) => {
                if e.starts_with(BUDGET_EXCEEDED_PREFIX) {
                    let mut core = state.core.blocking_lock();
                    core.pending_batch.insert(0, item);
                    crate::state::save_core_locked(&state.data_dir, &core);
                } else {
                    log::warn!("攒批消费失败,丢弃该项:{e}");
                }
            }
        }
    });
}

/// `budget_info` 的响应契约(与前端 lib/types.ts 的 BudgetInfo 一一对应)
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetInfo {
    pub used: u64,
    pub limit: u64,
    pub rejected: u64,
}

/// 设置面板查询:今日已用 / 上限 / 被拒次数
#[tauri::command]
pub fn budget_info(state: tauri::State<'_, Arc<AppState>>) -> BudgetInfo {
    let core = state.core.blocking_lock();
    BudgetInfo {
        used: core.budget.used,
        limit: core.settings.daily_budget,
        rejected: core.budget.rejected,
    }
}
