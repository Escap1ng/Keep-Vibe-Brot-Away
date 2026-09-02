// KVBA 前端共享类型(与 Rust 侧 serde camelCase 对齐)

/** 待答队列上限(与 Rust queue::MAX_PENDING 一致) */
export const MAX_PENDING = 3;
/** 优化提示词生成中的占位(与 Rust queue::PROMPT_PENDING_MARK 一致) */
export const PROMPT_PENDING_MARK = "(生成中)";
/** 每日 LLM 调用上限默认值(与 Rust Settings::default 一致) */
export const DEFAULT_DAILY_BUDGET = 80;

export interface Card {
  id: string;
  type: "review" | "tips" | "awareness";
  severity: "info" | "P1" | "P0";
  text: string;
  options: string[];
  theme: string;
  refFiles: string[];
  refEventId: string;
  createdAt: number;
  status: "pending" | "answered" | "dismissed" | "stale";
  answer: string;
  optimizedPrompt: string;
  projectPath: string;
}

export interface Settings {
  provider: "openai" | "google" | "anthropic";
  model: string;
  baseUrl: string | null;
  dailyBudget: number;
  paused: boolean;
  autostart: boolean;
  p0ReplaceQueue: boolean;
  dockX: number | null;
  dockY: number | null;
  capsuleHidden: boolean;
}

export interface DecisionEntry {
  ts: number;
  level: string;
  pass: boolean;
  reason: string;
  branch: string | null;
  score: number | null;
  elapsedMs: number;
  project: string;
}

export interface Facts {
  framework: string;
  techStack: string[];
  dirSummary: string;
  keyEntryPoints: string[];
  decisions: string[];
  awarenessFacts: string[];
  generatedAt: number;
}

export interface BudgetInfo {
  used: number;
  limit: number;
  rejected: number;
}

export interface EndpointInfo {
  port: number;
  helloToken: string;
  eventToken: string;
}

/** 运行统计(§1.5 成功度量本地埋点):当日计数 + 历史累计 */
export interface StatsInfo {
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
}
