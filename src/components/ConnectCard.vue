<script setup lang="ts">
import { computed, ref } from "vue";
import { store, savePeerIp, connectPeer } from "../stores/app";

const notice = ref("");

/** 演示模式或已有活动/建立中连接时禁止重复发起连接（后端也会拒绝） */
const connectDisabled = computed(
  () =>
    !!store.demoState ||
    ["connecting", "connected", "reconnecting", "paused"].includes(
      store.status
    )
);

function connect() {
  void (async () => {
    const err = await connectPeer();
    notice.value = err ?? "";
  })();
}

/** 明确动作保存对方 IP：失焦或回车触发，不按每次按键写盘 */
async function save() {
  const err = await savePeerIp();
  notice.value = err ?? "";
}
</script>

<template>
  <section class="card">
    <div class="label">对方的 ZeroTier IP</div>
    <div class="row">
      <input
        v-model="store.peerInput"
        placeholder="例如 10.147.17.36"
        @blur="save"
        @keyup.enter="save"
      />
      <button :disabled="connectDisabled" @click="connect">连接</button>
    </div>
    <div v-if="notice" class="hint warn">{{ notice }}</div>
  </section>
</template>

<style scoped>
.hint.warn { color: var(--red); }
</style>