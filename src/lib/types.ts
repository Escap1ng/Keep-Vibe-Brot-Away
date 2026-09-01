// KVBA 前端共享类型(与 Rust 侧 serde camelCase 对齐)

export type CapsuleView = "idle" | "active" | "popup";

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
