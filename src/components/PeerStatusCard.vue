<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { store, disconnectPeer, togglePause } from "../stores/app";

const borderTone = computed(() => {
  if (!store.peerDeviceName) return "";
  if (store.status === "connected") return "ok";
  if (store.status === "paused") return "idle";
  if (store.status === "error") return "bad";
  return "busy";
});

/** 仅 connected/paused 才称“已连接”；建立中/出错显示“对方设备” */
const headerLabel = computed(
  () => (store.status === "connected" || store.status === "paused" ? "已连接：" : "对方设备：")
);

const detailText = computed(() => {
  if (store.paused) return "已暂停同步";
  if (store.status === "connected") return "剪贴板同步已开启";
  if (store.status === "error") return store.statusText;
  return "";
});

/** network-latency-changed 事件 payload（与 Rust 侧 NetworkLatencyEvent 对应） */
interface LatencyEvent {
  latency_ms: number;
  generation: number;
}

/** 最近一次有效心跳 RTT（毫秒）。null = 尚未取得第一笔有效测量。 */
const latencyMs = ref<number | null>(null);

let unlistenLatency: UnlistenFn | null = null;

onMounted(async () => {
  try {
    unlistenLatency = await listen<LatencyEvent>("network-latency-changed", (ev) => {
      if (store.demoState) return;
      latencyMs.value = ev.payload.latency_ms;
    });
  } catch {
    // 无 Tauri 事件通道（纯浏览器开发环境）时忽略
  }
});

// 组件销毁时解除事件监听，避免累积监听器
onUnmounted(() => {
  unlistenLatency?.();
  unlistenLatency = null;
});

/** 连接断开、进入重连/错误/离线，或对方 IP 改变时立即清空旧延迟：
 *  新连接在收到第一笔有效 pong 之前不得继续显示上一条连接的旧 RTT。 */
watch(
  () => [store.status, store.peerIp] as const,
  ([status, ip], [, prevIp]) => {
    const active = status === "connected" || status === "paused";
    if (!active || prevIp !== ip) {
      latencyMs.value = null;
    }
  }
);

const pauseDisabled = computed(
  () => store.status !== "connected" && store.status !== "paused"
);

function pause() {
  void (async () => {
    const err = await togglePause();
    if (err) store.error = err;
  })();
}
function onDisconnect() {
  void (async () => {
    const err = await disconnectPeer();
    if (err) store.error = err;
  })();
}
</script>

<template>
  <section class="card peer" :class="borderTone">
    <template v-if="store.peerDeviceName">
      <div class="row top">
        <span class="dot" :class="store.status"></span>
        <span class="name">{{ headerLabel }}{{ store.peerDeviceName }}</span>
      </div>
      <div class="detail">
        {{ store.peerIp }}<template v-if="latencyMs !== null"> · 延迟 {{ latencyMs }} ms</template><template v-if="detailText"> · {{ detailText }}</template>
      </div>
      <div class="row buttons">
        <button :disabled="pauseDisabled" @click="pause">{{ store.paused ? "恢复" : "暂停" }}</button>
        <button @click="onDisconnect">断开</button>
      </div>
    </template>
    <div v-else class="empty">
      {{ store.status === "connecting" ? "正在连接对方……" : "等待输入对方 IP" }}
    </div>
  </section>
</template>

<style scoped>
.peer {
  border-color: var(--gray);
}
.peer.ok {
  border-color: var(--green);
}
.peer.busy {
  border-color: var(--yellow);
}
.peer.bad {
  border-color: var(--red);
}
.top {
  justify-content: flex-start;
  gap: 6px;
}
.name {
  font-weight: 600;
}
.detail {
  font-size: 12px;
  color: var(--muted);
  margin: 4px 0 8px;
}
.empty {
  font-size: 13px;
  color: var(--muted);
  padding: 4px 0;
}
.dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--gray);
}
.dot.connected { background: var(--green); }
.dot.reconnecting,
.dot.connecting { background: var(--yellow); }
.dot.error { background: var(--red); }
</style>