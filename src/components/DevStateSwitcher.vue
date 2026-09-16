<script setup lang="ts">
import { ref } from "vue";
import { store, refreshSnapshot } from "../stores/app";
import { DEMO_STATES, applyDemo, clearDemo, type DemoKey } from "../stores/demo";

const open = ref(false);
const keys = Object.keys(DEMO_STATES) as DemoKey[];

function pick(k: DemoKey) {
  applyDemo(k);
  open.value = false;
}
function toReal() {
  clearDemo();
  open.value = false;
  void refreshSnapshot();
}
</script>

<template>
  <div class="switcher">
    <button class="toggle" :class="{ on: open }" @click="open = !open" title="开发环境：状态预览">
      状态预览
    </button>
    <ul v-if="open" class="menu" role="menu">
      <li>
        <button class="item live" :class="{ active: !store.demoState }" @click="toReal">
          实时（真实数据）
        </button>
      </li>
      <li v-for="k in keys" :key="k">
        <button class="item" :class="{ active: store.demoState === k }" @click="pick(k)">
          {{ DEMO_STATES[k].label }}
        </button>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.switcher {
  position: relative;
  font-size: 12px;
}
.toggle {
  background: var(--panel);
  border: 1px dashed var(--border);
  color: var(--muted);
  border-radius: 8px;
  padding: 2px 8px;
  font-size: 12px;
  cursor: pointer;
}
.toggle.on {
  border-color: var(--green);
  color: var(--green);
}
.menu {
  position: absolute;
  bottom: 30px;
  right: 0;
  margin: 0;
  padding: 4px;
  list-style: none;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 10px;
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.18);
  min-width: 160px;
  z-index: 60;
}
.item {
  display: block;
  width: 100%;
  text-align: left;
  background: none;
  border: none;
  border-radius: 6px;
  padding: 6px 8px;
  color: inherit;
  cursor: pointer;
  box-shadow: none;
}
.item:hover {
  background: rgba(127, 127, 127, 0.12);
  border-color: transparent;
}
.item.active {
  color: var(--green);
  font-weight: 600;
}
.item.live {
  border-bottom: 1px solid var(--border);
  border-radius: 6px 6px 0 0;
  margin-bottom: 4px;
  padding-bottom: 8px;
}
</style>