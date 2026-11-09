<script setup lang="ts">
// 设置/历史/决策日志(三合一窗口,用完即关)
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "../lib/api";
import { cardDisplayText, cardKindPresentation } from "../lib/cardPresentation";
import { DEFAULT_DAILY_BUDGET, PROMPT_PENDING_MARK } from "../lib/types";
import type {
  BudgetInfo,
  Card,
  DecisionEntry,
  Facts,
  Settings,
  StatsInfo,
} from "../lib/types";

type TabId = "settings" | "history" | "log";

const tab = ref<TabId>("settings");

const s = ref<Settings | null>(null);
const keys = ref<Record<string, string>>({ openai: "", google: "", anthropic: "" });
const keyStatus = ref<string[]>([]);
const budget = ref<BudgetInfo | null>(null);
const facts = ref<Facts | null>(null);
const endpoint = ref<string>("查询中…");
const savedMsg = ref("");
const tabs = [
  { id: "settings", label: "设置" },
  { id: "history", label: "历史" },
  { id: "log", label: "决策日志" },
] as const;

const history = ref<Card[]>([]);
const decisions = ref<DecisionEntry[]>([]);
const stats = ref<StatsInfo | null>(null);

let msgTimer: ReturnType<typeof setTimeout> | null = null;
let unlistenFocus: (() => void) | null = null;

function noteSaved(msg: string) {
  savedMsg.value = msg;
  if (msgTimer) clearTimeout(msgTimer);
  msgTimer = setTimeout(() => (savedMsg.value = ""), 2400);
}

async function loadAll() {
  try {
    s.value = await api.settingsGet();
    keyStatus.value = await api.secretsTest();
    budget.value = await api.budgetInfo();
    facts.value = await api.factsGet();
    stats.value = await api.statsInfo();
    try {
      const ep = await api.endpointInfo();
      endpoint.value = ep.port ? `127.0.0.1:${ep.port}` : "未启动";
    } catch {
      endpoint.value = "未启动";
    }
  } catch (e) {
    noteSaved(`载入失败:${String(e)}`);
  }
}

async function loadHistory() {
  history.value = await api.historyList().catch(() => [] as Card[]);
}
async function loadDecisions() {
  decisions.value = await api.decisionLogList().catch(() => [] as DecisionEntry[]);
}

// 历史/决策日志是过程性数据,窗口开着时也在变 → 每次切到对应 Tab 都重新拉取
watch(tab, async (next) => {
  if (next === "history") await loadHistory();
  else if (next === "log") await loadDecisions();
});

async function save() {
  if (!s.value) return;
  const src = s.value;
  try {
    await api.settingsPut({
      provider: src.provider,
      model: src.model,
      baseUrl: src.baseUrl || null,
      dailyBudget: Number(src.dailyBudget) || DEFAULT_DAILY_BUDGET,
      autostart: !!src.autostart,
      p0ReplaceQueue: !!src.p0ReplaceQueue,
      paused: !!src.paused,
    });
    for (const [provider, key] of Object.entries(keys.value)) {
      if (key) await api.secretsPut(provider, key);
    }
    noteSaved("已保存");
  } catch (e) {
    noteSaved(`保存失败:${String(e)}`);
  }
  await loadAll();
}

const testing = ref(false);
async function testConnection() {
  if (!s.value) return;
  testing.value = true;
  noteSaved("测试中…");
  try {
    // 先落盘当前表单,保证测的是刚填的配置
    await save();
    const msg = await api.llmTest();
    noteSaved(`✔ ${msg}`);
  } catch (e) {
    noteSaved(`✘ ${String(e)}`);
  } finally {
    testing.value = false;
  }
}

async function copyText(text: string) {
  try {
    await navigator.clipboard.writeText(text);
    noteSaved("已复制");
  } catch {
    noteSaved("复制失败");
  }
}

function fmtTime(ts: number) {
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 相对时间:光给绝对时刻看不出"链路是不是早就断了" */
function fmtAgo(ts: number) {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (s < 60) return `${s} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return `${Math.floor(s / 86400)} 天前`;
}

/** 超过 10 分钟没有新事件,链路基本可以判定为未接入 */
const EVENT_STALE_SECS = 600;
const eventStale = computed(
  () => !stats.value?.lastEventIn || Date.now() / 1000 - stats.value.lastEventIn > EVENT_STALE_SECS,
);

const capsuleStateLabel: Record<string, string> = {
  idle: "圆点(idle)",
  active: "横幅(active)",
  popup: "队列面板(popup)",
};

onMounted(async () => {
  await loadAll();
  await loadHistory();
  await loadDecisions();
  // 设置窗口常驻且关闭时只隐藏，重新打开不会再次触发 mounted。
  // 每次获得焦点都刷新运行状态，避免显示应用启动时的陈旧快照。
  unlistenFocus = await getCurrentWindow().onFocusChanged(async ({ payload }) => {
    if (!payload) return;
    await loadAll();
    if (tab.value === "history") await loadHistory();
    else if (tab.value === "log") await loadDecisions();
  });
});

onUnmounted(() => {
  unlistenFocus?.();
  if (msgTimer) clearTimeout(msgTimer);
});
</script>

<template>
  <div class="settings">
    <nav class="tabs" role="tablist">
      <button
        v-for="t in tabs"
        :key="t.id"
        role="tab"
        class="tab"
        :class="{ on: tab === t.id }"
        @click="tab = t.id"
      >
        {{ t.label }}
      </button>
    </nav>

    <!-- 设置 -->
    <section v-if="tab === 'settings'" class="pane">
      <div class="scroller">
      <div v-if="!s" class="empty">正在载入设置…</div>
      <div v-else class="form-body">
      <section class="group">
        <h2 class="group-title">模型(直连用户自配 API,密钥本地加密)</h2>
        <label class="row">
          <span>Provider</span>
          <select v-model="s!.provider" class="ctrl">
            <option value="openai">OpenAI(兼容)</option>
            <option value="google">Google Gemini</option>
            <option value="anthropic">Anthropic Claude</option>
          </select>
        </label>
        <label class="row">
          <span>模型名</span>
          <input v-model="s!.model" class="ctrl" placeholder="如 gpt-4o-mini / gemini-2.0-flash / claude-3-5-haiku" />
        </label>
        <label class="row">
          <span>自定义 BaseURL</span>
          <input v-model="s!.baseUrl" class="ctrl" placeholder="可选:代理/中转地址(空 = 官方)" />
        </label>
        <label class="row">
          <span>密钥({{ s!.provider }})</span>
          <input
            v-model="keys[s!.provider]"
            type="password"
            class="ctrl"
            :placeholder="keyStatus.includes(s!.provider) ? '已保存(留空=不改)' : '粘贴 API Key'"
          />
        </label>
        <p class="hint" v-if="keyStatus.length">
          已配置:{{ keyStatus.join("、") }}
        </p>
      </section>

      <section class="group">
        <h2 class="group-title">成本与行为</h2>
        <label class="row">
          <span>每日 LLM 调用上限</span>
          <input
            v-model.number="s!.dailyBudget"
            type="number"
            min="1"
            step="1"
            class="ctrl narrow"
          />
          <span class="hint">已用 {{ budget?.used ?? 0 }}/{{ budget?.limit ?? DEFAULT_DAILY_BUDGET }} 次</span>
        </label>
        <label class="row switch-row">
          <span>暂停提问</span>
          <input v-model="s!.paused" type="checkbox" class="switch" />
        </label>
        <label class="row switch-row">
          <span>开机自启</span>
          <input v-model="s!.autostart" type="checkbox" class="switch" />
        </label>
        <label class="row switch-row">
          <span>P0 可替换队列最旧卡</span>
          <input v-model="s!.p0ReplaceQueue" type="checkbox" class="switch" />
          <span class="hint">(默认关)</span>
        </label>
      </section>

      <section class="group">
        <h2 class="group-title">运行状态</h2>
        <div class="row static"><span>本地端点</span><code>{{ endpoint }}</code></div>
        <div class="row static">
          <span>事件链路</span>
          <span v-if="!stats?.eventsIngested" class="warn">
            本次运行 0 条 —— 适配器未接入。检查适配器配置是否生效,并新开一个 Agent 会话
          </span>
          <span v-else-if="eventStale" class="warn">
            本次运行 {{ stats.eventsIngested }} 条,最近 {{ fmtAgo(stats.lastEventIn!) }}
            —— 链路曾通后已静默,检查 Agent 是否还在动作
          </span>
          <span v-else>
            本次运行 {{ stats.eventsIngested }} 条 · 最近 {{ fmtAgo(stats.lastEventIn!) }}(链路正常)
          </span>
        </div>
        <div class="row static"><span>活动项目</span><code class="proj">{{ stats?.activeProject || "—" }}</code></div>
        <div class="row static">
          <span>胶囊态</span>
          <span>{{ capsuleStateLabel[stats?.capsuleState ?? "idle"] }}</span>
          <span class="hint">卡片已入队却看不见时,先看这里</span>
        </div>
        <div class="row static"><span>今日</span><span>发卡 {{ stats?.cardsCreated ?? 0 }} · 回答 {{ stats?.answers ?? 0 }} · 不满意 {{ stats?.unsatisfied ?? 0 }}<template v-if="(stats?.cardsCreated ?? 0) > 0"> · 回答率 {{ Math.round(stats!.todayAnswerRate) }}%</template></span></div>
        <!-- 出稿来源:发卡数相同但含义完全不同。模板兜底不为 0 就说明 LLM 链路是坏的,
             而上面那行「发卡 N」照样往上跳 —— 这个数存在的唯一理由就是让那张灯能证伪 -->
        <div class="row static"><span>出稿来源</span><span>本地证据 {{ stats?.cardsEvidence ?? 0 }} · LLM {{ stats?.cardsLlm ?? 0 }} · 模板兜底 {{ stats?.cardsTemplate ?? 0 }}<template v-if="(stats?.cardsTemplate ?? 0) > 0"> · LLM 链路异常,详见日志与决策日志</template></span></div>
        <div class="row static"><span>累计</span><span>发卡 {{ stats?.total.cardsCreated ?? 0 }} · 回答 {{ stats?.total.answers ?? 0 }} · 不满意 {{ stats?.total.unsatisfied ?? 0 }}<template v-if="(stats?.total.cardsCreated ?? 0) > 0"> · 回答率 {{ Math.round(stats!.total.answerRate) }}%</template><template v-if="(stats?.batched ?? 0) > 0"> · 攒批待发 {{ stats!.batched }}</template></span></div>
        <div class="row static"><span>框架事实</span><span>{{ facts?.framework || "未生成(Agent 有动作后自动生成)" }}</span></div>
      </section>
      </div>
      </div>
      <div v-if="s" class="save-row">
        <button class="kvba-btn primary" @click="save">保存</button>
        <button class="kvba-btn outlined" :disabled="testing" @click="testConnection">测试连接</button>
        <Transition name="fade"><span v-if="savedMsg" class="saved">{{ savedMsg }}</span></Transition>
      </div>
    </section>

    <!-- 历史 -->
    <section v-else-if="tab === 'history'" class="scroller history">
      <div v-if="!history.length" class="empty">还没有历史记录。回答过的卡片会出现在这里。</div>
      <div v-for="c in history" :key="c.id" class="h-item">
        <div class="h-top">
          <span class="kvba-chip bb-bauhaus">{{ cardKindPresentation(c).label }}</span>
          <span class="h-time">{{ fmtTime(c.createdAt) }}</span>
        </div>
        <p class="h-text">{{ cardDisplayText(c) }}</p>
        <p v-if="c.insight" class="h-answer">建议：{{ c.insight.suggestedAction }}</p>
        <p class="h-answer">你的答复:{{ c.answer || "—" }}</p>
        <template v-if="c.optimizedPrompt && c.optimizedPrompt !== PROMPT_PENDING_MARK">
          <div class="h-prompts">
            <code class="h-prompt">{{ c.optimizedPrompt }}</code>
            <button class="kvba-btn text copy-btn" @click="copyText(c.optimizedPrompt)">复制</button>
          </div>
        </template>
      </div>
    </section>

    <!-- 决策日志 -->
    <section v-else class="scroller log">
      <div v-if="!decisions.length" class="empty">暂无门控评估记录。</div>
      <div v-for="(d, i) in decisions" :key="i" class="d-item" :class="{ pass: d.pass }">
        <div class="d-top">
          <span class="d-level">{{ d.level }}</span>
          <span class="d-verdict">{{ d.pass ? "通过" : "拒绝" }}</span>
          <span v-if="d.score !== null" class="d-score">{{ d.score }} 分</span>
          <span class="d-time">{{ fmtTime(d.ts) }}</span>
        </div>
        <p class="d-reason">{{ d.reason }}</p>
        <p v-if="d.branch" class="d-branch">分支:{{ d.branch }} · 耗时 {{ d.elapsedMs }}ms</p>
      </div>
    </section>
  </div>
</template>

<style scoped>
.settings {
  height: 100vh;
  display: flex;
  flex-direction: column;
  background: var(--md-surface);
}
.tabs {
  display: flex;
  gap: 4px;
  padding: 10px 14px 0;
  border-bottom: 1px solid var(--md-outline-variant);
}
.tab {
  padding: 8px 16px;
  font-size: var(--fs-md);
  font-weight: 500;
  color: var(--md-on-surface-variant);
  border-bottom: 2px solid transparent;
  transition: color var(--dur-2) var(--ease-out), border-color var(--dur-2) var(--ease-out);
}
.tab:hover {
  color: var(--md-on-surface);
}
.tab.on {
  color: var(--md-primary);
  font-weight: 600;
  border-bottom-color: var(--md-primary);
}

.pane {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.scroller {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.form-body {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.group {
  border: 1px solid var(--md-outline-variant);
  border-radius: var(--r-l);
  padding: 12px 16px 14px;
  background: var(--md-surface);
}
.group-title {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
  font-weight: 600;
  letter-spacing: 0.02em;
}
.row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 10px;
  font-size: var(--fs-md);
}
.row > span:first-child {
  flex: 0 0 136px;
  color: var(--md-on-surface-variant);
}
.ctrl {
  flex: 1;
  min-width: 0;
  height: 36px;
  padding: 0 10px;
  border: 1px solid var(--md-outline);
  border-radius: var(--r-xs);
  background: var(--md-surface);
  color: var(--md-on-surface);
  font-family: var(--font);
  font-size: var(--fs-md);
  transition: border-color var(--dur-2) ease, box-shadow var(--dur-2) ease;
}
.ctrl:hover {
  border-color: var(--md-on-surface-variant);
}
.ctrl:focus-visible {
  outline: none;
  border-color: var(--md-primary);
  box-shadow: 0 0 0 1px var(--md-primary);
}
.ctrl.narrow {
  flex: 0 0 80px;
}
.switch-row input.switch {
  appearance: none;
  flex: 0 0 auto;
  width: 40px;
  height: 22px;
  border-radius: var(--r-full);
  background: var(--md-outline-variant);
  border: 1px solid var(--md-outline);
  position: relative;
  cursor: pointer;
  transition: background-color var(--dur-2) ease, border-color var(--dur-2) ease;
}
.switch-row input.switch::after {
  content: "";
  position: absolute;
  top: 2px;
  left: 2px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: var(--md-surface);
  box-shadow: var(--shadow-1);
  transition: transform var(--dur-2) var(--ease-out);
}
.switch-row input.switch:checked {
  background: var(--md-primary);
  border-color: var(--md-primary);
}
.switch-row input.switch:checked::after {
  transform: translateX(18px);
}
.hint {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
}
.static {
  color: var(--md-on-surface);
}
.warn {
  color: var(--md-error);
}
.proj {
  word-break: break-all;
}
code {
  font-family: "Cascadia Code", Consolas, monospace;
  font-size: 12px;
}

.save-row {
  display: flex;
  align-items: center;
  gap: 12px;
  flex: 0 0 auto;
  padding: 12px 14px;
  background: var(--md-surface);
  border-top: 1px solid var(--md-outline-variant);
}
.saved {
  color: var(--md-primary);
  font-weight: 500;
}
.fade-enter-active,
.fade-leave-active {
  transition: opacity var(--dur-2) ease;
}
.fade-enter-from,
.fade-leave-to {
  opacity: 0;
}

/* 历史 */
.empty {
  padding: 32px 12px;
  text-align: center;
  color: var(--md-on-surface-variant);
  font-size: var(--fs-sm);
}
.h-item {
  border: 1px solid var(--md-outline-variant);
  border-radius: var(--r-m);
  padding: 10px 12px;
}
.h-top {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
.h-time {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
}
.h-text {
  margin-top: 6px;
  font-size: var(--fs-md);
}
.h-answer {
  margin-top: 4px;
  font-size: var(--fs-sm);
  color: var(--md-secondary);
}
.h-prompts {
  display: flex;
  align-items: stretch;
  gap: 8px;
  margin-top: 8px;
}
.h-prompt {
  flex: 1;
  padding: 6px 8px;
  background: var(--md-surface-variant);
  border-radius: var(--r-xs);
  font-size: 11px;
  white-space: pre-wrap;
  word-break: break-all;
}
.copy-btn {
  flex: 0 0 auto;
  height: auto;
  font-size: var(--fs-sm);
}

/* 决策日志 */
.d-item {
  border-left: 3px solid var(--md-error);
  padding: 6px 10px;
  background: var(--md-surface-variant);
  border-radius: var(--r-xs);
}
.d-item.pass {
  border-left-color: var(--md-secondary);
}
.d-top {
  display: flex;
  align-items: center;
  gap: 8px;
}
.d-level {
  font-weight: 700;
  font-size: var(--fs-sm);
  color: var(--md-primary);
}
.d-verdict {
  font-size: var(--fs-sm);
  font-weight: 600;
}
.d-score {
  margin-left: auto;
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
}
.d-time {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
}
.d-reason {
  font-size: var(--fs-md);
  margin-top: 4px;
}
.d-branch {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
  margin-top: 2px;
}
</style>
