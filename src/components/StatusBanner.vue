<script setup lang="ts">
import { computed } from "vue";
import { store } from "../stores/app";
import { STATUS_COLOR } from "../types/app";

const dotClass = computed(() => `dot ${STATUS_COLOR[store.status]}`);
</script>

<template>
  <header class="banner">
    <div>
      <div class="title">ClipLink<span v-if="store.appVersion" class="version"> v{{ store.appVersion }}</span></div>
      <div class="subtitle">
        两台电脑之间的文字剪贴板同步<span v-if="store.deviceName"> · {{ store.deviceName }}</span>
      </div>
    </div>
    <div class="state">
      <span :class="dotClass"></span>
      <span class="state-text" :data-status="store.status">{{ store.statusText }}</span>
    </div>
  </header>
  <div v-if="store.error && store.error.trim()" class="error" role="alert">⚠ {{ store.error }}</div>
</template>

<style scoped>
.banner {
  display: flex;
  justify-content: space-between;
  align-items: flex-start;
  gap: 8px;
}
.banner > div:first-child {
  min-width: 0;
}
.title {
  font-size: 18px;
  font-weight: 700;
}
.version {
  font-size: 12px;
  color: var(--muted);
  font-weight: 400;
}
.subtitle {
  font-size: 12px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.state {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  font-size: 12px;
  text-align: right;
  max-width: 68%;
  line-height: 1.35;
  margin-right: 6px;
}
.state-text {
  word-break: break-word;
}
.state-text[data-status="connected"] { color: var(--green); }
.state-text[data-status="reconnecting"],
.state-text[data-status="connecting"] { color: var(--yellow); }
.state-text[data-status="error"] { color: var(--red); }
.error {
  margin-top: 10px;
  font-size: 12px;
  line-height: 1.4;
  color: var(--red);
  border: 1px solid var(--red);
  border-radius: 8px;
  padding: 6px 10px;
  word-break: break-word;
}
.dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--gray);
}
.dot.green { background: var(--green); }
.dot.yellow { background: var(--yellow); }
.dot.red { background: var(--red); }
</style>