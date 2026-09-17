// 阶段 5：TCP 监听、连接生命周期、读写循环与心跳。
// 架构：NetworkManager 集中管理 listener 任务、当前活动连接、取消信号、writer 通道、
// 心跳状态与连接代次（generation）。所有状态写入/事件经 StatusSink 单一出口，
// 旧连接任务结束时必须仍是当前活动连接（generation 匹配 + settled 标记）才能影响状态。
// 慢速网络 IO 与任务 join 一律在 AppState 锁外；std Mutex 持锁期间无 await。
use crate::error::AppError;
use crate::protocol;
use crate::state::{ConnectionStatus, PeerInfo};
use serde::Serialize;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time;
use tokio_util::sync::CancellationToken;

pub const STATUS_EVENT: &str = "connection-status-changed";
/// 目标机网络延迟更新事件（复用现有 ping/pong 心跳的 RTT，T11-03A）。
pub const LATENCY_EVENT: &str = "network-latency-changed";
/// 最近剪贴板消息 ID 去重队列上限（阶段 7，应用重启后无需保留）。
pub const MAX_RECENT_MESSAGE_IDS: usize = 100;

/// 连接状态事件 payload：只含非敏感信息（无 device_secret / shared_key / auth / 原始消息）。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStatusEvent {
    pub status: ConnectionStatus,
    pub status_text: String,
    pub error_code: Option<String>,
    pub peer: Option<PeerInfo>,
    pub generation: u64,
}

/// RTT 事件 payload：本机记录的 ping 往返毫秒数 + 产生它的连接代次。非敏感。
#[derive(Debug, Clone, Serialize)]
pub struct NetworkLatencyEvent {
    pub latency_ms: u64,
    pub generation: u64,
}

/// 状态事件出口：生产环境为 TauriStatusSink（写 AppState + emit），测试用收集型实现。
pub trait StatusSink: Send + Sync {
    fn on_status(&self, ev: &ConnectionStatusEvent);
    /// RTT 更新（复用现有心跳的往返时间；默认 no-op，其余实现无需为此改动）。
    fn on_latency(&self, _ev: &NetworkLatencyEvent) {}
}

pub struct TauriStatusSink {
    app: AppHandle,
}

impl TauriStatusSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl StatusSink for TauriStatusSink {
    fn on_status(&self, ev: &ConnectionStatusEvent) {
        // 单一状态出口：先写 AppState.inner，再 emit 事件；前端快照与事件一致。
        if let Some(state) = self
            .app
            .try_state::<std::sync::Arc<crate::state::AppState>>()
        {
            if let Ok(mut g) = state.inner.lock() {
                g.status = ev.status;
                g.status_text = ev.status_text.clone();
                g.peer = ev.peer.clone();
            }
        }
        let _ = self.app.emit(STATUS_EVENT, ev);
        // 阶段 9：连接/状态变化同步刷新托盘菜单“当前状态”项
        crate::tray::sync_from_state(&self.app);
    }

    fn on_latency(&self, ev: &NetworkLatencyEvent) {
        let _ = self.app.emit(LATENCY_EVENT, ev);
    }
}

#[derive(Clone, Debug)]
pub struct ManagerConfig {
    /// 监听/连接端口（生产固定 45888）
    pub port: u16,
    pub heartbeat_secs: u64,
    pub handshake_timeout: Duration,
    pub connect_timeout: Duration,
    /// 自动重连：非用户断开后按保存的对方 IP 退避重连（阶段 8）
    pub reconnect: bool,
}

impl ManagerConfig {
    /// 生产参数：心跳 10 秒、握手 5 秒、连接 5 秒、自动重连开（与开发文档一致）。
    pub fn production(port: u16) -> Self {
        Self {
            port,
            heartbeat_secs: 10,
            handshake_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            reconnect: true,
        }
    }
}

/// 心跳状态（纯逻辑，可单测）：每 tick 发送新 ping；
/// 对应 pong 到达则复位丢失计数；不匹配/过期 pong 不影响计数；连续 3 次丢失判定失联。
#[derive(Debug, Default)]
pub struct HeartbeatState {
    pending: Option<PendingPing>,
    misses: u32,
}

#[derive(Debug)]
struct PendingPing {
    id: String,
    /// 本机发送 ping 时的墙钟毫秒（与协议 sent_at 同源）：RTT 只用本机记录计算，不信任对方时间。
    sent_at: u64,
}

impl HeartbeatState {
    /// 一次心跳 tick：若上一 ping 未应答则丢失计数 +1；达到 3 次返回超时；否则返回新 ping。
    pub fn on_tick(&mut self) -> (Option<(String, u64)>, bool) {
        if self.pending.is_some() {
            self.misses += 1;
        }
        if self.misses >= 3 {
            self.pending = None;
            return (None, true);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let sent_at = protocol::now_millis();
        self.pending = Some(PendingPing {
            id: id.clone(),
            sent_at,
        });
        (Some((id, sent_at)), false)
    }

    /// 收到 pong：仅当与当前 pending ping 匹配时，用本机记录的 ping 发送时间计算 RTT（毫秒）
    /// 并复位计数；否则返回 None（不匹配/过期 pong 不产生延迟事件、不错误重置 misses）。
    pub fn on_pong(&mut self, ping_id: &str) -> Option<u64> {
        if let Some(p) = self.pending.as_ref() {
            if p.id == ping_id {
                let rtt_ms = protocol::now_millis().saturating_sub(p.sent_at);
                self.pending = None;
                self.misses = 0;
                return Some(rtt_ms);
            }
        }
        None
    }

    #[cfg(test)]
    pub fn misses(&self) -> u32 {
        self.misses
    }
}

/// 重连退避状态：成功连接后重置。
struct ReconnectState {
    cancel: CancellationToken,
    attempts: u32,
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self {
            cancel: CancellationToken::new(),
            attempts: 0,
        }
    }
}

/// 重连退避间隔（秒）：2 → 5 → 10 → 20 → 30 → 30 → ...
const RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 20, 30];

/// 退避间隔（毫秒）：基础序列 + 按 device_id 固定的抖动（0~1500ms）。
/// 双方同时断线时若退避完全同步，会持续在同一时刻互相占用槽位而打结；
/// 抖动随 device_id 固定，两台设备必然错峰重试，保证收敛。
fn reconnect_delay_ms(device_id: &str, attempt: u32) -> u64 {
    let idx = (attempt as usize).min(RECONNECT_BACKOFF.len() - 1);
    let jitter_ms = device_id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64))
        % 1500;
    RECONNECT_BACKOFF[idx] * 1000 + jitter_ms
}

struct NetCore {
    device_id: String,
    device_name: String,
    cfg: ManagerConfig,
    /// 阶段 8：自动重连开关（可变，运行时从 AppConfig 同步）
    reconnect_enabled: AtomicBool,
    sink: Arc<dyn StatusSink>,
    zt_provider: Mutex<Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>>,
    /// 阶段 8：当前"已保存的对方 IP"读取器（自动重连目标；lib.rs 注入，从 AppState 读取）
    peer_ip_provider: Mutex<Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>>,
    /// 阶段 7：远程剪贴板落地回调（lib.rs 注入，负责写系统剪贴板 + 更新状态）
    clipboard_landing: Mutex<Option<Arc<dyn Fn(&protocol::ClipboardPayload) + Send + Sync>>>,
    /// 阶段 7：最近已处理的消息 ID（去重），上限 MAX_RECENT_MESSAGE_IDS
    recent_message_ids: Mutex<VecDeque<String>>,
    inner: Mutex<NetInner>,
    /// 阶段 8：自动重连状态（用户主动断开时清除）
    reconnect: Mutex<ReconnectState>,
}

#[derive(Default)]
struct NetInner {
    next_generation: u64,
    listener: Option<ListenerSlot>,
    /// 最近一次绑定/尝试绑定的 ZeroTier IP：相同 IP 重复通知时幂等，不重复启动监听器
    bind_ip: Option<String>,
    conn: Option<ConnSlot>,
}

struct ListenerSlot {
    addr: SocketAddr,
    cancel: CancellationToken,
}

struct ConnSlot {
    generation: u64,
    conn: Arc<Conn>,
}

pub struct Conn {
    generation: u64,
    cancel: CancellationToken,
    user_closed: AtomicBool,
    peer_closed: AtomicBool,
    heartbeat_lost: AtomicBool,
    settled: AtomicBool,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    writer_rx: Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>,
    heartbeat: Mutex<HeartbeatState>,
}

/// 网络管理器：listener + 单一活动连接 + 心跳。克隆开销为一次 Arc 引用计数。
#[derive(Clone)]
pub struct NetworkManager {
    core: Arc<NetCore>,
}

#[derive(Debug, Clone)]
pub enum CloseCause {
    /// 用户主动断开
    User,
    /// 对方发送 disconnect
    Peer,
    /// 心跳连续 3 次无有效 pong
    HeartbeatLost,
    /// 非预期 EOF/读取结束
    Lost,
    /// 协议或连接阶段失败
    Failed(AppError),
}

enum Handled {
    Continue,
    PeerGone,
    Fail(AppError),
}

impl NetworkManager {
    pub fn new(
        device_id: String,
        device_name: String,
        cfg: ManagerConfig,
        sink: Arc<dyn StatusSink>,
    ) -> Self {
        Self {
            core: Arc::new(NetCore {
                device_id,
                device_name,
                reconnect_enabled: AtomicBool::new(cfg.reconnect),
                cfg,
                sink,
                zt_provider: Mutex::new(None),
                peer_ip_provider: Mutex::new(None),
                clipboard_landing: Mutex::new(None),
                recent_message_ids: Mutex::new(VecDeque::new()),
                inner: Mutex::new(NetInner::default()),
                reconnect: Mutex::new(ReconnectState::default()),
            }),
        }
    }

    /// 注入本机 ZeroTier IP 读取器（打破 AppState 与 manager 的循环依赖，lib.rs 中设置）。
    pub fn set_zt_provider(&self, p: Arc<dyn Fn() -> Option<String> + Send + Sync>) {
        *self.core.zt_provider.lock().unwrap() = Some(p);
    }

    /// 阶段 8：配置自动重连开关（来自 AppConfig.auto_reconnect）。
    pub fn set_reconnect(&self, enabled: bool) {
        self.core
            .reconnect_enabled
            .store(enabled, Ordering::Relaxed);
    }

    /// 注入"已保存的对方 IP"读取器（自动重连目标，阶段 8）。断线后由重连任务读取最新值。
    pub fn set_peer_ip_provider(&self, p: Arc<dyn Fn() -> Option<String> + Send + Sync>) {
        *self.core.peer_ip_provider.lock().unwrap() = Some(p);
    }

    fn peer_ip_now(&self) -> Option<String> {
        self.core
            .peer_ip_provider
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p())
    }

    /// 注入远程剪贴板落地回调（阶段 7）：收到合法 clipboard_update 时调用，
    /// 回调负责写入系统剪贴板并更新状态。贪心：协议层只做校验，落地逻辑在注入侧。
    pub fn set_clipboard_landing(&self, f: Arc<dyn Fn(&protocol::ClipboardPayload) + Send + Sync>) {
        *self.core.clipboard_landing.lock().unwrap() = Some(f);
    }

    /// 发送一条消息：编码后经当前活动连接的 writer 通道发送。
    /// 无活动连接 / writer 通道已关闭返回 Err；当前仅限已连接后使用。
    pub fn send_message(&self, msg: &protocol::Message) -> Result<(), AppError> {
        let conn = {
            let g = self.core.inner.lock().unwrap();
            g.conn.as_ref().map(|s| s.conn.clone())
        };
        let Some(conn) = conn else {
            return Err(AppError::ConnectionClosed);
        };
        let bytes = protocol::encode(msg)?;
        conn.writer_tx
            .send(bytes)
            .map_err(|_| AppError::ConnectionClosed)
    }

    /// 阶段 7：消息 ID 去重。已见过的 ID 返回 true（调用方忽略该消息）；否则记录并返回 false。
    fn is_duplicate_message(&self, message_id: &str) -> bool {
        let mut q = self.core.recent_message_ids.lock().unwrap();
        if q.iter().any(|id| id == message_id) {
            return true;
        }
        q.push_back(message_id.to_string());
        while q.len() > MAX_RECENT_MESSAGE_IDS {
            q.pop_front();
        }
        false
    }

    fn zt_ip_now(&self) -> Option<String> {
        self.core
            .zt_provider
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p())
    }

    /// 是否有活动/建立中的连接（单一通道保护）。
    pub fn is_busy(&self) -> bool {
        self.core.inner.lock().unwrap().conn.is_some()
    }

    pub fn listener_local_addr(&self) -> Option<SocketAddr> {
        self.core
            .inner
            .lock()
            .unwrap()
            .listener
            .as_ref()
            .map(|l| l.addr)
    }

    /// 按当前 ZeroTier IP 启停 listener（幂等）：
    /// - None → 停止监听（IP 失效时不再监听失效地址）
    /// - Some(ip) 与上次相同 → 无操作（不重复启动）
    /// - Some(ip) 变化 → 取消旧监听任务，绑定 <ip>:port；绑定失败只记录错误类型与地址，不 panic。
    pub async fn sync_listener(&self, zt_ip: Option<&str>) {
        let want = zt_ip.map(str::to_string);
        let (old_cancel, last) = {
            let g = self.core.inner.lock().unwrap();
            let last = g.bind_ip.clone();
            let c = g.listener.as_ref().map(|l| l.cancel.clone());
            (c, last)
        };
        if last == want {
            return;
        }
        if let Some(c) = old_cancel {
            c.cancel();
        }
        let Some(target) = want else {
            let mut g = self.core.inner.lock().unwrap();
            g.bind_ip = None;
            g.listener = None;
            tracing::info!("ZeroTier IP 失效，监听已停止");
            return;
        };
        {
            let mut g = self.core.inner.lock().unwrap();
            g.bind_ip = Some(target.clone());
        }
        let bind_addr = match target.parse::<Ipv4Addr>() {
            Ok(ip) => SocketAddr::new(ip.into(), self.core.cfg.port),
            Err(_) => {
                tracing::warn!(ip=%target, "ZeroTier IP 非法，未启动监听");
                return;
            }
        };
        match TcpListener::bind(&bind_addr).await {
            Ok(listener) => {
                let local = listener.local_addr().unwrap_or(bind_addr);
                let cancel = CancellationToken::new();
                {
                    let mut g = self.core.inner.lock().unwrap();
                    g.listener = Some(ListenerSlot {
                        addr: local,
                        cancel: cancel.clone(),
                    });
                }
                tracing::info!(addr=%local, "TCP 监听已启动");
                let m = self.clone();
                tokio::spawn(async move {
                    m.accept_loop(listener, cancel).await;
                });
            }
            Err(e) => {
                let mut g = self.core.inner.lock().unwrap();
                g.listener = None;
                tracing::warn!(
                    addr = %bind_addr,
                    kind = %e.kind(),
                    "TCP 监听绑定失败（端口可能被占用）"
                );
            }
        }
    }

    async fn accept_loop(&self, listener: TcpListener, cancel: CancellationToken) {
        loop {
            tokio::select! {
                            _ = cancel.cancelled() => break,
                            res = listener.accept() => {
                                match res {
                                    Ok((mut stream, addr)) => {
                                        let _ = stream.set_nodelay(true);
                                        let peer_ip = addr.to_string();
                                        match self.claim_conn() {
            Ok(conn) => {
                                                 self.emit_if_current(&ConnectionStatusEvent {
                                                    status: ConnectionStatus::Connecting,
                                                    status_text: "正在连接对方……".into(),
                                                    error_code: None,
                                                    peer: None,
                                                    generation: conn.generation,
                                                });
                                                tracing::info!(gen = conn.generation, %addr, "收到入站连接");
                                                let m = self.clone();
                                                tokio::spawn(async move {
                                                    m.drive(conn, Some(stream), true, peer_ip, 0).await;
                                                });
                                            }
                                            Err(_) => {
                                                tracing::debug!(%addr, "已有活动连接，拒绝新的入站连接");
                                                let _ = stream.shutdown().await;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(kind = %e.kind(), "accept 出错");
                                        time::sleep(Duration::from_millis(100)).await;
                                    }
                                }
                            }
                        }
        }
    }

    /// 抢占单一连接槽位；已有活动/建立中连接时返回 AlreadyConnected。
    pub fn claim_conn(&self) -> Result<Arc<Conn>, AppError> {
        let mut g = self.core.inner.lock().unwrap();
        if g.conn.is_some() {
            return Err(AppError::AlreadyConnected);
        }
        g.next_generation += 1;
        let gen = g.next_generation;
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let conn = Arc::new(Conn {
            generation: gen,
            cancel: CancellationToken::new(),
            user_closed: AtomicBool::new(false),
            peer_closed: AtomicBool::new(false),
            heartbeat_lost: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            writer_tx: tx,
            writer_rx: Mutex::new(Some(rx)),
            heartbeat: Mutex::new(HeartbeatState::default()),
        });
        g.conn = Some(ConnSlot {
            generation: gen,
            conn: conn.clone(),
        });
        Ok(conn)
    }

    /// 主动连接：校验由 commands 层完成；这里只做槽位保护 + 5 秒超时 TCP 连接 + 握手。
    /// 用户主动连接会取消未完成的重连任务（与重连共用单一槽位）。
    pub async fn connect(&self, peer_ip: &str, port: u16) -> Result<(), AppError> {
        self.cancel_reconnect();
        let conn = self.claim_conn()?;
        let gen = conn.generation;
        self.emit_if_current(&ConnectionStatusEvent {
            status: ConnectionStatus::Connecting,
            status_text: "正在连接对方……".into(),
            error_code: None,
            peer: None,
            generation: gen,
        });
        tracing::info!(gen, peer_ip, port, "开始主动连接");
        let m = self.clone();
        let peer_ip = peer_ip.to_string();
        tokio::spawn(async move {
            m.drive(conn, None, false, peer_ip, port).await;
        });
        Ok(())
    }

    /// 用户主动断开：尽量发送 disconnect，取消全部任务并回到 Offline。
    /// 不自动重连；不清理配置中的 last_peer_ip。
    pub async fn disconnect(&self) {
        self.cancel_reconnect();
        let conn = {
            let g = self.core.inner.lock().unwrap();
            g.conn.as_ref().map(|s| s.conn.clone())
        };
        let Some(conn) = conn else {
            return;
        };
        conn.user_closed.store(true, Ordering::SeqCst);
        if let Ok(msg) = protocol::Message::new(
            protocol::MSG_DISCONNECT,
            &self.core.device_id,
            &protocol::DisconnectPayload {
                reason: protocol::REASON_USER_REQUESTED.into(),
            },
        ) {
            if let Ok(bytes) = protocol::encode(&msg) {
                let _ = conn.writer_tx.send(bytes);
            }
        }
        conn.cancel.cancel();
        self.finalize(conn, CloseCause::User);
    }

    /// 取消未完成的重连任务、重置退避计数（用户主动连接/断开、应用退出时调用）。
    pub fn cancel_reconnect(&self) {
        let mut rs = self.core.reconnect.lock().unwrap();
        rs.cancel.cancel();
        rs.attempts = 0;
        // 默认 token 已取消不可复用；下次 start_reconnect 会替换为新 token。
    }

    /// 开始一次自动重连：读取当前保存的对方 IP，按退避间隔派发重连任务。
    /// 返回 true 表示已调度；否则（重连未开启 / 无对方 IP）返回 false。
    /// 每轮尝试失败（连接任务 finalize 触发）会再次进入本方法，退避间隔逐级递增；
    /// 成功后 drive() 中重置计数（attempts=0），下次断线从头退避。
    fn start_reconnect(&self) -> bool {
        if !self.core.reconnect_enabled.load(Ordering::Relaxed) {
            return false;
        }
        let Some(peer) = self.peer_ip_now() else {
            tracing::debug!("自动重连：未保存对方 IP，跳过");
            return false;
        };
        let (delay_secs, cancel, attempt) = {
            let mut rs = self.core.reconnect.lock().unwrap();
            rs.cancel.cancel();
            rs.cancel = CancellationToken::new();
            let attempt = rs.attempts;
            let d = reconnect_delay_ms(&self.core.device_id, attempt);
            rs.attempts += 1;
            (d, rs.cancel.clone(), attempt)
        };
        let m = self.clone();
        let port = self.core.cfg.port;
        tracing::info!(peer = %peer, delay_ms = delay_secs, attempt, "自动重连已调度");
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = time::sleep(Duration::from_millis(delay_secs)) => {
                    m.reconnect_attempt(peer, port).await;
                }
            }
        });
        true
    }

    async fn reconnect_attempt(&self, peer: String, port: u16) {
        // 已有活动连接（用户已连接 / 对端已连入）时跳过本轮，不再安排下一轮
        if self.is_busy() {
            tracing::debug!(peer = %peer, "自动重连：已有活动连接，停止本轮");
            return;
        }
        tracing::info!(peer = %peer, "自动重连：开始尝试建立连接");
        if let Err(e) = self.connect(&peer, port).await {
            tracing::warn!(peer = %peer, %e, "自动重连启动失败（槽位/编码）");
            // 未进入连接任务（无 finalize 触发下一轮），保持“重试中”并手动安排下一轮
            self.emit(&ConnectionStatusEvent {
                status: ConnectionStatus::Reconnecting,
                status_text: crate::zerotier::STATUS_RECONNECTING.to_string(),
                error_code: None,
                peer: None,
                generation: 0,
            });
            self.start_reconnect();
        }
        // connect 成功时连接任务在后台进行；成功/失败分别经 drive 的
        // “重置 attempts” / “finalize → start_reconnect” 路径继续。
    }

    /// 停止 listener 与活动连接（测试收尾/应用退出）。
    pub async fn shutdown(&self) {
        self.cancel_reconnect();
        {
            let g = self.core.inner.lock().unwrap();
            if let Some(l) = &g.listener {
                l.cancel.cancel();
            }
            if let Some(c) = &g.conn {
                c.conn.cancel.cancel();
            }
        }
        time::sleep(Duration::from_millis(50)).await;
        let mut g = self.core.inner.lock().unwrap();
        g.listener = None;
        g.bind_ip = None;
        g.conn = None;
    }

    fn build_hello(&self) -> protocol::Message {
        protocol::Message::new(
            protocol::MSG_HELLO,
            &self.core.device_id,
            &protocol::HelloPayload {
                device_id: self.core.device_id.clone(),
                device_name: self.core.device_name.clone(),
                protocol_version: protocol::PROTOCOL_VERSION,
            },
        )
        .expect("本机身份已在配置阶段校验，hello 编码不应失败")
    }

    /// 连接主流程：(出站则先 TCP 连接) → writer 任务 → hello 握手 → Connected →
    /// 心跳任务 → 读循环 → 结束（finalize 保证旧连接不影响新连接状态）。
    async fn drive(
        &self,
        conn: Arc<Conn>,
        stream: Option<TcpStream>,
        inbound: bool,
        peer_ip: String,
        port: u16,
    ) {
        let stream = match stream {
            Some(s) => s,
            None => {
                let addr = match peer_ip.parse::<Ipv4Addr>() {
                    Ok(ip) => SocketAddr::new(ip.into(), port),
                    Err(_) => {
                        self.finalize(conn.clone(), CloseCause::Failed(AppError::InvalidPeerIp));
                        return;
                    }
                };
                let c = tokio::select! {
                    _ = conn.cancel.cancelled() => {
                        self.finalize(conn.clone(), CloseCause::User);
                        return;
                    }
                    r = time::timeout(self.core.cfg.connect_timeout, TcpStream::connect(addr)) => r,
                };
                match c {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        tracing::warn!(%addr, kind = %e.kind(), "TCP 连接失败");
                        self.finalize(conn.clone(), CloseCause::Failed(AppError::ConnectFailed));
                        return;
                    }
                    Err(_) => {
                        tracing::warn!(%addr, "TCP 连接超时");
                        self.finalize(conn.clone(), CloseCause::Failed(AppError::ConnectTimeout));
                        return;
                    }
                }
            }
        };
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();

        // writer 任务：唯一写 socket 的任务，串行发送通道中的帧
        let writer = {
            let conn2 = conn.clone();
            async move {
                let mut rx = match conn2.writer_rx.lock().unwrap().take() {
                    Some(r) => r,
                    None => return,
                };
                loop {
                    tokio::select! {
                        _ = conn2.cancel.cancelled() => {
                            // 取消后先排空已入队帧（如用户 disconnect 消息），再关闭
                            while let Ok(bytes) = rx.try_recv() {
                                if wr.write_all(&bytes).await.is_err() {
                                    break;
                                }
                            }
                            break;
                        }
                        item = rx.recv() => {
                            match item {
                                Some(bytes) => {
                                    if wr.write_all(&bytes).await.is_err() {
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
        };
        tokio::spawn(writer);

        let peer = match self.handshake(&conn, &mut rd, inbound, &peer_ip).await {
            Ok(p) => p,
            Err(e) => {
                conn.cancel.cancel();
                let cause = if conn.user_closed.load(Ordering::Relaxed) {
                    CloseCause::User
                } else {
                    CloseCause::Failed(e)
                };
                self.finalize(conn.clone(), cause);
                return;
            }
        };
        tracing::info!(
            gen = conn.generation,
            peer = %peer.device_name,
            peer_ip = %peer_ip,
            "hello 交换完成，连接已建立"
        );

        // 阶段 8：双方同时主动连接时的冲突收敛。
        // 单槽位设计下，双方各自 connect() 会先占住槽位，因此对方的入站必然在
        // accept_loop 的 busy 分支被拒绝，握手无法完成，无需在 drive 内按 device_id 判定；
        // 两侧会在各自退避一轮后由一方重连成功、另一方以入站方式接入，最终仍只保留一条通道。
        self.emit_if_current(&ConnectionStatusEvent {
            status: ConnectionStatus::Connected,
            status_text: crate::zerotier::STATUS_CONNECTED.to_string(),
            error_code: None,
            peer: Some(peer),
            generation: conn.generation,
        });
        // 阶段 8：连接成功，重连退避计数从头开始（下次断线从 2 秒退避起算）。
        self.core.reconnect.lock().unwrap().attempts = 0;

        let hb_secs = self.core.cfg.heartbeat_secs;
        let device_id = self.core.device_id.clone();
        let heartbeat = {
            let conn2 = conn.clone();
            async move {
                let mut interval = time::interval(Duration::from_secs(hb_secs));
                interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = conn2.cancel.cancelled() => break,
                        _ = interval.tick() => {}
                    }
                    let (ping, timed_out) = conn2.heartbeat.lock().unwrap().on_tick();
                    if timed_out {
                        conn2.heartbeat_lost.store(true, Ordering::SeqCst);
                        tracing::warn!(gen = conn2.generation, "心跳连续 3 次无 pong，判定失联");
                        conn2.cancel.cancel();
                        break;
                    }
                    if let Some((id, sent_at)) = ping {
                        if let Ok(msg) = protocol::Message::new(
                            protocol::MSG_PING,
                            &device_id,
                            &protocol::PingPayload {
                                ping_id: id,
                                sent_at,
                            },
                        ) {
                            if let Ok(bytes) = protocol::encode(&msg) {
                                let _ = conn2.writer_tx.send(bytes);
                            }
                        }
                    }
                }
            }
        };
        tokio::spawn(heartbeat);

        let cause = self.reader_loop(conn.clone(), &mut rd).await;
        conn.cancel.cancel();
        self.finalize(conn.clone(), cause);
    }

    /// hello 握手：出站先发送再读取；入站先读取再发送。
    /// 版本不兼容时回最小 error 消息后关闭；hello 超时（与连接阶段一致的超时）判 HandshakeTimeout。
    /// 返回 (对方摘要, 对方 device_id)——device_id 供同时连接冲突判定（阶段 8），不进前端。
    async fn handshake(
        &self,
        conn: &Arc<Conn>,
        rd: &mut tokio::net::tcp::OwnedReadHalf,
        inbound: bool,
        peer_ip: &str,
    ) -> Result<PeerInfo, AppError> {
        let timeout = self.core.cfg.handshake_timeout;
        let their = if inbound {
            self.read_hello_or_cancel(conn, rd, timeout).await?
        } else {
            let hello = self.build_hello();
            let bytes = protocol::encode(&hello)?;
            if conn.writer_tx.send(bytes).is_err() {
                return Err(AppError::ConnectionClosed);
            }
            self.read_hello_or_cancel(conn, rd, timeout).await?
        };
        if their.version != protocol::PROTOCOL_VERSION {
            let err = protocol::Message::new(
                protocol::MSG_ERROR,
                &self.core.device_id,
                &protocol::ErrorPayload {
                    reason: protocol::REASON_VERSION_MISMATCH.into(),
                },
            );
            if let Ok(err) = err {
                if let Ok(bytes) = protocol::encode(&err) {
                    let _ = conn.writer_tx.send(bytes);
                }
            }
            tracing::warn!(
                gen = conn.generation,
                version = their.version,
                "协议版本不兼容"
            );
            return Err(AppError::ProtocolVersionMismatch);
        }
        let payload = protocol::validate_hello(&their, &self.core.device_id)?;
        if inbound {
            let hello = self.build_hello();
            let bytes = protocol::encode(&hello)?;
            if conn.writer_tx.send(bytes).is_err() {
                return Err(AppError::ConnectionClosed);
            }
        }
        Ok(PeerInfo {
            device_name: payload.device_name.trim().to_string(),
            ip: peer_ip.to_string(),
        })
    }

    async fn read_hello_or_cancel(
        &self,
        conn: &Arc<Conn>,
        rd: &mut tokio::net::tcp::OwnedReadHalf,
        timeout: Duration,
    ) -> Result<protocol::Message, AppError> {
        tokio::select! {
            _ = conn.cancel.cancelled() => Err(AppError::ConnectionClosed),
            r = time::timeout(timeout, protocol::read_message(rd)) => match r {
                Ok(Ok(m)) => {
                    tracing::debug!(gen = conn.generation, t = %m.msg_type, "握手阶段收到消息");
                    Ok(m)
                }
                Ok(Err(e)) => Err(e),
                Err(_) => {
                    tracing::warn!(gen = conn.generation, "握手超时（对方未发送 hello）");
                    Err(AppError::HandshakeTimeout)
                }
            },
        }
    }

    async fn reader_loop(
        &self,
        conn: Arc<Conn>,
        rd: &mut tokio::net::tcp::OwnedReadHalf,
    ) -> CloseCause {
        loop {
            tokio::select! {
                _ = conn.cancel.cancelled() => {
                    return self.cause_from_flags(&conn);
                }
                res = protocol::read_message(rd) => {
                    match res {
                        Err(e) => {
                            let code = e.code();
                            let c = self.cause_from_flags_or(&conn, e);
                            if matches!(c, CloseCause::Lost | CloseCause::Failed(_)) {
                                tracing::warn!(gen = conn.generation, code, "连接读取结束");
                            }
                            return c;
                        }
                        Ok(msg) => {
                            tracing::debug!(gen = conn.generation, t = %msg.msg_type, "收到消息");
                            match self.handle_message(&conn, &msg) {
                                Handled::Continue => {}
                                Handled::PeerGone => {
                                    conn.peer_closed.store(true, Ordering::SeqCst);
                                    tracing::info!(gen = conn.generation, "对方发送 disconnect");
                                    return CloseCause::Peer;
                                }
                                Handled::Fail(e) => {
                                    tracing::warn!(gen = conn.generation, code = e.code(), "协议错误，关闭连接");
                                    return CloseCause::Failed(e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_message(&self, conn: &Arc<Conn>, msg: &protocol::Message) -> Handled {
        if msg.version != protocol::PROTOCOL_VERSION {
            return Handled::Fail(AppError::ProtocolVersionMismatch);
        }
        match msg.msg_type.as_str() {
            protocol::MSG_PING => match protocol::parse_ping(msg) {
                Some(ping) => {
                    if let Ok(pong) = protocol::Message::new(
                        protocol::MSG_PONG,
                        &self.core.device_id,
                        &protocol::PongPayload {
                            ping_id: ping.ping_id,
                            sent_at: ping.sent_at,
                        },
                    ) {
                        if let Ok(bytes) = protocol::encode(&pong) {
                            let _ = conn.writer_tx.send(bytes);
                        }
                    }
                    Handled::Continue
                }
                None => Handled::Fail(AppError::ProtocolInvalidJson),
            },
            protocol::MSG_PONG => match protocol::parse_pong(msg) {
                Some(pong) => {
                    let rtt_ms = conn.heartbeat.lock().unwrap().on_pong(&pong.ping_id);
                    if let Some(ms) = rtt_ms {
                        self.emit_latency_if_current(conn.generation, ms);
                    }
                    tracing::debug!(gen = conn.generation, rtt_ms = ?rtt_ms, "收到 pong");
                    Handled::Continue
                }
                None => Handled::Fail(AppError::ProtocolInvalidJson),
            },
            // 策略：同一连接中重复 hello 明确忽略（不计协议错误），避免对端重发导致误断开
            protocol::MSG_HELLO => {
                tracing::debug!(gen = conn.generation, "重复 hello，忽略");
                Handled::Continue
            }
            protocol::MSG_DISCONNECT => Handled::PeerGone,
            protocol::MSG_ERROR => Handled::Fail(AppError::ConnectionClosed),
            // 阶段 7：远程剪贴板落地。校验失败/重复 ID 只忽略不中断连接（数据面消息，非协议违规）。
            protocol::MSG_CLIPBOARD_UPDATE => {
                if self.is_duplicate_message(&msg.message_id) {
                    tracing::debug!(gen = conn.generation, "重复剪贴板消息 ID，忽略");
                    return Handled::Continue;
                }
                match protocol::validate_clipboard(msg) {
                    Ok(payload) => {
                        if let Some(f) = self.core.clipboard_landing.lock().unwrap().as_ref() {
                            f(&payload);
                        }
                        Handled::Continue
                    }
                    Err(e) => {
                        tracing::warn!(
                            gen = conn.generation,
                            code = e.code(),
                            "非法剪贴板消息，忽略"
                        );
                        Handled::Continue
                    }
                }
            }
            _ => {
                tracing::debug!(gen = conn.generation, t = %msg.msg_type, "未知消息类型，忽略");
                Handled::Continue
            }
        }
    }

    fn cause_from_flags(&self, conn: &Conn) -> CloseCause {
        if conn.user_closed.load(Ordering::Relaxed) {
            CloseCause::User
        } else if conn.peer_closed.load(Ordering::Relaxed) {
            CloseCause::Peer
        } else if conn.heartbeat_lost.load(Ordering::Relaxed) {
            CloseCause::HeartbeatLost
        } else {
            CloseCause::Lost
        }
    }

    fn cause_from_flags_or(&self, conn: &Conn, e: AppError) -> CloseCause {
        if conn.user_closed.load(Ordering::Relaxed) {
            CloseCause::User
        } else if conn.peer_closed.load(Ordering::Relaxed) {
            CloseCause::Peer
        } else if conn.heartbeat_lost.load(Ordering::Relaxed) {
            CloseCause::HeartbeatLost
        } else {
            CloseCause::Failed(e)
        }
    }

    /// 连接终结：settled 标记保证只生效一次；只有槽位仍属于该 generation 时才清理槽位并发出
    /// 终结事件，旧连接任务不会覆盖新连接状态。
    pub fn finalize(&self, conn: Arc<Conn>, cause: CloseCause) {
        if conn.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        let gen = conn.generation;
        let owns = {
            let mut g = self.core.inner.lock().unwrap();
            let owns = g.conn.as_ref().is_some_and(|s| s.generation == gen);
            if owns {
                g.conn = None;
            }
            owns
        };
        if !owns {
            tracing::debug!(gen, "旧连接结束，忽略其状态事件");
            return;
        }

        // 阶段 8：非用户断开且已保存对方 IP 时进入自动重连，界面直接呈现“正在重试”。
        if !matches!(&cause, CloseCause::User) && self.start_reconnect() {
            tracing::info!(gen, "连接结束，进入自动重连");
            self.emit(&ConnectionStatusEvent {
                status: ConnectionStatus::Reconnecting,
                status_text: crate::zerotier::STATUS_RECONNECTING.to_string(),
                error_code: None,
                peer: None,
                generation: gen,
            });
            return;
        }

        let (status, text, code) = self.terminal(&cause);
        tracing::info!(gen, ?status, code = ?code, "连接结束");
        self.emit(&ConnectionStatusEvent {
            status,
            status_text: text,
            error_code: code,
            peer: None,
            generation: gen,
        });
    }

    fn terminal(&self, cause: &CloseCause) -> (ConnectionStatus, String, Option<String>) {
        match cause {
            CloseCause::User => {
                let text = if self.zt_ip_now().is_some() {
                    crate::zerotier::STATUS_WAITING_INPUT.to_string()
                } else {
                    crate::zerotier::STATUS_NO_ZT.to_string()
                };
                (ConnectionStatus::Offline, text, None)
            }
            CloseCause::Peer => (
                ConnectionStatus::Offline,
                "对方已断开连接。".to_string(),
                None,
            ),
            CloseCause::HeartbeatLost => (
                ConnectionStatus::Error,
                "连接已中断。".to_string(),
                Some("heartbeat_timeout".to_string()),
            ),
            CloseCause::Lost => (
                ConnectionStatus::Error,
                "连接已中断。".to_string(),
                Some("connection_closed".to_string()),
            ),
            CloseCause::Failed(e) => (
                ConnectionStatus::Error,
                e.user_text(),
                Some(e.code().to_string()),
            ),
        }
    }

    fn emit(&self, ev: &ConnectionStatusEvent) {
        (self.core.sink.as_ref()).on_status(ev);
    }

    /// 当前连接槽位是否仍属于指定 generation（旧连接晚到的状态/RTT 事件不应覆盖新连接）。
    fn generation_is_current(&self, generation: u64) -> bool {
        self.core
            .inner
            .lock()
            .unwrap()
            .conn
            .as_ref()
            .is_some_and(|s| s.generation == generation)
    }

    /// 仅当前活动连接（generation 匹配槽位）可发送非终结事件；
    /// 旧连接任务在终结前若已被新连接取代，其事件不再发出，避免覆盖新连接状态。
    fn emit_if_current(&self, ev: &ConnectionStatusEvent) {
        if self.generation_is_current(ev.generation) {
            self.emit(ev);
        }
    }

    /// RTT 事件同样按 generation 过滤：旧连接产生的 RTT 不得更新新连接的延迟。
    fn emit_latency_if_current(&self, conn_generation: u64, rtt_ms: u64) {
        if self.generation_is_current(conn_generation) {
            (self.core.sink.as_ref()).on_latency(&NetworkLatencyEvent {
                latency_ms: rtt_ms,
                generation: conn_generation,
            });
        }
    }
}

/// 连接目标校验：trim 后必须为合法 IPv4；拒绝 0.0.0.0/回环/链路本地/广播/组播/保留段
/// （复用 zerotier::is_valid_ipv4）；拒绝本机当前 ZeroTier IPv4。
pub fn validate_peer_target(ip: &str, local_zt_ip: Option<&str>) -> Result<Ipv4Addr, AppError> {
    let ip = ip.trim();
    let parsed = ip
        .parse::<Ipv4Addr>()
        .map_err(|_| AppError::InvalidPeerIp)?;
    if !crate::zerotier::is_valid_ipv4(u32::from(parsed)) {
        return Err(AppError::InvalidPeerIp);
    }
    if let Some(z) = local_zt_ip {
        if z.trim() == ip {
            return Err(AppError::SelfConnection);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VecSink(Mutex<Vec<ConnectionStatusEvent>>);

    impl VecSink {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }
        fn last(&self) -> Option<ConnectionStatusEvent> {
            self.0.lock().unwrap().last().cloned()
        }
    }

    impl StatusSink for VecSink {
        fn on_status(&self, ev: &ConnectionStatusEvent) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }

    fn test_manager() -> (NetworkManager, Arc<VecSink>) {
        let sink = Arc::new(VecSink::new());
        let m = NetworkManager::new(
            "550e8400-e29b-41d4-a716-446655440000".into(),
            "TEST-A".into(),
            ManagerConfig {
                port: 0,
                heartbeat_secs: 1,
                handshake_timeout: Duration::from_millis(300),
                connect_timeout: Duration::from_millis(300),
                reconnect: false,
            },
            sink.clone(),
        );
        (m, sink)
    }

    // 23/28：槽位保护——已有连接时新的 claim 被拒绝（AlreadyConnected）
    #[test]
    fn claim_protects_single_channel() {
        let (m, sink) = test_manager();
        let c1 = m.claim_conn().unwrap();
        assert!(matches!(m.claim_conn(), Err(AppError::AlreadyConnected)));
        m.finalize(c1, CloseCause::User);
        assert!(!m.is_busy());
        let ev = sink.last().unwrap();
        assert_eq!(ev.status, ConnectionStatus::Offline);
    }

    // 27：旧 connection_id（generation）不能清理/覆盖新连接状态
    #[test]
    fn stale_generation_cannot_touch_new_connection() {
        let (m, sink) = test_manager();
        let c1 = m.claim_conn().unwrap();
        m.finalize(c1.clone(), CloseCause::User);
        let c2 = m.claim_conn().unwrap();
        assert_ne!(c2.generation, c1.generation);
        // c1 已 settled：再次 finalize 是 no-op，不影响 c2
        m.finalize(c1.clone(), CloseCause::Lost);
        assert!(m.is_busy());
        let ev = sink.last().unwrap();
        assert_eq!(ev.generation, c1.generation, "旧连接的终结事件先于新连接");
        m.finalize(c2.clone(), CloseCause::Peer);
        assert!(!m.is_busy());
        let ev = sink.last().unwrap();
        assert_eq!(ev.generation, c2.generation);
        assert_eq!(ev.status, ConnectionStatus::Offline);
    }

    // 31：ping 收到对应 pong 后保持连接（丢失计数复位），并返回本机计算的 RTT
    #[test]
    fn heartbeat_pong_keeps_connection() {
        let mut hb = HeartbeatState::default();
        let (p1, t1) = hb.on_tick();
        assert!(!t1);
        let id = p1.unwrap().0;
        let rtt = hb.on_pong(&id).expect("匹配 pong 应返回 RTT");
        assert!(rtt < 5000, "RTT 应为本机记录的合理毫秒数，得到 {rtt}");
        let (p2, t2) = hb.on_tick();
        assert!(!t2);
        assert!(p2.is_some());
        assert_eq!(hb.misses(), 0);
    }

    // 32：不匹配/过期 pong 不产生 RTT，也不重置丢失计数
    #[test]
    fn heartbeat_mismatched_pong_does_not_reset() {
        let mut hb = HeartbeatState::default();
        let (p1, _) = hb.on_tick();
        assert!(hb.on_pong("other-id").is_none());
        let (_, _) = hb.on_tick();
        assert_eq!(hb.misses(), 1);
        // p1 的迟到 pong：pending 已换成新 ping，不匹配 → 无 RTT、计数不变
        assert!(hb.on_pong(&p1.unwrap().0).is_none());
        assert_eq!(hb.misses(), 1);
    }

    // T11-03A：同一 pending ping 的 pong 只匹配一次（RTT 只产生一次），随后为 None
    #[test]
    fn heartbeat_pong_returns_rtt_only_once() {
        let mut hb = HeartbeatState::default();
        let (p1, _) = hb.on_tick();
        let id = p1.unwrap().0;
        assert!(hb.on_pong(&id).is_some(), "首次匹配应产生 RTT");
        assert!(
            hb.on_pong(&id).is_none(),
            "pending 已消费，重复 pong 不应再产生 RTT"
        );
        assert_eq!(hb.misses(), 0);
    }

    // 33：连续 3 次无 pong 触发超时
    #[test]
    fn heartbeat_three_misses_timeout() {
        let mut hb = HeartbeatState::default();
        let (_, t1) = hb.on_tick();
        assert!(!t1);
        let (_, t2) = hb.on_tick();
        assert!(!t2);
        let (_, t3) = hb.on_tick();
        assert!(!t3);
        let (p4, t4) = hb.on_tick();
        assert!(t4);
        assert!(p4.is_none());
    }

    // 阶段 7：消息 ID 去重——同一 ID 只允许处理一次，队列上限封顶
    #[test]
    fn duplicate_message_ids_dropped() {
        let (m, _) = test_manager();
        assert!(!m.is_duplicate_message("id-1"));
        assert!(m.is_duplicate_message("id-1"), "重复 ID 应被拒绝");
        // 填满队列后最旧的被淘汰：id-1 被弹出，可再次入队
        for i in 0..MAX_RECENT_MESSAGE_IDS {
            m.is_duplicate_message(&format!("gen-{i}"));
        }
        assert!(
            !m.is_duplicate_message("id-1"),
            "队列淘汰后旧 ID 不再命中（环形去重）"
        );
        assert_eq!(
            m.core.recent_message_ids.lock().unwrap().len(),
            MAX_RECENT_MESSAGE_IDS,
            "去重队列应保持上限"
        );
    }

    // 36/37/38/39/40：输入校验
    #[test]
    fn validate_peer_target_rules() {
        let zt = Some("192.168.191.180");
        assert!(validate_peer_target("10.147.17.36", zt).is_ok());
        assert!(validate_peer_target(" 10.147.17.36 ", zt).is_ok());
        for bad in [
            "abc",
            "999.1.1.1",
            "1.2.3",
            "1.2.3.4.5",
            "",
            "0.0.0.0",
            "127.0.0.1",
            "169.254.10.11",
            "255.255.255.255",
            "224.0.0.5",
            "240.1.1.1",
        ] {
            assert!(
                matches!(validate_peer_target(bad, zt), Err(AppError::InvalidPeerIp)),
                "应拒绝 {bad:?}"
            );
        }
        assert!(matches!(
            validate_peer_target("192.168.191.180", zt),
            Err(AppError::SelfConnection)
        ));
        assert!(validate_peer_target("192.168.191.181", zt).is_ok());
        assert!(validate_peer_target("10.0.0.1", None).is_ok());
        // 生产端口固定 45888
        assert_eq!(ManagerConfig::production(45888).port, 45888);
        assert_eq!(crate::config::DEFAULT_LISTEN_PORT, 45888);
    }

    // 阶段 8：退避序列 2→5→10→20→30→30…，且抖动落在 [0,1500ms) 区间内
    #[test]
    fn reconnect_backoff_sequence_and_jitter() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let expected_bases = [2000u64, 5000, 10000, 20000, 30000, 30000];
        for (attempt, base) in expected_bases.iter().enumerate() {
            let d = reconnect_delay_ms(id, attempt as u32);
            let limit = base + 1500;
            assert!(
                d >= *base && d < limit,
                "attempt {attempt}: {d} 应落在 [{base}, {limit})"
            );
        }
        // 抖动随 device_id 固定：两台设备在相同 attempt 下错峰重试
        assert_ne!(
            reconnect_delay_ms("550e8400-550e-41d4-a716-446655440000", 0),
            reconnect_delay_ms("6ba7b810-9dad-11d1-80b4-00c04fd430c8", 0)
        );
    }

    // 阶段 8：自动重连开关 —— 关闭时不调度；开启且有对方 IP 时调度并随次数递增退避
    #[tokio::test]
    async fn reconnect_toggled_by_set_reconnect() {
        let (m, _sink) = test_manager();
        let peer = "192.168.191.181".to_string();
        m.set_peer_ip_provider(Arc::new(move || Some(peer.clone())));
        m.set_reconnect(false);
        assert!(!m.start_reconnect(), "重连关闭时应返回 false");
        assert_eq!(m.core.reconnect.lock().unwrap().attempts, 0, "不调度不递增");

        m.set_reconnect(true);
        assert!(m.start_reconnect(), "重连开启且有对方 IP 时应调度");
        assert_eq!(m.core.reconnect.lock().unwrap().attempts, 1);
        // 用户主动断开会取消未完成的重连任务并重置计数
        m.cancel_reconnect();
        assert_eq!(m.core.reconnect.lock().unwrap().attempts, 0);
    }
}
