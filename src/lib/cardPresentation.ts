import type { Card, InsightType, RiskLevel } from "./types";

type LegacyCardType = Card["type"];

interface KindPresentation {
  label: string;
  className: LegacyCardType;
}

const LEGACY_KIND_PRESENTATION = {
  review: { label: "审查", className: "review" },
  tips: { label: "知识", className: "tips" },
  awareness: { label: "知情度", className: "awareness" },
} satisfies Record<LegacyCardType, KindPresentation>;

const INSIGHT_KIND_PRESENTATION = {
  risk: { label: "风险", className: "review" },
  decision: { label: "待确认", className: "awareness" },
  "test-gap": { label: "测试缺口", className: "review" },
  explain: { label: "本轮变更", className: "tips" },
  question: { label: "问题", className: "awareness" },
} satisfies Record<InsightType, KindPresentation>;

const SEVERITY_LABELS = {
  info: "信息",
  P2: "注意",
  P1: "重要",
  P0: "紧急",
} satisfies Record<RiskLevel, string>;

function ownValue<T>(record: Record<string, T>, key: string): T | undefined {
  return Object.prototype.hasOwnProperty.call(record, key) ? record[key] : undefined;
}

export function cardKindPresentation(card: Card): KindPresentation {
  const presentation = card.insight
    ? ownValue(INSIGHT_KIND_PRESENTATION, card.insight.type)
    : ownValue(LEGACY_KIND_PRESENTATION, card.type);
  return presentation ?? LEGACY_KIND_PRESENTATION.tips;
}

export function cardRiskLevel(card: Card): RiskLevel {
  const level = card.insight?.riskLevel ?? card.severity;
  return ownValue(SEVERITY_LABELS, level) === undefined ? "info" : level;
}

export function cardSeverityLabel(card: Card): string {
  return SEVERITY_LABELS[cardRiskLevel(card)];
}

export function cardSeverityClass(card: Card): `sev-${Lowercase<RiskLevel>}` {
  return `sev-${cardRiskLevel(card).toLowerCase()}` as `sev-${Lowercase<RiskLevel>}`;
}

export function cardDisplayText(card: Card): string {
  return card.insight?.conclusion ?? card.text;
}

export function cardShowsReferenceFiles(card: Card): boolean {
  return (
    card.refFiles.length > 0 &&
    !card.insight?.evidence.some((evidence) => evidence.kind === "file")
  );
}
