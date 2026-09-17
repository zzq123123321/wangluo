<script setup lang="ts">
import { onMounted, ref, type Component } from "vue";
import { listen } from "@tauri-apps/api/event";
import StatusBanner from "./components/StatusBanner.vue";
import LocalAddressCard from "./components/LocalAddressCard.vue";
import ConnectCard from "./components/ConnectCard.vue";
import PeerStatusCard from "./components/PeerStatusCard.vue";
import { store, refreshSnapshot, applySnapshot, updateSettings } from "./stores/app";
import {
  STATUS_TEXT,
  type ConnectionStatusEvent,
  type ZtIpChangedEvent,
  type AppErrorEvent,
  type ClipboardSyncedEvent,
  type AppSnapshot,
} from "./types/app";
import type { DemoKey } from "./stores/demo";

const dev = import.meta.env.DEV;

const DevSwitcher = ref<Component | null>(null);

/** 格式化最近同步时间（epoch ms 字符串 → 本地 HH:MM:SS） */
function formatSync(ms: string | null): string {
  if (!ms) return "暂无";
  if (!/^\d+$/.test(ms)) return "暂无";
  try {
    return new Date(Number(ms)).toLocaleTimeString();
  } catch {
    return "暂无";
  }
}

async function onAutostartChange(e: Event) {
  if (!store.loaded) return;
  const checked = (e.target as HTMLInputElement).checked;
  try {
    await updateSettings({ autostart: checked });
  } catch (err) {
    store.error = String(err);
  }
}

onMounted(async () => {
  if (dev) {
    const params = new URL(location.href).searchParams;
    if (params.get("dark") === "1") document.documentElement.classList.add("dark");
    else if (params.get("light") === "1") document.documentElement.classList.add("light");
    const { DEMO_STATES, applyDemo } = await import("./stores/demo");
    const key = params.get("demo");
    if (key && key in DEMO_STATES) applyDemo(key as DemoKey);
    // 开发预览：注入一条 app-error 提示（与 demo 叠加验证错误展示位）
    const errTest = params.get("errtest");
    if (errTest) store.error = errTest;
    DevSwitcher.value = (await import("./components/DevStateSwitcher.vue")).default;
  }
  // 后端 ZeroTier IP 变化事件：拉取最新快照刷新界面（演示模式下 refreshSnapshot 自动跳过）
  try {
    await listen<ZtIpChangedEvent>("zerotier-ip-changed", () => {
      void refreshSnapshot();
    });
    // 阶段 5：连接状态事件（Rust 侧已按连接代次过滤，前端直接应用）
    await listen<ConnectionStatusEvent>("connection-status-changed", (ev) => {
      if (store.demoState) return;
      const d = ev.payload;
      store.status = d.status;
      store.statusText = d.status_text || STATUS_TEXT[d.status];
      store.peerDeviceName = d.peer?.device_name ?? null;
      store.peerIp = d.peer?.ip ?? null;
    });
    // 阶段 6：通用错误提示事件（如剪贴板文字超限）
    await listen<AppErrorEvent>("app-error", (ev) => {
      if (store.demoState) return;
      store.error = ev.payload.message;
    });
    // 阶段 7：远程剪贴板落地后更新“最近同步”时间
    await listen<ClipboardSyncedEvent>("clipboard-synced", (ev) => {
      if (store.demoState) return;
      store.lastSync = ev.payload.time;
    });
    // 阶段 9：托盘内修改设置（暂停/恢复、开机启动）后同步前端
    await listen<AppSnapshot>("settings-changed", (ev) => {
      if (store.demoState) return;
      applySnapshot(ev.payload);
    });
  } catch {
    // 普通浏览器无 Tauri 运行时：无事件通道，开发页靠 demo 状态预览
  }
  refreshSnapshot();
});
</script>

<template>
  <main class="layout">
    <StatusBanner />
    <LocalAddressCard />
    <ConnectCard />
    <PeerStatusCard />
    <div class="foot">
      <div class="last-sync">
        最近同步：{{ formatSync(store.lastSync) }}
      </div>
      <div class="foot-row">
        <label class="check">
          <input
            type="checkbox"
            :checked="store.autostart"
            :disabled="!!store.demoState || !store.loaded"
            title="写入注册表启动项（HKCU Run 键，开机静默到托盘）"
            @change="onAutostartChange"
          />
          <span>开机启动</span>
        </label>
        <component :is="DevSwitcher" v-if="dev && DevSwitcher" />
      </div>
    </div>
  </main>
</template>

<style scoped>
.foot {
  margin-top: auto;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.last-sync {
  font-size: 12px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.foot-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
}
</style>