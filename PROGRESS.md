# ClipLink 开发进度

> 本文件是跨会话接力的唯一进度来源。新会话开始开发前，先读 `docs/开发文档.md`（需求）和本文件（进度）。
> 每完成一个阶段，立即更新本文件。

## 阶段状态

| 阶段 | 内容 | 状态 |
|---|---|---|
| 阶段 0 | 准备环境 | ✅ 完成（2026-09-16） |
| 阶段 1 | 项目骨架 | ✅ 完成（2026-09-16） |
| 阶段 2 | 静态界面 | ✅ 完成（2026-09-16） |
| 阶段 3 | ZeroTier IP 检测 | ✅ 完成（2026-09-16） |
| 阶段 4 | 配置和设备身份 | ✅ 完成（2026-09-16） |
| 阶段 5 | TCP 通信基础 | ✅ 完成（2026-09-16） |
| 阶段 6 | 单机剪贴板监听 | ✅ 完成（2026-09-16） |
| 阶段 7 | 双向同步 | ⬜ 未开始 |
| 阶段 8 | 自动重连和冲突处理 | ⬜ 未开始 |
| 阶段 9 | 托盘和开机启动 | ⬜ 未开始 |
| 阶段 10 | 安装包和防火墙 | ⬜ 未开始 |
| 阶段 11 | 测试和发布 | ⬜ 未开始 |

## 环境事实（2026-09-16 核验）

- 开发机：MSI 笔记本（Windows 10 26200，用户 LocalUser），ZeroTier 已运行，本机 ZT IP `192.168.191.180`（网络 xu_network），可作为 A 端测试机。
- Node v24.18.0（npm 有执行策略限制，必须用 `npm.cmd` 或 `cmd /c` 调用）。
- Git 2.54；VS2022 Build Tools 已装。
- Rust 1.98.1 stable MSVC（rustup 安装，cargo 在 `%USERPROFILE%\.cargo\bin`，新会话需 `$env:Path += ";$env:USERPROFILE\.cargo\bin"`）。
- npm 使用 npmmirror 镜像源。
- bash 工具的实际 cwd 是会话目录，`workdir` 参数不可靠；进入项目目录统一用 `cmd /c "cd /d <path> && ..."`。
- `tauri dev` 启动方式：运行项目根目录的 `dev.cmd`（内含 `set PATH=%PATH%;%USERPROFILE%\.cargo\bin`）；background_service 的 command 直接传 `dev.cmd` 的完整路径，不要再套 `cmd /c`（插件内部已用 `cmd /d /s /c` 包裹，嵌套会秒退且日志为空）。
- 后台日志文件（out.log）可能长时间为空（重定向缓冲），验证运行状态看进程（cliplink.exe / msedgewebview2）和 1420 端口，不要只看日志。

## 关键设计决策

1. 骨架用 `create-tauri-app` vue-ts 模板生成（Tauri 2 + Vue 3.5 + Vite 8 + TS 6）。
2. 前端状态不用 Pinia，用 `src/stores/app.ts` 的 reactive 模块（不新增依赖，符合文档"轻量"原则）；文档要求的 `stores/app.ts` 路径保留。
3. `AppState`（src-tauri/src/state.rs）：`device_name` + `Mutex<Inner>`；`Inner` 含 zerotier_ip / status / status_text / peer / paused / last_sync。**无配对设计**：填 IP 直连，握手成功即 Connected（见阶段 5 记录"设计变更"）。
4. 统一错误类型 `AppError`（src-tauri/src/error.rs）：`#[serde(transparent)]` newtype(String)，前端收到字符串错误。
5. 日志：tracing + tracing-subscriber env-filter，默认 `info,cliplink_lib=debug`。
6. 前端命令入口：`get_app_snapshot`、`refresh_zerotier_ip`（阶段 3 真实检测）、`update_settings`、`get_device_identity_summary`（阶段 4）；其余命令按文档第 12 节在对应阶段添加。配置在 `%APPDATA%\com.localuser.cliplink\config.json`（Tauri 2 Windows 按 identifier 解析 app_data_dir）；secret 只在 Rust 侧，快照/事件只含非敏感字段。
7. 窗口 480×560 不可缩放，标题 ClipLink；identifier `com.localuser.cliplink`。
8. Rust 侧后续模块已建空壳文件（clipboard/config/identity/network/protocol/tray/zerotier.rs），文件内注释标注所属阶段。
9. 演示/预览机制（阶段 2 引入，仅开发环境）：`src/stores/demo.ts` 状态表 + `DevStateSwitcher`（`import.meta.env.DEV` 门控，内联在底部行）+ URL 参数 `?demo=<key>`、`&light=1`/`&dark=1`（`:root.light`/`:root.dark` 覆盖 CSS 变量）。生产构建不含任何演示逻辑，已用 vite preview 截图验证。
10. 截图工具链：本机 Edge 装在 `C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`（x86）；PowerShell `&` 无法启动 msedge（本机怪癖），统一用 `cmd /c` 调 bat；截图批处理模板在 `%TEMP%\opencode\shot-cliplink.bat`（headless=new + --window-size=480,560 + --virtual-time-budget=6000）。
11. PS 5.1 的 `Get-Content`/`Set-Content` 默认 ANSI 编码，处理含中文的 UTF-8 源文件会乱码——改源文件一律用 Edit/Write 工具，不要用 PS 文本 cmdlet 往返。
12. 阶段 3 环境陷阱：PS 5.1 的 `cmd /c "...set PATH=...&& cargo ..."` 单行形式下 cargo 外部命令查找会莫名失败（`where cargo` 能找到但直接调用报"不是内部或外部命令"），**cargo 命令一律写 .bat 再用 `cmd /c <bat>` 跑**（模板 `%TEMP%\opencode\cl-cargo-check.bat`：`set PATH=%PATH%;C:\Users\LocalUser\.cargo\bin` 后依次 cargo fmt/test/check）。
13. 本屏幕 125% DPI：PS 5.1 进程默认 DPI-unaware，`GetWindowRect`/`SetWindowPos`/`CopyFromScreen`/`SetCursorPos` 坐标互相矛盾。截窗口/点按钮前先在脚本里 `[void][DllImport("user32.dll")]SetProcessDPIAware()`，之后所有坐标均为物理像素且一致；截不到整窗时把窗口 `SetWindowPos` 移到已知坐标再截。
14. 本机（MSI 开发机）的 LLM API（4090 主机 NInfer）经 ZeroTier 网络（xu_network）路由；**验证期间不可停止 ZeroTier 服务/适配器**，否则本机会话中断。"未发现 IP"场景一律用单元测试覆盖。
15. **Tauri 状态注册类型陷阱（阶段 5 踩过）**：`app.manage(state)` 中 `state: Arc<AppState>` 时，注册的类型是 `Arc<AppState>`（不是 `AppState`）；所有查找必须写 `try_state::<Arc<AppState>>()` / 命令参数 `State<'_, Arc<AppState>>`，写成 `AppState` 会**永远返回 None 且不报错**（曾导致 45888 监听永不绑定、每轮 ZT 检测被静默跳过）。
16. `detect_and_notify` 用 `try_state`（AppState 未就绪时跳过本轮，不 panic）：首轮 ZT 轮询任务可能与 `app.manage` 存在调度竞态，`state()` 会 panic 并**杀死整个轮询任务**（此后 10 秒轮询全部失效）。
17. background_service 的 command 传**不带引号**的完整路径（路径无空格）；带双引号会被 cmd 当作命令名的一部分报"不是内部或外部命令"。
18. `NetworkManager`（network.rs）：`Arc<NetCore>` 克隆开销为引用计数；状态事件经 `StatusSink` 单一出口（生产 `TauriStatusSink`：写 `AppState.inner` + emit `connection-status-changed`；测试用收集型 sink）。**事件按连接代次（generation）过滤**：非终结事件仅在槽位仍属于该 generation 时发出；终结事件（finalize）先比对槽位、匹配才清理并发出，旧连接任务不会覆盖新连接状态（settled AtomicBool 保证每连接只终结一次）。
19. 心跳：`time::interval` 首个 tick 立即完成 → 握手完成后**立即发首个 ping**（属预期行为，对端回 pong 即可）；连续 3 次无匹配 pong 判 `heartbeat_timeout`；不匹配/过期 pong 不重置计数。
20. writer 任务是唯一写 socket 的任务（mpsc 通道串行化所有帧）；收到取消信号时**先排空已入队帧**（如用户 disconnect 消息）再关闭，保证用户主动断开的 disconnect 帧一定发出。

## 阶段 6 完成记录（2026-09-16）

- **剪贴板工作线程**：`clipboard.rs` 提供 `start(state: Arc<AppState>, app: AppHandle)`，挂到 tauri 全局 async runtime（随进程退出销毁，无需手动停止）。每 **300ms** `tokio::time::interval` 触发一轮。**剪贴板访问单线程**（arboard 实例每轮 `Clipboard::new()` 新建 + `get_text()` 读取，无需跨轮持有实例，避免剪贴板句柄长期占用）。
- **读取纯文字**：`arboard`（v3.6.1，Windows 后端 clipboard-win），仅 `get_text()` 文字接口——图片/文件等非文字内容返回 Err 自然被忽略。读取失败 `tracing::trace` 后跳过本轮（下轮自动重试）。
- **SHA-256 变化检测**：`sha2` 0.10 对文字字节计算摘要 → `identity::hex_encode` 转 64 位 hex。与 `AppState.inner.last_clipboard_hash` 比较：相同跳过（不重复触发）；不同则更新 hash 和 `last_clipboard_text`（供阶段 7 发送），`tracing::debug` 记录变化。
- **大小限制**：文字 `len() > 1 MiB`（`MAX_CLIPBOARD_BYTES=1024*1024`，与协议 MAX_MESSAGE_BYTES 一致）→ `tracing::warn` + emit `app-error` 事件（文案"剪贴板文字超过 1 MiB，本次未同步。"，与文档第 13 节一致）→ 跳过（不写入内部状态，不留作待发内容）。**恰好 1 MiB 不触发**。
- **暂停**：每轮先读 `inner.paused`（与配置 sync_paused/update_settings 同一字段），暂停时直接跳过（不读剪贴板），恢复后自然继续。
- **空字符串跳过**：`text.is_empty()` 不处理（文档"空字符串不发送"）。
- **前端接入**：`types/app.ts` 新增 `AppErrorEvent { message }`；`App.vue` 监听 `app-error` 事件（演示模式下忽略）→ `store.error`。暂无 UI 展示位置（阶段 7/9 决定如何展示）。
- **测试**：新增 3 个单元测试（62 单元 + 9 集成 = **71/71**）：哈希与 `sha2` 一致且 64 位 hex、不同输入不同哈希、大小边界（恰好 1 MiB 不触发 / 超 1 MiB 触发）。`cargo fmt --check`/`cargo check` 干净（无 dead-code 警告：`shutdown` 通道已按 ponytail 原则删除），`npm run build` 通过。
- **遗留**：
  - 真实 UI 无错误展示位（`app-error` 事件存到 `store.error` 但界面未渲染，阶段 7 决定是否加错误提示）。
  - 剪贴板真实读写验证（已完成一次，代码不留存）：独立 `clipboard_real.rs` 测试直接对系统剪贴板 `set_text` 后延时 400ms `get_text` 读回一致（模拟记事本/浏览器复制），跑完删除（长留会覆盖用户剪贴板/污染并行测试）；开发机 tauri dev 运行期间 `Set-Clipboard` 出现剪贴板竞争偶发报错（文字实际写入成功，`Get-Clipboard` 可读回）——反证 worker 确实在按 300ms 轮询系统剪贴板。4090 装 ClipLink 后的双机环节再顺带复核发送侧。
  - `last_clipboard_hash/text` 目前只在内存，应用重启后重置（阶段 7 不需要跨重启保留哈希）。
  - **git push 待重试**：阶段 6 提交 `2271036` 已完成提交，但 push 时手机热点连 GitHub 失败（443 慢/不可达），本地已提交未推送，网络恢复后 `git push origin main`。

## 阶段 5 完成记录（2026-09-16）

- **监听器规则**：只绑定"最近一次检测到的本机 ZeroTier IPv4 : 配置端口（45888）"，**不绑 0.0.0.0**。`NetworkManager::sync_listener(ip)` 幂等：`None` → 停止监听（IP 失效不再监听失效地址）；与上次相同 → 无操作（不重复启动）；IP 变化 → 取消旧监听任务、重绑新地址；绑定失败只 `tracing::warn`（记录地址+错误类型），不 panic、不影响已有连接。触发点复用阶段 3 的 `detect_and_notify`（结果变化时 spawn `sync_listener`），**无新增轮询器**。启动顺序：lib.rs setup 中先建 `NetworkManager` + `AppState`（含 net）+ `manage`，再注入 `zt_provider`（读 `AppState.inner.zerotier_ip` 的闭包，打破 manager↔AppState 循环依赖），最后 spawn ZT 轮询。
- **帧格式**：4 字节**大端**长度前缀 + UTF-8 JSON；单条上限 **1 MiB**。读取顺序：先读 4 字节 → 长度 0 → `protocol_invalid_length`；> 1 MiB → `protocol_message_too_large`（**先查长度再分配缓冲**，不对超长帧分配大内存）→ 读正文 → 非 UTF-8 → `protocol_invalid_utf8` → JSON 解析失败 → `protocol_invalid_json` → EOF → `connection_closed`。任一协议错误关闭该连接（Error 状态），进程不崩溃。
- **消息结构**（protocol.rs `Message`）：`{ version: 1, type, message_id(UUID), timestamp(毫秒), device_id, payload(Value), auth: Option<String> }`；`type` 经 `#[serde(rename="type")]`。类型：`hello`（payload: device_id/device_name/protocol_version）、`ping`（ping_id/sent_at）、`pong`（回显 ping_id/sent_at）、`disconnect`（reason）、`error`（reason）+ 未知类型安全忽略。**auth 恒为 null**（本版本不认证，字段保留以便将来扩展）。
- **hello 流程**：出站 = 先发送本机 hello 再等对方 hello（5 秒超时 → `handshake_timeout`）；入站 = 先读对方 hello 再回复本机 hello。校验：`protocol_version==1`（不匹配 → 回最小 error 帧后关闭，`protocol_version_mismatch`）+ `validate_hello`（device_id 为 UUID、device_name 1..=64 字符、**不等于本机 device_id**）。握手成功 → 直接进入 **Connected**（无配对环节，见下方"取消配对"变更）。同一连接中**重复 hello 明确忽略**（不计协议错误，避免对端重发导致误断开）。
- **心跳**：每 10 秒（`ManagerConfig::production`；测试 1 秒）发 ping（`time::interval` 首 tick 立即 → 握手后即刻发首个 ping）；收到匹配 pong 复位计数；连续 **3 次**无匹配 pong → `heartbeat_lost` → Error"连接已中断。"（error_code=`heartbeat_timeout`）。`HeartbeatState` 纯逻辑可单测。
- **断开流程**：
  - 用户主动（`disconnect_peer` 命令 / UI"断开"）：`user_requested` disconnect 帧（writer 排空保证发出）→ 取消心跳/读写任务 → Offline；文案按 ZT 状态："等待输入对方 IP。" / "未发现 ZeroTier IP。"；error_code=null；**不删除**配置里的 last_peer_ip。
  - 对方 disconnect 帧 → Offline"对方已断开连接。"。
  - 心跳失联 / 非预期 EOF → Error"连接已中断。"（`heartbeat_timeout` / `connection_closed`）。
  - TCP 连接失败/超时（5 秒）→ Error（`connect_failed` / `connect_timeout`）。
- **generation 机制**：每次 `claim_conn` 递增 `next_generation`；`Conn.settled`（AtomicBool）保证每连接只 finalize 一次；**事件按代次过滤**（见决策 18）：旧连接任务在终结前若已被新连接取代，其事件不再发出，旧连接不会覆盖新连接状态。
- **单连接冲突策略**：全局**最多一条活动/建立中连接**（`inner.conn` 槽位）。入站时已有连接 → 新连接立即 shutdown 关闭；出站时已有连接 → 命令返回 `already_connected`（"已有活动连接，请先断开当前连接。"）。连接目标校验（`validate_peer_target`，commands 层）：trim 后合法 IPv4（复用 zerotier::is_valid_ipv4，拒绝 0.0.0.0/回环/链路本地/广播/组播/保留段）+ 拒绝本机当前 ZT IP（`self_connection`"不能连接本机自己的 ZeroTier IP"）。
- **`connect_peer(ip)`**：校验 → **连接前保存** last_peer_ip（保存失败仅 `tracing::warn` 并继续连接，内存与磁盘均保持原值——与 update_settings 相同的"先落盘成功再更新内存"规则；注意 Mutex 不可重入：clone 后须先释放锁再落盘）→ `net.connect`。命令层错误为稳定中文文案（AppError 枚举序列化 `user_text()`，事件 error_code 为稳定 snake_code）。
- **事件结构**：`connection-status-changed` → `{ status, status_text, error_code: string|null, peer: {device_name, ip}|null, generation }`（非敏感，无 secret/auth/原始消息）。前端 App.vue 监听后直接更新 store（status/statusText/peerDeviceName/peerIp）；Rust 侧已按代次过滤，前端无需再判旧事件。演示模式下忽略事件。
- **前端接入**：store 新增 `connectPeer()`（invoke `connect_peer`，空输入前端拦截"请输入对方的 ZeroTier IP"，后端错误文案直接显示）/ `disconnectPeer()`；ConnectCard 连接按钮在 演示/connecting/connected/reconnecting/paused 时禁用；PeerStatusCard 标题仅 connected/paused 显示"已连接："否则"对方设备："，error 显示 statusText，暂停按钮仅 connected/paused 可用，空态 connecting 显示"正在连接对方……"；断开按钮（阶段 5 起可用）。
- **测试**：`cargo test` 共 **68/68 通过**（单元 59 + 集成 9）——取消配对后删除 1 个 `paired_peer_roundtrip` 单元测试。
  - 协议单元 16：全类型往返、大端前缀、分片读取（FragReader 模拟 TCP 分片）、超长长度不分配、0 长度、坏 UTF-8、坏 JSON、未知类型安全解码、>1MiB 拒绝编码、null auth 往返、hello 校验（版本/UUID/空名/超长名/本机 device_id）。
  - 心跳单元 3：pong 保持、不匹配/过期 pong 不重置、3 次失联。
  - 代次/槽位单元 2：claim 保护（AlreadyConnected）、旧代次 finalize 不影响新连接。
  - 目标校验单元 1：合法/非法/本机 IP/生产端口 45888。
  - 命令单元 4：输入校验（非法/回环/组播/本机 IP/提交后 AlreadyConnected/不可达地址有限时失败）、空闲断开为空操作、快照/身份摘要无 secret。
  - **集成 9**（`tests/tcp_loopback.rs`，127.0.0.1 + 随机端口，裸 TcpStream 模拟对端；测试身份为固定测试数据）：入站 hello/ping/pong 全链路（进入 Connected、收到对端 disconnect 退出）；出站连接 + hello + 用户主动断开（收到 disconnect 帧、回 Offline、槽位释放）；超长帧关闭；坏 JSON/坏 UTF-8 不崩溃；入站+出站无 hello 握手超时；对端关 socket 检测；已有连接时新入站被拒；出站连接被拒（connect_failed/timeout）；TEST-NET-1 不可达地址有限时失败。
  - `cargo fmt --check`、`cargo check` 无警告；`npm run build`（vue-tsc + vite）通过。
- **实际运行验证（tauri dev 真实运行）**：
  - 监听：`192.168.191.180:45888`（本机 ZT IP，**非 0.0.0.0**，netstat 确认 OwningProcess=cliplink.exe）。
  - **经 ZeroTier 的端到端 hello**：PowerShell TcpClient 连 `192.168.191.180:45888`（走 ZT 路由），发长度前缀 hello 帧 → 收到管理器 hello（type=hello、device_id=本机 6c0afbb1…、device_name=MSI、protocol_version=1）→ 发 disconnect → 再次连接复测通过（槽位正确释放，可重连）。
  - **真实踩坑（已修复）**：`app.manage(Arc<AppState>)` 后所有 `try_state::<AppState>` 返回 None（类型实为 `Arc<AppState>`）→ 每轮 ZT 检测静默跳过、45888 永不绑定；已统一查找类型为 `Arc<AppState>`（命令参数/检测/状态 sink 三处）。此坑在单测中不可见（测试不走 Tauri manage），必须实跑验证。
  - dev 服务日志（out/err.log）在本机持续为空（重定向缓冲），验证以进程 + 端口 + 独立 `cliplink.exe` 重定向 stdout 的日志为准（日志仅含 目录/device_id/device_name/ZT IP/监听地址/连接事件，无 secret/64 位 hex/配置正文）。
  - 验证后已停服务：无 cliplink/vite/cargo/rustc 残留进程，1420 与 45888 端口均已释放。
- **遗留**：
  - 两机（本机 + 4090）真实互连未做——4090 上尚无 ClipLink；本机自环回 ZT 路径已验证（监听+握手+重连），两机互连留待 4090 装上本应用后在阶段 6/7 顺带验证。
  - UI 视觉验收（新状态文案/按钮禁用态/断开弹窗）留独立视觉验收会话（本会话只做功能与协议验证）。
  - 正式配置 `last_peer_ip=10.147.17.99`（阶段 4 UI 测试值，未变）。
  - `docs/开发文档.md` 未改（协议/状态/文案与文档第 8/11/12/15 节一致；阶段 5 无需求变更）。

### 设计变更（2026-09-16，用户决策）：取消配对

- **决策**：填对方 ZeroTier IP 直接连接，握手（hello 交换）成功后即进入 **Connected**，不再有配对/认证/共享密钥环节。理由：用户明确"填 IP 就能连就够了"；安全模型见 docs 第 9 节（信任 ZeroTier 网络边界，本版本不做消息认证）。
- **代码影响**：删除 `AwaitingPairing` 状态与 `pairing_code`（state.rs）、`PairedPeer` 结构与 `SHARED_KEY_LEN`（config.rs，含 3 处测试）、`paired_peer` 配置字段及其校验、hello 中认证逻辑切点、前端 `PairingDialog.vue`（文件删除）/awaiting_pairing 状态/`pairingCode` 字段/相关 demo 数据、"连接并配对"按钮文案（改"连接"）。协议 `auth` 字段保留但恒为 null。
- **兼容**：已存 `config.json` 若含 `paired_peer` 字段会被 serde 忽略（未知字段策略），无需迁移。
- **阶段表调整**：原阶段 6（首次配对）/7（后续认证）取消；阶段 6=单机剪贴板监听、7=双向同步，余下顺延（见上表）。
- **测试**：`cargo test` 68/68 通过（比取消配对前少 1 个配对往返测试）、`npm run build` 通过；`cargo fmt --check`/`cargo check` 干净。

## 阶段 4 完成记录（2026-09-16）

- **应用数据目录（本机实测解析值）**：`C:\Users\LocalUser\AppData\Roaming\com.localuser.cliplink`（Tauri 2 在 Windows 上按 **identifier** `com.localuser.cliplink` 解析，不是 productName；代码经 `app.path().app_data_dir()` 获取，不硬编码）。启动时 `create_dir_all` 自动创建。
- **配置文件**：`config.json`，`schema_version: 1`。未知字段策略：**serde 默认忽略**（低版本读高版本新增字段不报错）；缺失的非关键字段按 serde 默认值补齐（listen_port 缺省回 45888，其余回 false/null）。
- **数据结构**：`LocalIdentity { device_id, device_name, device_secret }`（identity.rs）；`AppConfig { schema_version, listen_port=45888, autostart=false, sync_paused=false, last_peer_ip=null, paired_peer=null, identity }`；`PairedPeer { device_id, device_name, ip, shared_key }` 为**预留结构，恒为 null 直到阶段 6**，本阶段不生成 shared_key。
- **身份生成**：device_id = UUID v4（`uuid` crate v4，内部 getrandom）；device_secret = `getrandom`（Windows 上为系统安全随机源 BCrypt/CryptGenRandom）32 字节，**小写十六进制（64 字符）持久化**；device_name = `hostname::get()`（Windows 计算机名），trim → 超 64 字符截断 → 空则回退 `Windows-PC`。不用时间戳/伪随机/MAC/ZT IP。
- **原子保存**：同目录 `config.json.tmp` 写入 → `write_all` + `sync_all` → Windows 用 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 替换（std::fs::rename 在 Windows 目标已存在会报错，**MoveFileExW 非零返回才是成功**，已踩过）；其他平台 rename。失败清理临时文件并经 AppError 返回，不 panic。替换步骤对 `PermissionDenied` 做 5 次×10ms 重试（Windows Defender 会瞬时占用新临时文件，真实 Windows 上高频保存会触发）。
- **损坏恢复**：JSON 解析失败或关键字段语义校验失败（端口 0、非法 IPv4、device_id 非 UUID、device_secret 非 32 字节 hex、device_name 空）→ 原文件**先备份**为 `config.json.bak-<unix秒>`（冲突加 `-N` 计数，永不覆盖旧备份）→ 生成新默认配置+新身份原子保存 → `tracing::warn` 只记原因与备份路径，**不含配置正文和密钥**（serde 解析错误只报行列位置）。原文件绝不静默删除。
- **并发保存**：`ConfigStore.save_lock`（专用 std Mutex）串行化完整"写临时文件+替换"流程，防止并发踩踏同一临时文件；磁盘 IO 在 AppState 的 `config`/`inner` 锁外，持锁期间无 await。
- **AppState**：`{ store: Arc<ConfigStore>, config: Mutex<AppConfig>, inner: Mutex<Inner> }`；`Inner` 新增 `last_peer_ip`（配置回显）。完整配置（含 secret）只在 Rust 侧；`AppState::new` 从配置初始化 `paused`/`last_peer_ip`/`peer`（paired_peer 的非敏感摘要）。lib.rs setup 中**先加载配置并 manage(AppState)，再 spawn ZeroTier 轮询**；配置初始化失败则 setup 返回 Err 退出（可诊断，不带着未初始化身份运行）。
- **commands**：`get_app_snapshot`（新增 device_id/device_name/listen_port/autostart/last_peer_ip，peer 摘要不含 shared_key）；`update_settings`（入参 camelCase：autostart/syncPaused/lastPeerIp；lastPeerIp 空串=清除、不传=不改、否则须为有效 IPv4；**先持久化成功后再更新内存**，保存失败内存不变）；`get_device_identity_summary`（仅 device_id+device_name）。无"读完整配置/导出密钥"命令。
- **前端接入**：store 新增 deviceId/listenPort/autostart/lastPeerIp/initialized；首次快照把 last_peer_ip 回填输入框；`savePeerIp()` 在**失焦/回车**时触发（不按每次按键写盘），非法 IPv4 前端先拦截并在卡片内提示；开机启动复选框接入 autostart 持久化（仅存值，系统开机启动留阶段 11，演示模式禁用）；StatusBanner 副标题追加 "· <设备名>"（ellipsis 防溢出，不破坏 480×560 布局）。演示预览器不变（仍仅 dev）。
- **测试**：33/33 通过（identity 6 + config 12 + commands 3 + zerotier 12），覆盖任务清单全部 23 项（损坏备份/恢复、无效 UUID/secret、临时文件写入失败不破坏原配置、缺失字段兼容、paired_peer 往返（测试密钥）、8 线程×5 并发保存、敏感字段不出现在快照序列化等）。测试全部用临时目录，不触碰真实用户配置。
- **构建**：`cargo fmt --check`、`cargo check`（仅 2 个预期 dead-code 警告：`ConnectionStatus` 未用变体与 `pairing_code`，阶段 5/6 使用）、`cargo test`、`npm run build`（vue-tsc+vite）全部通过。
- **实际运行验证（tauri dev 真实运行，多次）**：
  - 首次启动在 `%APPDATA%\com.localuser.cliplink` 创建 `config.json`，device_id `752bdc4b…`（后缩略），device_name=MSI（与 Windows 计算机名一致），listen_port=45888，secret 64 位 hex。
  - 界面 + ZeroTier 检测正常（192.168.191.180），视觉验收子会话 3 张截图全部 PASS（`docs/screenshots/stage4-first-run.png` / `stage4-second-run.png` / `stage4-run3.png` / `stage4-run3-typed.png`）。
  - 重启后 device_id 不变（`6c0afbb1…` 在后续多次启动间稳定，含独立 `cliplink.exe` 运行）。
  - **配置修改持久化**：停应用后直接改 `last_peer_ip=192.168.191.200`（Edit 工具，无 BOM）→ 重启后界面输入框正确回显；再经**真实 UI** 点击输入框 → Ctrl+A → 输入 `10.147.17.99` → Tab 失焦 → 磁盘 config.json 的 last_peer_ip 变为 `10.147.17.99`（update_settings 全链路验证）。
  - **损坏恢复真实触发**：验证中曾用 PS 5.1 `Set-Content -Encoding UTF8`（带 BOM）改配置，应用启动正确识别为损坏 → 备份 `config.json.bak-1789540413`（含 BOM 原文与原身份）→ 生成新配置新身份，未崩溃。
  - **日志秘密检查**：`target\debug\cliplink.exe` 直接重定向 stdout 捕获（background_service 的 out.log 在本机持续为空，见决策 34）：日志仅含 目录/device_id/device_name/ZeroTier IP 与事件，**无 device_secret、shared_key、64 位 hex、无完整配置正文**（正则 `[0-9a-f]{64}` 扫描为 False）。前端无任何 console 打印（源码 grep 确认）。
  - WebView2 偶发 "Error launching CrashSender.exe" 崩溃弹窗（第二次运行出现，重启即消失，本机 WebView2 运行时怪癖，与应用代码无关；三次独立视觉验收均确认 UI 功能正常）。
  - 验证后已停服务：无 cliplink/vite/cargo/rustc 残留进程，1420 端口已释放。
- **遗留**：正式配置当前 `last_peer_ip=10.147.17.99`（UI 测试值，阶段 5 连接测试会用到/覆盖）；目录中保留 1 个 `.bak`（BOM 损坏备份，可删）。

## 阶段 3 完成记录（2026-09-16）

- Windows API：Win32 `GetAdaptersAddresses`（winapi 0.3.9，features `iphlpapi`+`iptypes`+`winsock2`；tokio 仅 `time` feature 供后台 interval）。winapi 0.3.9 无 `_LUID` 结构体，`IP_ADAPTER_ADDRESSES` 是 `IP_ADAPTER_ADDRESSES_LH` 的别名（`um::iptypes`）；IPv4 从 `SOCKADDR.sa_data[2..6]`（`[CHAR;14]`，网络字节序）读取，`sa_family==2` 判 AF_INET。
- 网卡识别：`Description` 或 `FriendlyName` 含 "zerotier"（不区分大小写）即 ZeroTier 网卡；本机实际描述 "ZeroTier Virtual Port"、显示名 "ZeroTier One"。不按网段判断，故 Wi-Fi/以太网/其他 VPN 不会误判。
- IPv4 过滤：排除 `0.0.0.0`、回环 `127/8`、链路本地 `169.254/16`、广播 `255.255.255.255`、组播 `224/4`、保留段 `240/4`。
- 多地址选择规则：**取所有 ZeroTier 网卡有效 IPv4 中数值最小者**（去重后升序取首）——多次轮询结果确定，不会在候选间跳变；单 ZT 网络（本机实际场景）下即唯一地址。
- 结果类型：`ZtResult { Found, NoAdapter, NoValidIpv4 }`（serde snake_case 字符串）。`refresh_zerotier_ip` 命令返回它；只有 `GetAdaptersAddresses` 真正失败才返回 `AppError`。ZeroTier 未启动/未入网/未授权在 GetAdaptersAddresses 层面无法可靠区分，统一归入 NoAdapter/NoValidIpv4（内部 ZtResult 保留差异，日志可诊断）。
- AppState：`Inner` 新增 `zerotier_hint`/`hint_warn`（初始为"正在检测 ZeroTier 网络……"灰态）。`detect_and_notify` 在锁外做 Win32 调用，仅比对/更新时短暂持锁，事件在锁外发送；未检测到有效地址时清空旧 IP；仅结果变化时更新并 `emit("zerotier-ip-changed", { ip })`（payload 类型 `ZtIpEvent` ↔ 前端 `ZtIpChangedEvent`）。
- 后台轮询：`lib.rs` setup 中 `tauri::async_runtime::spawn` + `tokio::time::interval(10s)`，首次 tick 立即执行（启动即检测）。任务随 Tauri 全局 async runtime 生存，进程退出即销毁；未来窗口隐藏到托盘（阶段 11）时进程不退出、轮询继续，符合"后台保持检测"语义，无需显式取消。
- 前端：`App.vue` onMounted 注册 `listen("zerotier-ip-changed")`（演示模式下 refreshSnapshot 自动跳过，模拟状态不影响生产检测）；`LocalAddressCard` 新增"刷新"按钮（演示模式禁用），点击调 `refresh_zerotier_ip` 后拉快照，API 失败时卡片提示"ZeroTier 检测失败：<原因>"（红色，不破坏其余界面状态）；复制按钮复制 `store.zerotierIp` 真实地址。
- 单元测试：12/12 通过（正常地址/无网卡/有网卡无 IPv4/仅 169.254/仅回环无效/多网卡并存/大小写匹配/多候选稳定/旧→新/有效→无/重复不变/字节序）。`cargo fmt --check`、`cargo check` 通过（仅 2 个预期 dead-code 警告：`ConnectionStatus` 未用变体与 `pairing_code` 字段，阶段 5/6 使用）；`npm run build`（vue-tsc+vite）通过。
- 实际验证（tauri dev 真实运行）：
  - 界面显示 `192.168.191.180` + "ZeroTier 已连接"（灰）+ "等待输入对方 IP。"，截图 `docs/screenshots/real-connected.png`。
  - 交叉验证：WMI（ZeroTier Virtual Port → 192.168.191.180）、`zerotier-cli -j listnetworks`（xu_network → 192.168.191.180/24）、应用显示，三方一致。
  - 后台 10 秒轮询：控制台可见每 10 秒一条 `DEBUG cliplink_lib` 复查日志（result=Found）。
  - 手动刷新：点击"刷新"按钮成功（按钮聚焦环，检测完成恢复），命令链路与后台轮询共用 `detect_and_notify`。
  - 复制按钮：点击后系统剪贴板内容 = `192.168.191.180`。
  - demo 预览：`?demo=no_zerotier&light=1` 页面正常（截图 `docs/screenshots/demo-no_zerotier.png`），开发切换器仅 dev 出现。
  - "未发现 IP"真实场景未做：本机 LLM API 经 ZT 路由，断 ZT 会中断本会话；由单测 `no_zt_adapter`/`zt_adapter_without_ipv4`/`zt_only_link_local` 覆盖。
  - 验证后已停服务：无 cliplink/cargo 残留进程，1420 端口已释放。

## 阶段 1 完成记录（2026-09-16）

- 骨架：create-tauri-app vue-ts 模板（Tauri 2.11.5 / Vue 3.5 / Vite 8.3 / TS 6）。
- 验证结果：`npm run build`（vue-tsc + vite）通过；`cargo check` 通过（仅预期 dead-code 警告）；`tauri dev` 实际启动，cliplink.exe + WebView2 窗口运行，vite 1420 正常。
- 前端 `get_app_snapshot` 调用链已通（onMounted 触发 invoke）；界面实际渲染效果待独立视觉验收会话确认。
- 应用窗口 480×560 不可缩放，标题 ClipLink，identifier `com.localuser.cliplink`。

## 阶段 2 完成记录（2026-09-16）

- 演示状态：新增 `src/stores/demo.ts`，9 个模拟状态（detecting / no_zerotier / waiting_input / connecting / awaiting_pairing / connected / paused / reconnecting / error），状态文字与开发文档第 3 节"必须提供的状态文字"逐字一致，覆盖任务要求 7 类。
- 状态切换器：`src/components/DevStateSwitcher.vue`，仅在 `import.meta.env.DEV` 为真时渲染（内联在底部一行，位于"开机启动"与"打开设置"之间，不再用 fixed 定位遮挡）；支持 URL 参数 `?demo=<key>`（可叠加 `&light=1` / `&dark=1` 强制主题）。生产构建（`import.meta.env.DEV=false`）完全不包含该组件与演示逻辑。
- 主题：浅色为默认，`prefers-color-scheme` 自动深色；`?dark=1`/`?light=1` 通过 `:root.dark`/`:root.light` 覆盖变量，仅开发预览用。
- 布局：全局 `html/body` `overflow:hidden`、`.layout` `overflow:hidden`，卡片 `flex-shrink:0`，输入/名称/长状态文字统一省略号或换行，480×560 内无重叠、无横向滚动。
- 键盘焦点：`:focus-visible` 绿色描边（仅键盘导航触发）；禁用态 `opacity + --hover 背景 + --muted 文字`；错误态红点红字。
- 设备状态卡边框色随状态：connected 绿 / connecting·awaiting·reconnecting 黄 / paused 灰 / error 红（修复"连接中却显示绿框"）。
- 提示色：检测中为灰色提示，仅"未发现 ZeroTier"为红色警告（`hintWarn` 字段，真实模式由 `loaded && !zerotierIp` 判定）。
- 视觉验收：12 张截图存于 `docs/screenshots/`（light-×9 + dark-connected/awaiting_pairing/error 共 12 张，外加 prod-check）。全部经独立视觉验收子会话逐图读取核对，12/12 PASS；prod-check 确认生产构建底部无"状态预览"开发按钮。
- 构建：`npm run build`（vue-tsc + vite）通过。

### 视觉验收截图清单（docs/screenshots/）
- light-detecting / light-no_zerotier / light-waiting_input / light-connecting / light-awaiting_pairing / light-connected / light-paused / light-reconnecting / light-error
- dark-connected / dark-awaiting_pairing / dark-error
- prod-check（生产构建，验证无开发按钮）

## 已知问题 / 待办

- 开机启动复选框已持久化偏好值（阶段 4）；系统开机启动插件与"打开设置"按钮留阶段 9。
- 图标仍为模板 tauri.svg（阶段 10 换正式图标并生成安装包图标）。
- 两机真实互连（本机 + 4090）待 4090 装 ClipLink 后验证（见阶段 5 遗留）。
- "未发现 ZeroTier IP"的真实运行场景未实测（本机 LLM API 依赖 ZT，见决策 14）；由单测覆盖，后续可在 4090 主机（不依赖 ZT 跑 LLM）上补测。

## 下一步（阶段 6：首次配对）

1. 双方 hello 交换后生成配对确认码（基于双方 device_secret 的共享材料，见开发文档第 11 节算法），UI 显示六位确认码（`pairing_code` 进入 AppSnapshot/事件）。
2. `approve_pairing` / `reject_pairing` 命令：确认后保存 `paired_peer { device_id, device_name, ip, shared_key }`（shared_key 由双方 secret 派生，仅存 Rust 侧配置），进入 Connected；拒绝则断开并清理。
3. PairingDialog 启用"允许并记住/拒绝"按钮；确认码不一致时给出明确提示。
4. 配对后重连走阶段 7 的认证（shared_key + auth 字段），阶段 6 只负责"首次"。
5. 完成标准：两机首次配对成功（确认码一致→Connected）、拒绝后回到未配对状态、配对信息持久化且 secret 不泄漏。