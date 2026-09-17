import { createApp } from "vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App.vue";
import IndicatorApp from "./indicator/IndicatorApp.vue";
import "./style.css";

let isIndicator = false;
try {
  isIndicator = getCurrentWindow().label === "indicator";
} catch {
  // 普通浏览器（dev/预览）无 Tauri 运行时：渲染主界面
}

if (isIndicator) {
  document.documentElement.style.background = "transparent";
  document.body.style.background = "transparent";
  createApp(IndicatorApp).mount("#app");
} else {
  createApp(App).mount("#app");
}