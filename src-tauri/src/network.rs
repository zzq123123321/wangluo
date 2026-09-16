// 阶段 5：TCP 监听、连接生命周期、读写循环与心跳。
// 架构：NetworkManager 集中管理 listener 任务、当前活动连接、取消信号、writer 通道、
// 心跳状态与连接代次（generation）。所有状态写入/事件经 StatusSink 单一出口，
// 旧连接任务结束时必须仍是当前活动连接（generation 匹配 + settled 标记）才能影响状态。
// 慢速网络 IO 与任务 join 一律在 AppState 锁外；std Mutex 持锁期间无 await。
use crate::error::AppError;
use crate::protocol;
use crate::state::{ConnectionStatus, PeerInfo};
use serde::Serialize;
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

/// 连接状态事件 payload：只含非敏感信息（无 device_secret / shared_key / auth / 原始消息）。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionStatusEvent {
    pub status: ConnectionStatus,
    pub status_text: String,
    pub error_code: Option<String>,
    pub peer: Option<PeerInfo>,
    pub generation: u64,
}

/// 状态事件出口：生产环境为 TauriStatusSink（写 AppState + emit），测试用收集型实现。
pub trait StatusSink: Send + Sync {
    fn on_status(&self, ev: &ConnectionStatusEvent);
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
    }
}

#[derive(Clone, Debug)]
pub struct ManagerConfig {
    /// 监听/连接端口（生产固定 45888）
    pub port: u16,
    pub heartbeat_secs: u64,
    pub handshake_timeout: Duration,
    pub connect_timeout: Duration,
}

impl ManagerConfig {
    /// 生产参数：心跳 10 秒、握手 5 秒、连接 5 秒（与开发文档一致）。
    pub fn production(port: u16) -> Self {
        Self {
            port,
            heartbeat_secs: 10,
            handshake_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
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
        self.pending = Some(PendingPing { id: id.clone() });
        (Some((id, sent_at)), false)
    }

    /// 收到 pong：仅当与当前 pending ping 匹配时复位计数；否则忽略（不错误恢复）。
    pub fn on_pong(&mut self, ping_id: &str) -> bool {
        if let Some(p) = self.pending.as_ref() {
            if p.id == ping_id {
                self.pending = None;
                self.misses = 0;
                return true;
            }
        }
        false
    }

    #[cfg(test)]
    pub fn misses(&self) -> u32 {
        self.misses
    }
}

struct NetCore {
    device_id: String,
    device_name: String,
    cfg: ManagerConfig,
    sink: Arc<dyn StatusSink>,
    zt_provider: Mutex<Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>>,
    inner: Mutex<NetInner>,
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
                cfg,
                sink,
                zt_provider: Mutex::new(None),
                inner: Mutex::new(NetInner::default()),
            }),
        }
    }

    /// 注入本机 ZeroTier IP 读取器（打破 AppState 与 manager 的循环依赖，lib.rs 中设置）。
    pub fn set_zt_provider(&self, p: Arc<dyn Fn() -> Option<String> + Send + Sync>) {
        *self.core.zt_provider.lock().unwrap() = Some(p);
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
    pub async fn connect(&self, peer_ip: &str, port: u16) -> Result<(), AppError> {
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

    /// 停止 listener 与活动连接（测试收尾/应用退出）。
    pub async fn shutdown(&self) {
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
        self.emit_if_current(&ConnectionStatusEvent {
            status: ConnectionStatus::Connected,
            status_text: "已连接，剪贴板同步已开启。".into(),
            error_code: None,
            peer: Some(peer),
            generation: conn.generation,
        });

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
                    let matched = conn.heartbeat.lock().unwrap().on_pong(&pong.ping_id);
                    tracing::debug!(gen = conn.generation, matched, "收到 pong");
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

    /// 仅当前活动连接（generation 匹配槽位）可发送非终结事件；
    /// 旧连接任务在终结前若已被新连接取代，其事件不再发出，避免覆盖新连接状态。
    fn emit_if_current(&self, ev: &ConnectionStatusEvent) {
        let owns = {
            let g = self.core.inner.lock().unwrap();
            g.conn
                .as_ref()
                .is_some_and(|s| s.generation == ev.generation)
        };
        if owns {
            self.emit(ev);
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

    // 31：ping 收到对应 pong 后保持连接（丢失计数复位）
    #[test]
    fn heartbeat_pong_keeps_connection() {
        let mut hb = HeartbeatState::default();
        let (p1, t1) = hb.on_tick();
        assert!(!t1);
        let id = p1.unwrap().0;
        assert!(hb.on_pong(&id));
        let (p2, t2) = hb.on_tick();
        assert!(!t2);
        assert!(p2.is_some());
        assert_eq!(hb.misses(), 0);
    }

    // 32：不匹配/过期 pong 不重置丢失计数
    #[test]
    fn heartbeat_mismatched_pong_does_not_reset() {
        let mut hb = HeartbeatState::default();
        let (p1, _) = hb.on_tick();
        assert!(!hb.on_pong("other-id"));
        let (_, _) = hb.on_tick();
        assert_eq!(hb.misses(), 1);
        // p1 的迟到 pong：pending 已换成新 ping，不匹配 → 计数不变
        assert!(!hb.on_pong(&p1.unwrap().0));
        assert_eq!(hb.misses(), 1);
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
}
