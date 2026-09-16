# ClipLink

通过 ZeroTier 在两台 Windows 电脑之间双向同步纯文字剪贴板的托盘工具。

- 需求与技术方案：`docs/开发文档.md`
- 进度接力：`PROGRESS.md`
- 技术栈：Tauri 2 + Vue 3 + TypeScript + Rust（Tokio）

## 开发

```powershell
npm install
npm run tauri dev
```

检查：

```powershell
npm run build          # 前端类型检查 + 构建
cargo check            # 在 src-tauri 目录执行
```