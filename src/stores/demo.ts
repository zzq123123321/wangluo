import { store } from "./app";
import type { AppSnapshot } from "../types/app";

type DemoKey =
  | "detecting"
  | "no_zerotier"
  | "waiting_input"
  | "connecting"
  | "connected"
  | "paused"
  | "reconnecting"
  | "error";

interface DemoState {
  label: string;
  status: AppSnapshot["status"];
  statusText: string;
  zerotierIp: string | null;
  hint: string;
  hintWarn?: boolean;
  loaded?: boolean;
  peerDeviceName?: string;
  peerIp?: string;
  paused?: boolean;
  lastSync?: string | null;
}

// 状态文字与 docs/开发文档.md 第 3 节"必须提供的状态文字"逐字一致
export const DEMO_STATES: Record<DemoKey, DemoState> = {
  detecting: {
    label: "检测中",
    status: "offline",
    statusText: "正在检测 ZeroTier 网络……",
    zerotierIp: null,
    hint: "正在检测 ZeroTier 网络……",
    loaded: false,
    hintWarn: false,
  },
  no_zerotier: {
    label: "未发现 ZeroTier",
    status: "offline",
    statusText: "未发现 ZeroTier IP。",
    zerotierIp: null,
    hint: "未发现 ZeroTier IP，请确认 ZeroTier 已启动，并且本机已经加入并获准访问网络。",
    loaded: true,
    hintWarn: true,
  },
  waiting_input: {
    label: "等待输入对方 IP",
    status: "offline",
    statusText: "等待输入对方 IP。",
    zerotierIp: "192.168.191.180",
    hint: "ZeroTier 已连接",
  },
  connecting: {
    label: "连接中",
    status: "connecting",
    statusText: "正在连接对方……",
    zerotierIp: "192.168.191.180",
    hint: "ZeroTier 已连接",
    peerDeviceName: "DESKTOP-B",
    peerIp: "10.147.17.36",
  },
  connected: {
    label: "已连接",
    status: "connected",
    statusText: "已连接，剪贴板同步已开启。",
    zerotierIp: "192.168.191.180",
    hint: "ZeroTier 已连接",
    peerDeviceName: "DESKTOP-B",
    peerIp: "10.147.17.36",
    lastSync: "来自 DESKTOP-B · 刚刚",
  },
  paused: {
    label: "已暂停",
    status: "paused",
    statusText: "已暂停同步。",
    zerotierIp: "192.168.191.180",
    hint: "ZeroTier 已连接",
    peerDeviceName: "DESKTOP-B",
    peerIp: "10.147.17.36",
    paused: true,
    lastSync: "来自 DESKTOP-B · 3 分钟前",
  },
  reconnecting: {
    label: "重连中",
    status: "reconnecting",
    statusText: "连接已中断，正在重试……",
    zerotierIp: "192.168.191.180",
    hint: "ZeroTier 已连接",
    peerDeviceName: "DESKTOP-B",
    peerIp: "10.147.17.36",
  },
  error: {
    label: "连接错误",
    status: "error",
    statusText: "无法连接，请确认对方程序已经启动。",
    zerotierIp: "192.168.191.180",
    hint: "ZeroTier 已连接",
  },
};

export type { DemoKey };

export function applyDemo(key: DemoKey) {
  const d = DEMO_STATES[key];
  store.demoState = key;
  store.statusText = d.statusText;
  store.status = d.status;
  store.zerotierIp = d.zerotierIp;
  store.zerotierHint = d.hint;
  store.hintWarn = d.hintWarn ?? false;
  store.loaded = d.loaded ?? true;
  store.peerDeviceName = d.peerDeviceName ?? null;
  store.peerIp = d.peerIp ?? null;
  store.paused = d.paused ?? false;
  store.lastSync = d.lastSync ?? null;
  store.error = null;
}

export function clearDemo() {
  store.demoState = null;
}