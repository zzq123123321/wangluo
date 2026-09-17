<script setup lang="ts">
/**
 * T11-03B 状态呼吸灯：主窗口被最小化后显示的 40×40 小浮窗。
 * 复用 connection-status-changed / settings-changed 事件与 stores/app.ts，
 * 不新增轮询；单击 invoke("restore_from_indicator") 由 Rust 恢复主窗口。
 */
import { computed, onMounted, onUnmounted, ref } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { store, applySnapshot } from "../stores/app";
import {
  STATUS_TEXT,
  type AppSnapshot,
  type ConnectionStatus,
  type ConnectionStatusEvent,
} from "../types/app";

type Tone = "green" | "yellow" | "red" | "gray";

/** 沿用项目既有状态语义：绿=Connected、黄=Connecting/Reconnecting、
 *  红=Error、灰=Offline/Paused 以及无可用 ZeroTier IP 的非错误状态。 */
function toneOf(status: ConnectionStatus): Tone {
  if (status === "error") return "red";
  if (status === "connected") return "green";
  if (status === "connecting" || status === "reconnecting") {
    return store.zerotierIp ? "yellow" : "gray";
  }
  return "gray";
}

const tone = computed<Tone>(() => toneOf(store.status));

const unlisteners = ref<UnlistenFn[]>([]);

function restoreMain() {
  void invoke("restore_from_indicator").catch(() => {});
}

onMounted(async () => {
  try {
    const snap = await invoke<AppSnapshot>("get_app_snapshot");
    applySnapshot(snap);
  } catch {
    // 无 Tauri 运行时：保留默认灰态
  }
  const add = (p: Promise<UnlistenFn>) => p.then((u) => unlisteners.value.push(u)).catch(() => {});
  add(
    listen<ConnectionStatusEvent>("connection-status-changed", (ev) => {
      if (store.demoState) return;
      const d = ev.payload;
      store.status = d.status;
      store.statusText = d.status_text || STATUS_TEXT[d.status];
      store.peerDeviceName = d.peer?.device_name ?? null;
      store.peerIp = d.peer?.ip ?? null;
    })
  );
  add(
    listen<AppSnapshot>("settings-changed", (ev) => {
      if (store.demoState) return;
      applySnapshot(ev.payload);
    })
  );
});

onUnmounted(() => {
  for (const u of unlisteners.value) u();
});
</script>

<template>
  <button class="led" :class="tone" @click="restoreMain" />
</template>

<style scoped>
.led {
  width: 100%;
  height: 100%;
  border: none;
  border-radius: 50%;
  background: transparent;
  cursor: pointer;
  padding: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  outline: none;
}
.led:active {
  background: transparent;
}
.led:focus-visible {
  outline: none;
}
.led::before {
  content: "";
  width: 14px;
  height: 14px;
  border-radius: 50%;
  animation: breathe 0.9s ease-in-out infinite alternate;
}
.led.green::before {
  background: #34c759;
  box-shadow: 0 0 12px 6px rgba(52, 199, 89, 0.55);
}
.led.yellow::before {
  background: #ffcc00;
  box-shadow: 0 0 12px 6px rgba(255, 204, 0, 0.5);
}
.led.red::before {
  background: #ff3b30;
  box-shadow: 0 0 12px 6px rgba(255, 59, 48, 0.55);
}
.led.gray::before {
  background: #8e8e93;
  box-shadow: 0 0 8px 3px rgba(142, 142, 147, 0.35);
}
@keyframes breathe {
  from {
    opacity: 0.45;
    transform: scale(0.8);
  }
  to {
    opacity: 1;
    transform: scale(1.1);
  }
}
</style>