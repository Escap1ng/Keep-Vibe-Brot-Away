<script setup lang="ts">
// 弹出面板:active(卡片横幅)/ popup(队列列表 ≤3 张)
// 消费驱动:满意/不满意/自定义…/跳过/不再问这类;不满意 → 下一轮优化提示词(剪贴板 + .kvba/next-prompt.md)
import { computed, onMounted, onUnmounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { api } from "../lib/api";
import { MAX_PENDING, PROMPT_PENDING_MARK } from "../lib/types";
import type { Card } from "../lib/types";

const win = getCurrentWindow();
const cards = ref<Card[]>([]);
const current = ref(0);
const mode = ref<"active" | "popup">("popup");
const customOpen = ref(false);
const customText = ref("");
const working = ref(false);
const toast = ref("");
let toastTimer: ReturnType<typeof setTimeout> | null = null;
let hideTimer: ReturnType<typeof setTimeout> | null = null;
let unlisteners: Array<() => void> = [];

const currentCard = computed(() => cards.value[current.value] ?? null);
// 必须是 computed:setup 期的一次性快照永远不会随队列刷新
const shownCards = computed(() => cards.value.slice(0, MAX_PENDING));

function showToast(msg: string) {
  toast.value = msg;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toast.value = ""), 2800);
}

async function refresh() {
  cards.value = await api.queueListen().catch(() => []);
  if (current.value >= cards.value.length) current.value = Math.max(0, cards.value.length - 1);
}

function go(delta: number) {
  const n = cards.value.length;
  if (n < 2) return;
  current.value = (current.value + delta + n) % n;
  customOpen.value = false;
}

async function answer(card: Card, text: string) {
  working.value = true;
  try {
    const updated = await api.queueAnswer(card.id, text);
    if (updated.optimizedPrompt === PROMPT_PENDING_MARK) {
      showToast("正在生成「下一轮优化提示词」:完成后写入剪贴板并归档到 .kvba/next-prompt.md");
    }
  } catch (e) {
    showToast(String(e));
  } finally {
    working.value = false;
    customOpen.value = false;
    customText.value = "";
    await refresh();
  }
}

async function dismiss(card: Card) {
  await api.queueDismiss(card.id).catch(() => void 0);
  await refresh();
}

async function askOne() {
  working.value = true;
  try {
    const msg = await api.gateTick(true);
    showToast(msg);
  } catch (e) {
    showToast(String(e));
  } finally {
    working.value = false;
    await refresh();
  }
}

async function mute(card: Card) {
  await api.queueMuteTheme(card.id).catch(() => void 0);
  await refresh();
}

async function submitCustom(card: Card) {
  const t = customText.value.trim();
  if (!t) return;
  await answer(card, `自定义: ${t}`);
}

async function closePanel() {
  await api.panelClose().catch(() => void 0);
}

function onHeaderPointerDown(e: PointerEvent) {
  // 按住标题栏空白处 → native 拖动窗口(排除按钮/退出是它们自己处理)
  const t = e.target as HTMLElement;
  if (t.closest("button") || t.closest("input") || t.closest("textarea")) return;
  if ((e as PointerEvent).button === 0) {
    win.startDragging().catch(() => void 0);
  }
}

function onKeyEsc(e: KeyboardEvent) {
  if (e.key === "Escape") closePanel();
}

function cancelAutoHide() {
  if (hideTimer) {
    clearTimeout(hideTimer);
    hideTimer = null;
  }
}

async function armAutoHide() {
  if (hideTimer) clearTimeout(hideTimer);
  hideTimer = setTimeout(async () => {
    // 正在输入自定义答复时不收起,避免打断作答
    if (document.activeElement instanceof HTMLTextAreaElement) return;
    if (!document.hasFocus()) {
      await api.panelClose().catch(() => void 0);
    }
    hideTimer = null;
  }, 400);
}

onMounted(async () => {
  await refresh();

  unlisteners.push(
    await listen<{ mode: string }>("panel:show", async (e) => {
      await refresh();
      mode.value = (e.payload?.mode as "active" | "popup") || "popup";
      // active(新卡到达)从队首看起;popup 保留用户当前浏览位置
      if (mode.value === "active") current.value = 0;
    }),
    await listen<Card>("card.new", refresh),
    await listen("card.dismissed", refresh),
    await listen("card.updated", refresh),
  );

  // 失焦自动收起(仅在面板曾获得焦点时可靠;未交互时由 ✕/ESC 兜底)
  unlisteners.push(
    await win.onFocusChanged(({ payload }) => {
      if (!payload) armAutoHide();
    }),
  );
  window.addEventListener("pointerdown", cancelAutoHide, true);
  window.addEventListener("keydown", onKeyEsc, true);
});

onUnmounted(() => {
  unlisteners.forEach((u) => u());
  window.removeEventListener("pointerdown", cancelAutoHide, true);
  window.removeEventListener("keydown", onKeyEsc, true);
  if (hideTimer) clearTimeout(hideTimer);
  if (toastTimer) clearTimeout(toastTimer);
});

const sevLabel: Record<string, string> = { info: "信息", P1: "重要", P0: "紧急" };
const kindLabel: Record<string, string> = { review: "审查", tips: "知识", awareness: "知情度" };
</script>

<template>
  <div class="panel">
    <!-- 头部:队列信息 + 来一道 + 关闭;按住空白区域可拖动窗口 -->
    <header
      class="panel-head dragzone"
      @pointerdown="onHeaderPointerDown"
    >
      <div class="head-left">
        <span class="logo-mark"></span>
        <span class="queue-info">队列 {{ cards.length }}/{{ MAX_PENDING }}</span>
      </div>
      <div class="head-actions">
        <button class="kvba-btn text pressable head-tip" @click="askOne" :disabled="working">
          来一道
        </button>
        <button class="kvba-btn text pressable head-close" title="关闭" @click="closePanel">✕</button>
      </div>
    </header>

    <!-- active:当前卡片横幅 -->
    <section v-if="mode === 'active' && currentCard" class="card-view" :key="currentCard.id + '-act'">
      <div class="card-topline">
        <span class="kvba-chip bb-bauhaus">{{ kindLabel[currentCard.type] }}</span>
        <span class="kvba-chip" :class="currentCard.severity === 'P0' ? 'sev-p0' : 'sev-info'">
          {{ sevLabel[currentCard.severity] }}
        </span>
        <span class="theme-tag">#{{ currentCard.theme }}</span>
      </div>
      <p class="card-text">{{ currentCard.text }}</p>
      <div v-if="currentCard.refFiles.length" class="ref-list">
        <span v-for="f in currentCard.refFiles.slice(0, 3)" :key="f" class="ref-file">{{ f }}</span>
      </div>
      <div class="card-note" v-if="currentCard.status === 'stale'">该卡引用的文件被再次修改,已标记为过时。</div>

      <!-- 操作区 -->
      <div class="actions">
        <template v-if="!customOpen">
          <button class="kvba-btn primary" :disabled="working" @click="answer(currentCard, '满意')">满意</button>
          <button class="kvba-btn tonal" :disabled="working" @click="answer(currentCard, '不满意')">不满意</button>
          <button class="kvba-btn outlined" :disabled="working" @click="customOpen = true">自定义…</button>
        </template>
        <template v-else>
          <textarea
            v-model="customText"
            class="custom-input"
            rows="2"
            maxlength="120"
            placeholder="写下你的判断或补充…"
            @keydown.ctrl.enter="submitCustom(currentCard)"
          ></textarea>
          <div class="custom-actions">
            <button class="kvba-btn text" @click="customOpen = false">取消</button>
            <button class="kvba-btn primary" :disabled="!customText.trim()" @click="submitCustom(currentCard)">
              提交
            </button>
          </div>
        </template>
      </div>
      <footer class="card-foot">
        <button class="kvba-btn text foot-btn" @click="dismiss(currentCard)">跳过</button>
        <button class="kvba-btn text foot-btn" @click="mute(currentCard)">不再问这类</button>
        <div class="switch">
          <button v-if="cards.length > 1" class="chev" @click="go(-1)">‹</button>
          <span class="count">{{ current + 1 }}/{{ cards.length }}</span>
          <button v-if="cards.length > 1" class="chev" @click="go(1)">›</button>
        </div>
      </footer>
    </section>

    <!-- popup:队列列表 -->
    <section v-else class="list-view">
      <div v-if="!shownCards.length" class="empty">
        <span class="empty-mark"></span>
        <p>队列是空的。Agent 活跃时,KVBA 会在等待间隙递上小知识和审查问题。</p>
      </div>
      <button
        v-for="(c, i) in shownCards"
        :key="c.id + '-item'"
        class="queue-item pressable"
        @click="current = i; mode = 'active'"
      >
        <span class="q-kind" :class="c.type">{{ kindLabel[c.type] }}</span>
        <span class="q-sev" :class="c.severity">{{ sevLabel[c.severity] }}</span>
        <span class="q-text">{{ c.text }}</span>
      </button>
    </section>

    <!-- toast -->
    <Transition name="toast">
      <div v-if="toast" class="toast">{{ toast }}</div>
    </Transition>
  </div>
</template>

<style scoped>
.panel {
  width: 344px;
  height: 452px;
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  background: transparent;
}
/* 表面:毛玻璃材质(Apple 材质层级 → 面板更"厚") */
.panel::before {
  content: "";
  position: absolute;
  inset: 0;
  border-radius: var(--r-l);
  background: var(--md-surface);
  box-shadow: var(--shadow-3);
  backdrop-filter: blur(20px) saturate(160%);
  z-index: -1;
}
.panel {
  position: relative;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 2px 4px 0;
}
.head-left {
  display: flex;
  align-items: center;
  gap: 8px;
}
.head-actions {
  display: flex;
  align-items: center;
  gap: 2px;
}
.head-close {
  width: 28px;
  height: 28px;
  padding: 0;
  font-size: 12px;
  color: var(--md-on-surface-variant);
  border-radius: var(--r-full);
}
.dragzone {
  cursor: grab;
}
.dragzone:active {
  cursor: grabbing;
}
.logo-mark {
  width: 12px;
  height: 12px;
  border-radius: 3px;
  background: var(--md-primary);
  position: relative;
}
.logo-mark::after {
  content: "";
  position: absolute;
  right: -3px;
  bottom: -3px;
  width: 6px;
  height: 6px;
  background: var(--md-tertiary);
  border-radius: 50%;
}
.queue-info {
  font-weight: 600;
  letter-spacing: 0.04em;
}

/* 卡片 */
.card-view {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 14px 12px 8px;
  border-radius: var(--r-m);
  background: var(--md-surface-variant);
  transition:
    opacity var(--dur-2) var(--ease-out),
    transform var(--dur-2) var(--ease-out);
}
.card-topline {
  display: flex;
  align-items: center;
  gap: 6px;
}
.sev-p0 {
  background: var(--md-error);
  color: #fff;
  font-weight: 600;
}
.sev-info {
  background: var(--md-outline-variant);
}
.theme-tag {
  margin-left: auto;
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
}
.card-text {
  font-size: var(--fs-lg);
  line-height: 1.55;
}
.ref-list {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
.ref-file {
  font-family: "Cascadia Code", Consolas, monospace;
  font-size: 11px;
  padding: 2px 6px;
  border-radius: var(--r-xs);
  background: var(--md-surface);
  color: var(--md-on-surface-variant);
  border: 1px solid var(--md-outline-variant);
}
.card-note {
  font-size: var(--fs-sm);
  color: var(--md-tertiary);
}

.actions {
  margin-top: auto;
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.custom-input {
  flex: 1;
  min-height: 44px;
  border: 1px solid var(--md-outline);
  border-radius: var(--r-xs);
  padding: 8px;
  font-family: var(--font);
  font-size: var(--fs-sm);
  background: var(--md-surface);
  color: var(--md-on-surface);
  resize: none;
}
.custom-actions {
  display: flex;
  gap: 6px;
  margin-left: auto;
}

.card-foot {
  display: flex;
  align-items: center;
  gap: 4px;
}
.foot-btn {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
  height: 30px;
}
.switch {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 6px;
}
.chev {
  width: 26px;
  height: 26px;
  border-radius: 50%;
  font-size: 16px;
  line-height: 1;
  color: var(--md-primary);
  transition:
    transform var(--dur-1) var(--ease-out),
    background-color var(--dur-2) ease;
}
.chev:hover {
  background: var(--md-primary-container);
}
.chev:active {
  transform: scale(0.92);
}
.count {
  font-size: var(--fs-sm);
  color: var(--md-on-surface-variant);
  min-width: 28px;
  text-align: center;
}

/* 列表 */
.list-view {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 8px;
  overflow-y: auto;
}
.empty {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
  color: var(--md-on-surface-variant);
  padding: 20px;
  text-align: center;
  font-size: var(--fs-sm);
}
.empty-mark {
  width: 28px;
  height: 28px;
  border: 3px solid var(--md-outline-variant);
  border-radius: 50%;
  position: relative;
}
.empty-mark::after {
  content: "";
  position: absolute;
  inset: 4px;
  background: var(--md-secondary-container);
  border-radius: 50%;
  animation: breathe 3.2s var(--ease-in-out) infinite alternate;
}
@keyframes breathe {
  from {
    opacity: 0.4;
  }
  to {
    opacity: 1;
  }
}
.queue-item {
  display: grid;
  grid-template-columns: auto auto 1fr;
  gap: 8px;
  align-items: start;
  padding: 10px 12px;
  border-radius: var(--r-m);
  background: var(--md-surface);
  box-shadow: var(--shadow-1);
  text-align: left;
  transition:
    box-shadow var(--dur-2) ease,
    transform var(--dur-1) var(--ease-out);
}
.queue-item:active {
  transform: scale(0.98);
}
.q-kind {
  font-size: var(--fs-sm);
  font-weight: 600;
  padding: 1px 6px;
  border-radius: var(--r-xs);
}
.q-kind.review {
  background: var(--md-error-container);
  color: var(--md-on-error-container);
}
.q-kind.tips {
  background: var(--md-secondary-container);
  color: var(--on-secondary-container);
}
.q-kind.awareness {
  background: var(--md-tertiary-container);
  color: var(--on-tertiary-container);
}
.q-sev {
  font-size: var(--fs-sm);
}
.q-sev.P0 {
  color: var(--md-error);
  font-weight: 700;
}
.q-text {
  font-size: var(--fs-md);
  line-height: 1.4;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

/* toast */
.toast {
  position: absolute;
  left: 16px;
  right: 16px;
  bottom: 14px;
  padding: 10px 14px;
  border-radius: var(--r-m);
  background: var(--bb-ink);
  color: var(--md-surface);
  font-size: var(--fs-sm);
  line-height: 1.5;
  box-shadow: var(--shadow-3);
}
.toast-enter-active,
.toast-leave-active {
  transition:
    opacity var(--dur-2) var(--ease-out),
    transform var(--dur-2) var(--ease-out);
}
.toast-enter-from,
.toast-leave-to {
  opacity: 0;
  transform: translateY(8px);
}
</style>
