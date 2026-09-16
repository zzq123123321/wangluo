import { reactive } from "vue";
import { invoke } from "@tauri-apps/api/core";
import type { AppSnapshot, SettingsUpdate } from "../types/app";
import { STATUS_TEXT } from "../types/app";

export const store = reactive<{
  loaded: boolean;
  initialized: boolean;
  deviceId: string;
  deviceName: string;
  appVersion: string;
  listenPort: number;
  autostart: boolean;
  lastPeerIp: string | null;
  zerotierIp: string | null;
  zerotierHint: string;
  status: AppSnapshot["status"];
  statusText: string;
  peerDeviceName: string | null;
  peerIp: string | null;
  paused: boolean;
  lastSync: string | null;
  pairingCode: string | null;
  peerInput: string;
  error: string | null;
  demoState: string | null;
  hintWarn: boolean;
}>({
  loaded: false,
  initialized: false,
  deviceId: "",
  deviceName: "",
  appVersion: "",
  listenPort: 45888,
  autostart: false,
  lastPeerIp: null,
  zerotierIp: null,
  zerotierHint: "正在检测 ZeroTier 网络……",
  status: "offline",
  statusText: "未连接",
  peerDeviceName: null,
  peerIp: null,
  paused: false,
  lastSync: null,
  pairingCode: null,
  peerInput: "",
  error: null,
  demoState: null,
  hintWarn: false,
});

/** 与 Rust 侧 is_valid_ipv4_str 同规则 */
export function isValidIpv4(s: string): boolean {
  const parts = s.split(".");
  if (parts.length !== 4) return false;
  return parts.every(
    (p) => p.length > 0 && p.length <= 3 && /^\d{1,3}$/.test(p) && Number(p) <= 255
  );
}

export async function refreshSnapshot() {
  if (store.demoState) return;
  try {
    const s: AppSnapshot = await invoke("get_app_snapshot");
    store.deviceId = s.device_id;
    store.deviceName = s.device_name;
    store.appVersion = s.app_version;
    store.listenPort = s.listen_port;
    store.autostart = s.autostart;
    store.lastPeerIp = s.last_peer_ip;
    // 首次加载后用已保存的对方 IP 回填输入框；之后不打断用户输入
    if (!store.initialized) {
      store.peerInput = s.last_peer_ip ?? "";
      store.initialized = true;
    }
    store.zerotierIp = s.zerotier_ip;
    store.zerotierHint = s.zerotier_hint;
    store.hintWarn = s.hint_warn;
    store.status = s.status;
    store.statusText = s.status_text || STATUS_TEXT[s.status];
    store.peerDeviceName = s.peer?.device_name ?? null;
    store.peerIp = s.peer?.ip ?? null;
    store.paused = s.paused;
    store.lastSync = s.last_sync;
    store.loaded = true;
  } catch (e) {
    store.error = String(e);
    store.statusText = String(e);
  }
}

/** 持久化设置；成功后用返回快照刷新本地非敏感状态 */
export async function updateSettings(settings: SettingsUpdate): Promise<void> {
  const s: AppSnapshot = await invoke("update_settings", { settings });
  store.autostart = s.autostart;
  store.lastPeerIp = s.last_peer_ip;
  store.paused = s.paused;
}

/** 保存对方 IP 输入（明确动作：失焦/回车触发，不按每次按键写盘）。
 * 返回 null 表示成功或无需保存，否则返回错误信息。 */
export async function savePeerIp(): Promise<string | null> {
  if (!store.loaded) return null;
  const ip = store.peerInput.trim();
  if (ip && !isValidIpv4(ip)) return "不是有效的 IPv4 地址";
  try {
    await updateSettings({ lastPeerIp: ip });
    return null;
  } catch (e) {
    return String(e);
  }
}

/** 主动连接对方（阶段 5）。成功提交后状态由 connection-status-changed 事件驱动。
 * 返回 null 表示提交成功，否则返回后端错误文案（如 IP 非法/本机 IP/已有连接）。 */
export async function connectPeer(): Promise<string | null> {
  if (store.demoState) return null;
  const ip = store.peerInput.trim();
  if (!ip) return "请输入对方的 ZeroTier IP";
  try {
    await invoke("connect_peer", { ip });
    return null;
  } catch (e) {
    return String(e);
  }
}

/** 用户主动断开（阶段 5）：发送 disconnect、停止心跳/读写、回到 Offline。
 * 不删除已保存的对方 IP / 配对信息。 */
export async function disconnectPeer(): Promise<string | null> {
  if (store.demoState) return null;
  try {
    await invoke("disconnect_peer");
    return null;
  } catch (e) {
    return String(e);
  }
}