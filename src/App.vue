<script setup lang="ts">
// 按窗口 label 分发视图:胶囊 / 面板 / 设置
import { onMounted, ref } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import CapsuleDot from "./components/CapsuleDot.vue";
import CapsulePanel from "./components/CapsulePanel.vue";
import SettingsView from "./components/SettingsView.vue";

const label = ref("capsule-dot");
onMounted(async () => {
  try {
    label.value = getCurrentWindow().label;
  } catch {
    label.value = "capsule-dot";
  }
  document.body.dataset.label = label.value;
  document.body.classList.toggle("label-transparent", label.value !== "settings");
});
</script>

<template>
  <CapsuleDot v-if="label === 'capsule-dot'" />
  <CapsulePanel v-else-if="label === 'capsule-panel'" />
  <SettingsView v-else-if="label === 'settings'" />
</template>
