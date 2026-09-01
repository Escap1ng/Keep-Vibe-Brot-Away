//! 每日 LLM 调用预算:默认 80 次/日,超限进入"攒批"降级(只累计信号,不调 LLM)

use std::sync::Arc;

use crate::state::{local_date, AppState};

#[derive(Debug)]
pub enum BudgetErr {
    Exceeded,
}

/// 检查并登记一次调用(原子:core 锁内完成)
pub fn check_then_inc(state: &Arc<AppState>) -> Result<(), BudgetErr> {
    let mut core = state.core.blocking_lock();
    let today = local_date();
    if core.budget.date != today {
        core.budget.date = today;
        core.budget.used = 0;
        core.budget.rejected = 0;
    }
    if core.budget.used >= core.settings.daily_budget {
        core.budget.rejected += 1;
        let _ = std::fs::write(
            state.data_dir.join("budget.json"),
            serde_json::to_string(&core.budget).unwrap_or_default(),
        );
        return Err(BudgetErr::Exceeded);
    }
    core.budget.used += 1;
    let _ = std::fs::write(
        state.data_dir.join("budget.json"),
        serde_json::to_string(&core.budget).unwrap_or_default(),
    );
    Ok(())
}

/// 今日剩余次数(设置面板展示)
pub fn remaining(state: &Arc<AppState>) -> (u64, u64) {
    let core = state.core.blocking_lock();
    (core.budget.used, core.settings.daily_budget)
}

/// 设置面板查询:今日已用 / 上限
#[tauri::command]
pub fn budget_info(state: tauri::State<'_, Arc<AppState>>) -> serde_json::Value {
    let core = state.core.blocking_lock();
    serde_json::json!({
        "used": core.budget.used,
        "limit": core.settings.daily_budget,
        "rejected": core.budget.rejected,
    })
}
