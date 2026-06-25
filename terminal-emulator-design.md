# 自制终端模拟器 · 设计文档（Rust）

> **目标场景**：为 **Claude Code / opencode** 等 AI 编码 agent 定制的终端。
> SSH 后**多窗口并行**；对 agent 的**高频 token 输出兼容性好**；常用**基本指令 + tmux**；带**会话监控**；agent **执行完毕有提示**。
> 技术栈：Rust，目标平台 mac + Linux。

## 0. 一句话定义

你要写的是**终端模拟器**，不是 shell。它启动 shell / ssh（通过 PTY），把对端输出的**字节流**解析成屏幕上的**字符网格**并渲染，同时把键鼠输入**编码成字节**回写。它不执行命令，只负责「显示 + 输入桥接 + 会话管理」。

---

## 1. 目标场景 → 技术需求（本项目的核心）

| 你的场景 | 推导出的技术需求 | 影响 |
|---|---|---|
| 跑 Claude Code / opencode（全屏 TUI） | 备用屏(1049)、24-bit truecolor、**同步输出(DEC 2026)**、OSC 8/52、kitty 键盘协议、Unicode/Nerd Font | 解析层+渲染层必须**很稳** |
| 对 token 流式输出兼容性好 | **同步输出**去闪烁 + 高吞吐解析（不丢字节/不卡）+ 脏行重绘 | 性能 + 2026 提前做 |
| SSH 后多窗口并行 | 多 PTY 会话 + **tab/窗口管理**；每会话独立 Grid/Parser | 架构从一开始按多会话设计 |
| 常用 tmux | 做好 **tmux 客户端兼容**：鼠标(1006)、focus 上报(1004)、**DCS passthrough** | 见 §6 |
| 要监控 | 每 tab 的 busy/idle/活动状态 + 概览面板 | 用 OSC 133 / 活动检测，见 §5 |
| 执行完毕有提示 | BEL / OSC 9 / OSC 777 → **原生桌面通知 + tab 角标** | 见 §4 |

**一句话的范围决策**：**分屏/pane 交给 tmux，别自己重造**。你的终端只做 ① tab（跨 ssh 主机的多会话）② 极致兼容 ③ 监控 + 通知。这能省掉一大块工作量。

---

## 2. 核心架构（多会话版）

`ConnectionManager` 持有 `Vec<Connection>`；每个 **Connection**（一个 SSH 目标）持有一条**共享 SSH 传输** + `Vec<Window>`；每个 **Window** = 一条 PTY/channel + 独立 `Parser` + 独立 `Grid/State` + 自己的监控/通知状态。UI **左侧边栏 = `Connection(host) → Windows` 的树**，host 下的「+」在同一连接里开新窗口（**不重新认证**，详见 §7）。

单会话内的数据流闭环（详见对话里的架构图）：

- **输出**：PTY 读 → `vte` 解析 → 更新网格/状态 → 渲染器画到窗口
- **输入**：`winit` 键鼠 → 编码成 VT 字节 → 写回 PTY
- **控制**：resize → `ioctl(TIOCSWINSZ)` + `SIGWINCH`

你必须自己实现：①PTY 层 ②解析层（`vte::Parser` + 你的 `Perform`，把动作落到状态）③状态模型（cell 网格 + 光标 + scrollback + 备用屏 + 滚动区域）④渲染层（字体 shaping/光栅化 + GPU glyph atlas + 脏行重绘）。

---

## 3. 兼容性清单（为 Claude Code / opencode / tmux，必须支持）

- **颜色**：SGR 16 / 256 / **truecolor 24-bit**
- **备用屏**：DECSET 1049（vim/htop/tmux/agent 全屏都靠它）
- **同步输出**：DECSET **2026**（关键——流式 token 不闪烁，现代 TUI 标配）
- **bracketed paste**（2004）、**focus 上报**（1004，tmux/vim 要）
- **鼠标**：SGR 1006（+1000/1002/1003）
- **OSC**：8 超链接、**52 剪贴板（含 DCS passthrough 包装）**、7 cwd、**133 提示符标记（A/B/C/D+exit）**
- **通知**：BEL、OSC 9、OSC 777
- **键盘**：kitty keyboard protocol（CSI u）
- **Unicode**：CJK/emoji/ZWJ 宽度、Nerd Font / powerline 字形、字体回退

> OSC 133 是关键复用点：它既给你**「命令何时开始/结束 + 退出码」**（→ 完成提示），又给你**每个 tab 的忙/闲状态**（→ 监控）。

---

## 4. 完成提示（通知）设计 — 已核对可行

- **Claude Code 侧**：有 **Stop hook**，每次回应结束都会触发，可执行任意命令。v2.1.141+ 的 hook 支持 `terminalSequence` 字段，让 Claude Code 直接吐转义序列（BEL/OSC/设标题）。**最省事**：Stop hook 吐一个 BEL（`\a`）或 OSC 9。
- **你的终端侧**：把 `BEL / OSC 9 / OSC 777` →
  - 原生桌面通知（mac 用 `NSUserNotification`，Linux 用 `notify-rust`/D-Bus），
  - 仅在**窗口失焦**或**非当前 tab**时弹（靠 focus 上报判断），
  - 给对应 tab 加**角标/高亮**。
- **opencode**：同为 TUI，用 bell-on-done；没有钩子时用**静默检测**（某 tab 由"有输出"转为"N 秒无输出"）兜底。
- **跨 tmux+ssh**：BEL 最稳（tmux 一般直接转发）；OSC 9/777 可能需 `allow-passthrough`。所以**首选 BEL** 做完成提示。

---

## 5. 监控设计

- **每会话状态机**：优先用 **OSC 133** 的 `C`(命令开始)/`D;exit`(结束+退出码) → `运行中 / 空闲 / 失败`；对端没装 shell 集成时，用**活动/静默启发式**（类似 tmux 的 monitor-activity / monitor-silence）兜底。
- **UI**：tab 上的状态点（运行中=转圈、完成=绿、失败=红、有新输出=高亮）；可选一个**概览面板**列出所有会话（host、当前命令、状态、最后活动时间）。
- 这套状态判定与 §4 的"完成提示"共用同一信号源。

---

## 6. SSH + tmux 透传（关键易踩坑，已核对）

剪贴板/超链接/通知要穿过 `远端 app → tmux → ssh → 你的终端` 这条链：

- **远端 tmux 配置**：`set -g allow-passthrough on`、`set -g set-clipboard on`。tmux 3.3+ **默认关闭** DCS passthrough。
- **机制**：现代工具（Claude Code/opencode）把 OSC 52 包在 **DCS passthrough** 里，绕过 tmux 自己的剪贴板处理 → 你的终端**必须能解 DCS passthrough + OSC 52**，且 tmux 要开 `allow-passthrough`，两者缺一不可。
- **已知痛点**（把这条链做稳 = 你的差异化卖点）：Claude Code `#38944`（/copy 在 tmux 里间歇失败，应走 DCS passthrough）、opencode `#19982`（allow-passthrough 关时 OSC 52 失败）。

---

## 7. 同一 SSH 目标下的多窗口（侧边栏 / 连接复用）

你要的「侧边栏多窗口」= **一个 SSH 连接，底下挂多个 shell 窗口**，侧边栏按主机分组（Termius 模式）。

**数据模型**：

```
ConnectionManager
└─ Connection(gadi.nci.org.au)        ← 一条共享 SSH 传输
   ├─ Window 1  (PTY/channel + Parser + Grid + 监控/通知)
   ├─ Window 2
   └─ [+] 新开窗口 → 复用连接，免再次认证
```

侧边栏就是这棵树；点 host 下的「+」在同一连接里开新 shell。

**两条实现路线**：

*路线 A — OpenSSH ControlMaster（先做，最省力）*
- 每个窗口用 `portable-pty` spawn：
  `ssh -tt -o ControlMaster=auto -o ControlPath=~/.ssh/cm-%C -o ControlPersist=10m <host>`
- 第一个窗口认证（含 MFA/OTP，如有）；之后**同 host 的新窗口复用 master、秒开、免再认证**。
- 白嫖系统 ssh 的密钥 / agent / known_hosts / ProxyJump，零重复造轮子。
- Rust 里可用 `openssh` crate 管理 master + 开 session，或直接 spawn ssh。
- 代价：依赖系统 ssh；master 生命周期靠 `ControlPersist` 兜底。

*路线 B — 原生 russh（Termius 同款，后期升级）*
- 终端自持一条 `russh` 连接；每个窗口 = `Handle::open_session` 开 channel + `request_pty` + shell。（已确认 russh 支持单连接多 channel + PTY）
- 侧边栏直接映射 channels，可显示连接状态 / 延迟 / 断线重连。
- 代价：自己实现认证（密钥 / 密码 / **keyboard-interactive 走 MFA/OTP**）、known_hosts、ssh-agent、keepalive、ProxyJump。

**建议**：先用路线 A 把「侧边栏 + 同连接多窗口免认证」跑通（几十行），验证体验；要 Termius 级连接管理 UI 再上路线 B。

**和 tmux 互补**：原生窗口让每个 agent 独占一个窗口、各自的通知 / 监控 / 角标——这是 tmux 的 window（共享一块屏）做不到的隔离。需要时还能让每个原生窗口 attach 到各自的 tmux 会话。

---

## 8. Rust 技术栈（均已核对为当前主流且活跃）

| 关注点 | crate | 说明 |
|---|---|---|
| 窗口 + 输入 | `winit` | 跨平台开窗、键鼠/resize/focus 事件 |
| GPU 渲染 | `wgpu` | mac→Metal，Linux→Vulkan/GL |
| 字体 shaping/回退/光栅化 | `cosmic-text` | 内部 `harfrust`+`swash`，纯 Rust，免 FreeType/HarfBuzz C 依赖；连字 + 彩色 emoji + CJK 回退 |
| ANSI/VT 解析 | `vte` | Alacritty 维护，你实现 `Perform`（注意要扩展 OSC/DCS passthrough 处理） |
| PTY | `portable-pty` | WezTerm 出品，跨平台 |
| SSH（路线 B 原生） | `russh` / `openssh` | `russh`=单连接多 channel+PTY；`openssh`=包装系统 ssh 的 ControlMaster |
| Unicode 宽度 / 字素簇 | `unicode-width` / `unicode-segmentation` | — |
| 剪贴板 / 通知 | `arboard` / `notify-rust` | 复制粘贴；桌面通知 |
| 配置 / 日志 | `serde`+`toml` / `tracing` | — |

**捷径**：`alacritty_terminal` 把「解析 + 网格状态」整块给你，但**它对 OSC 133 / DCS passthrough / 同步输出的覆盖未必满足你这个 agent 场景**——你大概率要在解析层做扩展，所以建议自己掌控 `Perform`。

---

## 9. 重排后的路线图（MVP → 你的日常工具）

| 阶段 | 内容 | 验收 |
|---|---|---|
| ✅ **M0** PTY 回显 | `portable-pty` spawn `$SHELL`，读字节直接 print | shell 提示符可见 |
| ✅ **M1** 窗口+静态文本 | `winit`+`wgpu`+`glyphon`(cosmic-text) 画字 | 窗口显示文本 |
| ✅ **M2** 最小 REPL | PTY→`vte`→网格→渲染；键盘回写 | 能跑 `ls`/`echo` |
| ✅ **M3** 核心序列 | truecolor/256 SGR + dim/italic/underline/strike、光标、擦除、滚动区域 DECSTBM、RI/IND/NEL、IL/DL/ICH/DCH/ECH、DECSC/DECRC | `ls --color` / vim 排版对 |
| ✅ **M4** 输入完整 | 应用光标键、F1–F12、Ctrl/Alt/Shift 修饰编码、bracketed paste、鼠标 1000/1002/1003+1006 | readline 编辑正常 |
| ✅ **M5** resize | 全窗口重排 + `TIOCSWINSZ` + `SIGWINCH` | 改大小 shell 跟随 |
| ✅ **M6** 兼容性攻坚 ⭐ | **备用屏 1049 + 同步输出 2026** + OSC 8/52 + **DCS passthrough** + focus 1004 + 复制粘贴 | **ssh→tmux→Claude Code 全屏无闪烁、复制到本地剪贴板** |
| ✅ **M7** 多连接·多窗口 ⭐ | 窗口扁平列表 + **ControlMaster 连接复用** + 顶部 tab 条 | 同一连接下 `Cmd+T` 秒开新标签、免再认证 |
| ✅ **M8** 完成通知 ⭐ | BEL/OSC9/OSC777 → 桌面通知（notify-rust）+ 失焦感知 + tab 角标 | Claude Code 跑完弹通知 |
| ✅ **M9** 监控 ⭐ | OSC 133 命令开始/结束(退出码) + tab 状态点（绿/黄/红）+ 活动高亮 | tab 显示忙/闲/失败 |
| ✅ **M10** 性能/打磨 | 脏行 shape 缓存、glyph atlas（glyphon）、CJK/emoji 宽度、scrollback、主题 + 配置文件 | 流式输出不卡 |
| 🟦 **M11**（可选） 原生 SSH | `russh` 自持连接、连接管理 UI、断线重连 | **走路线 A（ControlMaster）已满足；russh 为文档化的可选升级，待真实 server 验证** |

> **进度（本版本）**：**M0–M10 全部落地**，跑在最新依赖栈（wgpu 29 / glyphon 0.11 / vte 0.15）。
> 备用屏 + 同步输出 + truecolor + 完美 Unicode + 复制粘贴（含 OSC 52 / DCS passthrough）+ 多标签
> + 桌面通知 + OSC 133 监控 + scrollback + 配置文件，均已实现并通过编译 / clippy(`-D warnings`) / 起窗冒烟测试。
> **M11（原生 russh）**走可选路线：路线 A（ControlMaster）已满足原生 SSH 多窗口；russh 自持连接是
> 标注的后期可选升级（需真实 SSH server 验证）。
> **下一个版本 = M6 兼容性攻坚**（备用屏 1049 + 同步输出 2026 + DCS passthrough），
> 这是跑 Claude Code / vim / tmux 全屏 UI 的前提。

> **依赖栈升级（本版本）**：渲染栈已从 wgpu 0.19 / glyphon 0.5 / cosmic-text 0.10 / vte 0.11
> 升到**当前最新**：**wgpu 29 / glyphon 0.11 / cosmic-text 0.18 / vte 0.15**（winit 仍用最新稳定 0.30）。
> 版本用 caret（`^`）需求而非锁死，可自由上调；glyphon 0.11 在依赖表里直接耦合 wgpu 29 + cosmic-text 0.18，
> 故三者自动对齐。主要 API 迁移落在 `render.rs`（`Cache`/`Viewport`、`CurrentSurfaceTexture` 枚举、
> 管线描述符新字段）与 `window.rs`（`vte::advance` 改收切片）；详见 README「依赖版本」。
> 本版本是第一个**真正本地编译 + clippy 通过 + 实机起窗冒烟测试通过**的版本。

> **已知问题 · 输入延迟（已修一轮）**：初版 `render.rs` 每帧把整屏所有行重新 shape，
> 在 debug 构建下打字明显卡顿。已加**按行 shape 缓存**（`render.rs` 的 `line_cache`：对每行内容
> 求哈希，只有变化的行才重排；光标列并入哈希，所以光标移动只失效旧/新两行）。
> 另：**务必用 `cargo run --release`** —— cosmic-text 的字形 shaping 在 unoptimized 下慢很多。
> 后续 M10 还可继续优化：脏区只重绘、glyph atlas 复用、Mailbox present 降延迟。

**总验收**：在你的终端里走完 `ssh → tmux → Claude Code` 全程——颜色/全屏正确、流式 token 不闪、复制能落到本地剪贴板、跑完弹通知、tab 实时显示忙/闲。再跑 `vttest` 查一致性。

---

## 10. 第一步

从 **M0** 开始，但架构**一开始就按多会话设计**（`SessionManager` 持有 `Vec<Session>`，哪怕先只有一个）。`cargo new`，加 `portable-pty`，约 20 行 spawn shell 并回显字节，先证明 PTY 跑通。

---

## 参考项目

- **st**（suckless，C，几千行）— 通读理解最小实现。
- **Alacritty / alacritty_terminal**（Rust）— `vte` 来源；解析+网格可参考/复用。
- **Rio**（Rust + wgpu）— 技术栈最接近；渲染器 Sugarloaf + 脏行重绘思路。
- **WezTerm**（Rust）— 功能最全；`portable-pty` 来源；查"某特性怎么做"。
- **kitty**（C + Python）— GPU 渲染、kitty 键盘协议、同步输出的先驱。
