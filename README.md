# Term

为 AI 编码 agent（Claude Code / opencode）定制的 GPU 终端。主打 **SSH + tmux 多标签并行 · 完成通知 · 会话监控**，用 winit/wgpu/glyphon 自渲染。

> 在终端里 `ssh → tmux → Claude Code`：全屏 UI 不闪、流式输出顺滑、复制能落到本地剪贴板、跑完弹桌面通知、标签实时显示忙/闲/失败。

---

## 下载

### macOS（Apple Silicon / M 系列）

直接下预编译二进制，无需装 Rust：

1. 下载 **[term-v0.4.0-macos-arm64](https://github.com/SiriusEst/term/releases/download/v0.4.0/term-v0.4.0-macos-arm64)**（或到 [Releases](https://github.com/SiriusEst/term/releases) 页面）。
2. 赋予执行权限：
   ```bash
   chmod +x term-v0.4.0-macos-arm64
   ```
3. 首次运行被 Gatekeeper 拦截时，去掉隔离属性（或在「系统设置 → 隐私与安全性」里放行）：
   ```bash
   xattr -d com.apple.quarantine term-v0.4.0-macos-arm64
   ```
4. 运行：
   ```bash
   ./term-v0.4.0-macos-arm64            # 打开本地 shell
   ./term-v0.4.0-macos-arm64 myhost     # ssh 到 ~/.ssh/config 里的别名 myhost
   ```

### 其它平台（Intel mac / Linux / Windows）

暂无预编译二进制，请[从源码编译](#从源码编译)。

---

## 从源码编译

需要本机装好 Rust（<https://rustup.rs>），然后：

```bash
git clone https://github.com/SiriusEst/term.git
cd term
cargo build --release          # 产物在 target/release/term
./target/release/term
```

开发时也可以直接 `cargo run`，但**建议加 `--release`**：debug 构建下字形排版慢，打字会有明显延迟。

---

## 使用

```bash
term                            # 本地 shell（开一个 GPU 窗口）
term myhost                     # ssh 到 ~/.ssh/config 里的别名 myhost
term user@host.example.com      # 或直接 user@host
```

- **退出**：在 shell 里 `exit` / `Ctrl-D`，或直接关窗口。
- **开多个标签**：`Cmd+T`。SSH 到同一主机时，第二个标签靠 ControlMaster 复用连接，秒开免认证。
- **远端在 tmux 里跑 Claude Code** 时，想让剪贴板/通知穿透，远端 `tmux` 需要：
  ```tmux
  set -g allow-passthrough on
  set -g set-clipboard on
  ```

---

## 快捷键

| 操作 | macOS | Linux |
|---|---|---|
| 复制选区 | `Cmd+C` | `Ctrl+Shift+C` |
| 粘贴 | `Cmd+V` | `Ctrl+Shift+V` |
| 新标签 | `Cmd+T` | `Ctrl+Shift+T` |
| 关标签 | `Cmd+W` | `Ctrl+Shift+W` |
| 跳到第 n 个标签 | `Cmd+1..9` | `Ctrl+Shift+1..9` |
| 上 / 下一个标签 | `Cmd+[` / `Cmd+]` | `Ctrl+Shift+[` / `]` |
| 回看历史 | `Shift+PgUp` / `PgDn`，或鼠标滚轮 | 同左 |
| 选择文本 | 鼠标拖选（应用要鼠标时按 `Shift` 强制本地选择） | 同左 |
| 打开超链接 | 点击 OSC 8 链接 | 同左 |

---

## 配置

可选配置文件 `~/.config/term/config.toml`。文件缺失或字段缺省都会回退内置默认，解析出错也不会让程序起不来。

```toml
font_size = 16.0       # 逻辑像素字号（未乘 HiDPI 缩放），默认 15
scrollback = 10000     # 主屏回看行数，默认 5000

[theme]
fg = "#cdd6f4"
bg = "#1e1e2e"
cursor = "#f5e0dc"
selection = "#45475a"
```

---

## 主要功能

- **渲染 / 颜色**：truecolor 24-bit + 256 + 16 色，bold / dim / italic / underline / inverse / strike，块状光标、选区高亮、OSC 8 超链接。
- **完美 Unicode**：CJK / emoji 按显示宽度占两列，组合字符占零列。
- **全屏应用**：备用屏（vim / htop / tmux / Claude Code）、同步输出（流式 token 不闪烁）。
- **复制粘贴**：系统剪贴板 + 括号粘贴；**OSC 52** 让远端（含 tmux 里的 Claude Code）把内容写进你的本地剪贴板。
- **多标签**：每标签独立 PTY / 解析 / 网格；SSH 同主机标签复用连接。
- **完成通知**：`BEL` / `OSC 9` / `OSC 777` → 桌面通知，仅在失焦或非当前标签时弹。
- **会话监控**：`OSC 133` 标记命令开始/结束 → 标签上的 运行中 / 空闲 / 失败 状态点。
- **回看**：scrollback，鼠标滚轮 / `Shift+PgUp`/`PgDn`。

### 搭配 zsh 插件

`zsh-autosuggestions` / `zsh-syntax-highlighting` 是 shell 插件（靠标准 ANSI 序列工作），本终端已能完美渲染（dim 建议、语法高亮、就地重绘不卡，右方向键/End 接受建议）：

```bash
# Oh My Zsh 用户：
git clone https://github.com/zsh-users/zsh-autosuggestions   ${ZSH_CUSTOM:-~/.oh-my-zsh/custom}/plugins/zsh-autosuggestions
git clone https://github.com/zsh-users/zsh-syntax-highlighting ${ZSH_CUSTOM:-~/.oh-my-zsh/custom}/plugins/zsh-syntax-highlighting
# 在 ~/.zshrc 的 plugins=(...) 里加这两个；syntax-highlighting 必须放最后。
```

---

## 实现与设计

依赖栈：winit 0.30 / wgpu 29 / glyphon 0.11 / cosmic-text 0.18 / vte 0.15。完整设计、VT 序列覆盖、依赖版本迁移说明、SSH 多窗口两条路线（ControlMaster vs russh）见：

- 设计文档：[`terminal-emulator-design.md`](./terminal-emulator-design.md)

代码结构：

```
ConnectionManager   src/manager.rs    顶层；tab 条 = 所有连接的窗口扁平列表
└─ Connection       src/connection.rs 一个 SSH 目标；同目标多窗口复用连接（ControlMaster）
   └─ Window        src/window.rs     PTY + writer + 独立 Parser + 独立 Grid + 监控状态
      ├─ PtyProcess src/pty.rs        PTY 封装（reader / writer / resize）
      ├─ Grid       src/grid.rs       网格 + 光标 + 模式 + 滚动区域 + 备用屏 + scrollback
      └─ Parser     src/parser.rs     vte::Perform → 把动作落到 Grid

App / 事件循环      src/app.rs        winit：多窗口路由 / 键鼠 / 复制粘贴 / 标签 / 通知 / 重绘
键盘编码            src/input.rs      winit 按键 → VT 字节
渲染                src/render.rs     wgpu 交换链 + glyphon 文字 + 矩形管线
配置                src/config.rs     ~/.config/term/config.toml
```

---

## License

双授权：**MIT OR Apache-2.0**（Rust 生态惯例），使用者二选一。
见 [`LICENSE-MIT`](./LICENSE-MIT) 与 [`LICENSE-APACHE`](./LICENSE-APACHE)。
