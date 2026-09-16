export type ConnectionStatus =
  | "offline"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "paused"
  | "error";

export interface PeerInfo {
  device_name: string;
  ip: string;
}

export interface AppSnapshot {
  device_id: string;
  device_name: string;
  app_version: string;
  listen_port: number;
  autostart: boolean;
  last_peer_ip: string | null;
  zerotier_ip: string | null;
  zerotier_hint: string;
  hint_warn: boolean;
  status: ConnectionStatus;
  status_text: string;
  peer: PeerInfo | null;
  paused: boolean;
  last_sync: string | null;
}

/** update_settings 命令入参：未传字段表示不修改；lastPeerIp 空字符串表示清除 */
export interface SettingsUpdate {
  autostart?: boolean;
  syncPaused?: boolean;
  lastPeerIp?: string;
}

/** zerotier-ip-changed 事件 payload（与 Rust 侧 ZtIpEvent 对应） */
export interface ZtIpChangedEvent {
  ip: string | null;
}

/** connection-status-changed 事件 payload（与 Rust 侧 ConnectionStatusEvent 对应） */
export interface ConnectionStatusEvent {
  status: ConnectionStatus;
  status_text: string;
  error_code: string | null;
  peer: PeerInfo | null;
  generation: number;
}

/** refresh_zerotier_ip 命令返回值（与 Rust 侧 ZtResult 对应） */
export type ZtRefreshResult = "found" | "no_adapter" | "no_valid_ipv4";

export const STATUS_TEXT: Record<ConnectionStatus, string> = {
  offline: "未连接",
  connecting: "正在连接对方……",
  connected: "已连接，剪贴板同步已开启",
  reconnecting: "连接已中断，正在重试……",
  paused: "已暂停同步",
  error: "发生错误",
};

export const STATUS_COLOR: Record<ConnectionStatus, "green" | "yellow" | "red" | "gray"> = {
  connected: "green",
  connecting: "yellow",
  reconnecting: "yellow",
  error: "red",
  offline: "gray",
  paused: "gray",
};

/** app-error 事件 payload（后端通用错误提示） */
export interface AppErrorEvent {
  message: string;
}