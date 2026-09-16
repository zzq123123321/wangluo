<script setup lang="ts">
import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { store, refreshSnapshot } from "../stores/app";
import type { ZtRefreshResult } from "../types/app";

const refreshing = ref(false);

async function copyIp() {
  if (!store.zerotierIp) return;
  try {
    await navigator.clipboard.writeText(store.zerotierIp);
  } catch {
    // 剪贴板 API 不可用时静默，主流程不依赖
  }
}

async function onRefresh() {
  if (store.demoState || refreshing.value) return;
  refreshing.value = true;
  store.zerotierHint = "正在检测 ZeroTier 网络……";
  store.hintWarn = false;
  try {
    await invoke<ZtRefreshResult>("refresh_zerotier_ip");
    await refreshSnapshot();
  } catch (e) {
    store.error = String(e);
    store.zerotierHint = `ZeroTier 检测失败：${String(e)}`;
    store.hintWarn = true;
  } finally {
    refreshing.value = false;
  }
}
</script>

<template>
  <section class="card">
    <div class="label">我的 ZeroTier IP</div>
    <div class="row">
      <input :value="store.zerotierIp ?? ''" placeholder="检测中……" readonly />
      <button :disabled="!store.zerotierIp" @click="copyIp">复制</button>
      <button :disabled="!!store.demoState || refreshing" @click="onRefresh">
        {{ refreshing ? "检测中" : "刷新" }}
      </button>
    </div>
    <div class="hint" :class="{ warn: store.hintWarn }">{{ store.zerotierHint }}</div>
  </section>
</template>

<style scoped>
.hint.warn { color: var(--red); }
</style>