// 命令封装(统一 camelCase 参数 → Rust snake_case)
import { invoke } from "@tauri-apps/api/core";
import type {
  BudgetInfo,
  Card,
  DecisionEntry,
  EndpointInfo,
  Facts,
  Settings,
} from "./types";

export const api = {
  settingsGet: () => invoke<Settings>("settings_get"),
  settingsPut: (patch: Partial<Settings>) => invoke<void>("settings_put", { patch }),
  queueListen: () => invoke<Card[]>("queue_listen"),
  queueAnswer: (cardId: string, answer: string) =>
    invoke<Card>("queue_answer", { cardId, answer }),
  queueDismiss: (cardId: string) => invoke<void>("queue_dismiss", { cardId }),
  queueMuteTheme: (cardId: string) => invoke<void>("queue_mute_theme", { cardId }),
  gateTick: (force = true) => invoke<string>("gate_tick", { force }),
  decisionLogList: () => invoke<DecisionEntry[]>("decision_log_list"),
  historyList: () => invoke<Card[]>("history_list"),
  factsGet: () => invoke<Facts | null>("facts_get"),
  endpointInfo: () => invoke<EndpointInfo>("endpoint_info"),
  budgetInfo: () => invoke<BudgetInfo>("budget_info"),
  llmTest: () => invoke<string>("llm_test_connection"),
  statsInfo: () =>
    invoke<{
      lastEventIn: number | null;
      activeProject: string;
      cardsCreated: number;
      answers: number;
      unsatisfied: number;
      todayAnswerRate: number;
      total: {
        cardsCreated: number;
        answers: number;
        unsatisfied: number;
        answerRate: number;
      };
      batched: number;
    }>("stats_info"),
  secretsPut: (provider: string, key: string) =>
    invoke<void>("secrets_put", { provider, key }),
  secretsTest: () => invoke<string[]>("secrets_test"),
  dockSnap: (x: number, y: number) => invoke<[number, number]>("dock_snap", { x, y }),
  panelOpen: (mode: "active" | "popup") => invoke<void>("panel_open", { mode }),
  panelClose: () => invoke<void>("panel_close"),
  capsuleHide: () => invoke<void>("capsule_hide"),
  capsuleShow: () => invoke<void>("capsule_show"),
  settingsOpen: () => invoke<void>("settings_open"),
  capsuleSetState: (capsuleState: string) =>
    invoke<void>("capsule_set_state", { capsuleState }),
};
