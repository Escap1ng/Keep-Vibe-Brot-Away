<script setup lang="ts">
// 悬浮胶囊 idle 态:40px 圆点 / 拖拽(逻辑→物理像素按 dpr 换算) / 边缘吸附 / 单击展开面板 / 右键菜单
// 说明:窗口 48×48(圆点 40 + 4px 余量),非交互区≈0,无需点击穿透状态机
import { onMounted, onUnmounted, ref } from "vue";
import { getCurrentWindow, PhysicalPosition } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { Menu, MenuItem, PredefinedMenuItem } from "@tauri-apps/api/menu";
import { api } from "../lib/api";

const win = getCurrentWindow();
const pendingCount = ref(0);
const hovered = ref(false);
const dragging = ref(false);

interface DragState {
  pointerId: number;
  mx: number; // 指针起点(逻辑屏幕坐标)
  my: number;
  wx: number; // 窗口起点(物理坐标)
  wy: number;
  lx: number; // 最近一帧指针(逻辑)
  ly: number;
  moved: boolean;
  dpr: number;
}
let drag: DragState | null = null;
let suppressClick = false;
let unlisteners: Array<() => void> = [];

async function refreshBadge() {
  try {
    const cards = await api.queueListen();
    pendingCount.value = cards.length;
  } catch {
    /* 面板未就绪时忽略 */
  }
}

async function onPointerDown(e: PointerEvent) {
  const el = e.currentTarget as HTMLElement;
  if (e.button !== 0 || drag) return;
  const pos = await win.outerPosition();
  drag = {
    pointerId: e.pointerId,
    mx: e.screenX,
    my: e.screenY,
    wx: pos.x,
    wy: pos.y,
    lx: e.screenX,
    ly: e.screenY,
    moved: false,
    dpr: window.devicePixelRatio || 1,
  };
  el.setPointerCapture(e.pointerId);
}

function onPointerMove(e: PointerEvent) {
  if (!drag || e.pointerId !== drag.pointerId) return;
  drag.lx = e.screenX;
  drag.ly = e.screenY;
  const dxL = e.screenX - drag.mx;
  const dyL = e.screenY - drag.my;
  if (!drag.moved && Math.hypot(dxL, dyL) > 4) drag.moved = true;
  if (!drag.moved) return;
  dragging.value = true;
  // 逻辑坐标差 × dpr → 物理位移
  const nx = Math.round(drag.wx + dxL * drag.dpr);
  const ny = Math.round(drag.wy + dyL * drag.dpr);
  win.setPosition(new PhysicalPosition(nx, ny)).catch(() => void 0);
}

async function onPointerUp(e: PointerEvent) {
  if (!drag || e.pointerId !== drag.pointerId) return;
  const d = drag;
  drag = null;
  dragging.value = false;
  if (!d.moved) return;
  // 拖拽结束:抑制随后补发的 click,并吸附落点
  suppressClick = true;
  setTimeout(() => (suppressClick = false), 300);
  const nx = Math.round(d.wx + (d.lx - d.mx) * d.dpr);
  const ny = Math.round(d.wy + (d.ly - d.my) * d.dpr);
  try {
    const [sx, sy] = await api.dockSnap(nx, ny);
    await win.setPosition(new PhysicalPosition(sx, sy));
  } catch {
    /* 吸附失败保持原位 */
  }
}

async function onClick() {
  if (suppressClick) {
    suppressClick = false;
    return;
  }
  await refreshBadge();
  await api.panelOpen("popup");
}

async function onContextMenu(e: MouseEvent) {
  e.preventDefault();
  const menu = await Menu.new({
    items: [
      await MenuItem.new({
        id: "tip",
        text: "来一道",
        action: async () => {
          try {
            await api.gateTick(true);
          } catch {
            // 出错(队列满/无活动) → 打开面板给用户可见反馈
            await api.panelOpen("popup").catch(() => void 0);
          }
        },
      }),
      await MenuItem.new({
        id: "settings",
        text: "打开设置…",
        action: () => api.settingsOpen(),
      }),
      await MenuItem.new({
        id: "hide",
        text: "隐藏胶囊(仅托盘)",
        action: () => api.capsuleHide(),
      }),
      await PredefinedMenuItem.new({ item: "Separator" }),
      await MenuItem.new({
        id: "quit",
        text: "退出",
        action: () => api.quitApp(),
      }),
    ],
  });
  await menu.popup();
}

onMounted(async () => {
  refreshBadge();
  unlisteners.push(
    await listen("card.new", refreshBadge),
    await listen<{ id: string }>("card.dismissed", refreshBadge),
    await listen<{ id: string }>("card.updated", refreshBadge),
    await listen("panel:closed", refreshBadge),
  );
});

onUnmounted(() => {
  unlisteners.forEach((f) => f());
});
</script>

<template>
  <div class="kvba-dot-shell">
    <div
      class="kvba-dot pressable"
      :class="{ hovered, dragging }"
      @pointerdown="onPointerDown"
      @pointermove="onPointerMove"
      @pointerup="onPointerUp"
      @pointercancel="onPointerUp"
      @click="onClick"
      @contextmenu="onContextMenu"
      @pointerenter="hovered = true"
      @pointerleave="hovered = false"
    >
      <!-- 包豪斯三角标(品牌记号) -->
      <svg class="dot-mark" viewBox="0 0 16 12" aria-hidden="true">
        <path d="M1 1h14L8 11z" fill="var(--on-primary)" />
      </svg>
      <span v-if="pendingCount > 0" class="dot-badge">{{ pendingCount > 9 ? "9+" : pendingCount }}</span>
    </div>
  </div>
</template>

<style scoped>
.kvba-dot-shell {
  width: 48px;
  height: 48px;
  display: flex;
  align-items: center;
  justify-content: center;
}
.kvba-dot {
  position: relative;
  width: 40px;
  height: 40px;
  border-radius: var(--r-full);
  background: var(--md-primary);
  box-shadow: var(--shadow-2);
  display: flex;
  align-items: center;
  justify-content: center;
  touch-action: none;
  transition: transform var(--dur-2) var(--ease-out), box-shadow var(--dur-2) ease,
    background-color var(--dur-2) ease;
}
.kvba-dot.hovered {
  transform: scale(1.08);
}
.kvba-dot.dragging {
  transform: scale(1.12);
  box-shadow: var(--shadow-3);
}

/* 包豪斯记号:倒三角 */
.dot-mark {
  width: 16px;
  height: 12px;
  display: block;
  transition: transform var(--dur-3) var(--ease-out);
}
.kvba-dot.hovered .dot-mark {
  transform: translateY(2px);
}

.dot-badge {
  position: absolute;
  top: -6px;
  right: -6px;
  min-width: 18px;
  height: 18px;
  padding: 0 5px;
  border-radius: var(--r-full);
  background: var(--bb-gold);
  border: 2px solid var(--md-surface);
  color: var(--on-tertiary-container);
  font-size: 11px;
  font-weight: 600;
  line-height: 14px;
  text-align: center;
  box-shadow: var(--shadow-1);
}
</style>
