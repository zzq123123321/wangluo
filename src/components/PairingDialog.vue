<script setup lang="ts">
import { store, disconnectPeer } from "../stores/app";

function approve() {
  // 阶段 6 实现 approve_pairing
}
function reject() {
  // 阶段 6 实现 reject_pairing
}
function onDisconnect() {
  void (async () => {
    const err = await disconnectPeer();
    if (err) store.error = err;
  })();
}
</script>

<template>
  <div v-if="store.status === 'awaiting_pairing'" class="overlay">
    <div class="dialog" role="dialog" aria-modal="true" aria-label="配对确认">
      <div class="title">等待配对确认</div>
      <p class="desc">请核对双方显示的确认码是否一致，一致才可安全同步。</p>
      <div class="peer">
        <span class="peer-name">{{ store.peerDeviceName ?? "对方设备" }}</span>
        <span class="peer-ip">{{ store.peerIp ?? "" }}</span>
      </div>
      <div class="code" aria-label="六位确认码">{{ store.pairingCode ?? "--------" }}</div>
      <div class="row">
        <button class="primary" disabled title="阶段 6 实现" @click="approve">允许并记住</button>
        <button disabled title="阶段 6 实现" @click="reject">拒绝</button>
        <button @click="onDisconnect">断开</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 40;
}
.dialog {
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 20px;
  width: 340px;
  text-align: center;
}
.title {
  font-weight: 700;
  font-size: 16px;
}
.desc {
  font-size: 12px;
  color: var(--muted);
  margin: 6px 0 12px;
}
.peer {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin-bottom: 8px;
}
.peer-name {
  font-weight: 600;
  font-size: 14px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.peer-ip {
  font-size: 12px;
  color: var(--muted);
}
.code {
  font-size: 34px;
  font-weight: 700;
  letter-spacing: 10px;
  padding-left: 10px;
  margin: 14px 0 18px;
  font-family: ui-monospace, Consolas, monospace;
}
.row {
  display: flex;
  gap: 8px;
  justify-content: center;
}
.primary {
  border-color: var(--green);
  color: var(--green);
  font-weight: 600;
}
</style>