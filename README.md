# Term

为 AI 编码 agent（Claude Code / opencode）定制的终端，主打 **SSH + tmux 多窗口并行 + 完成通知 + 会话监控**。
完整设计见 [`terminal-emulator-design.md`](./terminal-emulator-design.md)。

当前进度：**M1–M10 全部落地**（M11 原生 russh 走可选路线，见文末），跑在**当前最新依赖栈**上
（winit 0.30 / **wgpu 29** / glyphon 0.11 / cosmic-text 0.18 / vte 0.15）。
备用屏 + 同步输出 + truecolor + 完美 Unicode + 复制粘贴 + 多标签 + 桌面通知 + 会话监控 + scrollback + 配置，
都已实现。本版本已用本地工具链 **编译 + clippy（`-D warnings`）通过 + 实机起窗冒烟测试通过**（debug/release 均 OK）。

## 运行

需要本机装好 Rust（<https://rustup.rs>）。

```bash
cargo run                            # 本地 shell（开一个 GPU 窗口）
cargo run -- gadi                    # ssh 到 ~/.ssh/config 里的别名 gadi
cargo run -- tz1597@gadi.nci.org.au  # 或直接 user@host
```

退出：在 shell 里 `exit` / `Ctrl-D`，或直接关窗口。

> **想要打字跟手就加 `--release`**：`cargo run --release`。cosmic-text 的字形 shaping 在
> debug（unoptimized）下慢很多，会有明显输入延迟。渲染层已做按行 shape 缓存（只重排变化的行），
> release 构建下应当顺滑。

> M0 的 `crossterm` 本地 raw 模式透传已移除，换成自渲染窗口。

## 做了什么（M1–M10）

数据流闭环：

- **输出**：各窗口 PTY 读线程 →（带会话标识的）`winit` 用户事件 → `vte` 解析 → 字符网格（`grid.rs`）→ wgpu/glyphon 渲染
- **输入**：`winit` 键鼠 → 本地快捷键（复制/粘贴/标签/回看）或编码成 VT 字节（`input.rs`）→ 写回**活动**窗口 PTY
- **控制**：resize → 重配交换链 + 全窗口网格重排 + `ioctl(TIOCSWINSZ)`（子进程收到 `SIGWINCH`）

**渲染 / 颜色（完美高亮 + Unicode）**

- 颜色：SGR 16 / 256 / **truecolor 24-bit**，**bold / dim(faint) / italic / underline / inverse / strike / hidden**
- **完美 Unicode**：CJK / emoji 按 `unicode-width` 占两列（宽字符右半占位、组合字符占零列）
- 块状光标（光标色块）、选区高亮、下划线（OSC 8 超链接点击可打开）
- 脏行 shape 缓存（只重排变化的行，回看位置并入缓存键）

**VT 序列（M3 + M6）**

- 光标：CUU/CUD/CUF/CUB/CNL/CPL/CHA/HPA/VPA/CUP、DECSC/DECRC(ESC 7/8 + CSI s/u)
- 擦除/增删：ED/EL/ECH、ICH/DCH、IL/DL、SU/SD、**滚动区域 DECSTBM**、**RI/IND/NEL**
- **备用屏 1049**（vim/htop/tmux/Claude Code 全屏 UI）、**同步输出 2026**（流式 token 不闪烁）
- DEC 私有模式：?1 应用光标键、?7 自动换行、?25 光标显隐、?1000/1002/1003 鼠标、?1006 SGR 鼠标、?1004 焦点、?2004 括号粘贴
- **OSC**：0/2 标题、7 cwd、**8 超链接**、9/777 通知、**52 剪贴板**、**133 提示符标记**
- **DCS passthrough**：tmux 把 OSC 52 等包进 `DCS tmux; … ST`，解包后再解析（差异化卖点）
- DA / DSR 应答（回写 PTY）、RIS 硬复位

**输入（M4）**

- 可打印字符、方向键（含**应用光标键** DECCKM）、Home/End/PgUp/PgDn/Del/Ins、**F1–F12**
- **Shift/Alt/Ctrl 修饰编码**（xterm `CSI 1;mod 〈letter〉`）、Ctrl+字母 控制码、Alt 前缀 ESC
- **括号粘贴**（DECSET 2004，粘贴注入剔除）

**复制粘贴 / 鼠标（M6）**

- **复制**：鼠标拖选 + `Cmd+C`（mac）/ `Ctrl+Shift+C`（Linux）→ 系统剪贴板
- **粘贴**：`Cmd+V` / `Ctrl+Shift+V` → 剪贴板（应用开括号粘贴时自动包裹）
- 应用启用鼠标上报（1000/1002/1003 + 1006）时鼠标事件转发；按住 **Shift** 强制本地选择
- **OSC 52** 让远端 app（含 tmux 里的 Claude Code）能把内容写到你的本地剪贴板

**多标签（M7）**

- 顶部 tab 条；`Cmd+T` 新标签 / `Cmd+W` 关标签 / `Cmd+1..9` 跳转 / `Cmd+[` `Cmd+]` 切换
- 每标签独立 PTY + Parser + Grid + 监控状态；背景标签持续解析（切回去即最新）
- SSH 同主机多标签靠 **ControlMaster 复用连接**，第二个标签秒开免认证（见下）

**完成通知（M8）**

- `BEL` / `OSC 9` / `OSC 777` → **桌面通知**（`notify-rust`），**仅在失焦或非当前标签时弹**
- 对应标签点亮**角标**（tab 上的提示点）

**会话监控（M9）**

- **OSC 133** 命令开始/结束（带退出码）→ 每标签 运行中 / 空闲 / 失败 状态点（绿/黄/红）
- 非当前标签有新输出 → tab 高亮

**回看 / resize / 配置（M5 + M10）**

- **scrollback**：鼠标滚轮 / `Shift+PgUp` `Shift+PgDn` 回看历史（备用屏与鼠标模式下让给应用）
- resize 全窗口重排 + `TIOCSWINSZ`
- **配置文件** `~/.config/term/config.toml`：字号、scrollback 行数、主题色（见下）

**还没做**

- **M11（可选）原生 SSH（russh）**：见下文「关于 M11」——已用**路线 A（ControlMaster）**满足原生 SSH 多窗口需求，
  russh 自持连接是文档里标注的可选升级（需真实 SSH server 验证，故保留为后续）。
- kitty 键盘协议（CSI u）、sixel/图片、连字（ligature）开关、按 grapheme 簇组合宽度的极端 case。

## 快捷键

| 操作 | mac | Linux |
|---|---|---|
| 复制选区 | `Cmd+C` | `Ctrl+Shift+C` |
| 粘贴 | `Cmd+V` | `Ctrl+Shift+V` |
| 新标签 | `Cmd+T` | `Ctrl+Shift+T` |
| 关标签 | `Cmd+W` | `Ctrl+Shift+W` |
| 跳到第 n 个标签 | `Cmd+1..9` | `Ctrl+Shift+1..9` |
| 上/下一个标签 | `Cmd+[` / `Cmd+]` | `Ctrl+Shift+[` / `]` |
| 回看历史 | `Shift+PgUp` / `PgDn`，或鼠标滚轮 | 同 |
| 选择文本 | 鼠标拖选（应用要鼠标时按 `Shift` 强制本地选择） | 同 |
| 打开超链接 | 点击 OSC 8 链接单元格 | 同 |

## 配置 `~/.config/term/config.toml`

文件缺失或字段缺省都回退内置默认；解析出错不会让程序起不来。

```toml
font_size = 16.0       # 逻辑像素字号（未乘 HiDPI 缩放），默认 15
scrollback = 10000     # 主屏回看行数，默认 5000

[theme]
fg = "#cdd6f4"
bg = "#1e1e2e"
cursor = "#f5e0dc"
selection = "#45475a"
```

## 关于 zsh-autosuggestions / zsh-syntax-highlighting

这两个是 **zsh 插件**（在 shell 里跑），不是终端的功能——它们靠发标准 ANSI 序列实现：
自动建议用暗灰前景（dim/256 色），语法高亮用红/绿/黄/青 SGR。**终端这边要做的是把它们渲染完美**，
而本终端已支持：truecolor + 256 + **dim/bold/italic/underline** + 完美 Unicode + 跟手的就地重绘（输入时整行重新上色不卡），
「右方向键/End 接受建议」也已由 M4 输入编码支持。装好插件即可获得你描述的体验：

```bash
# Oh My Zsh 用户：
git clone https://github.com/zsh-users/zsh-autosuggestions   ${ZSH_CUSTOM:-~/.oh-my-zsh/custom}/plugins/zsh-autosuggestions
git clone https://github.com/zsh-users/zsh-syntax-highlighting ${ZSH_CUSTOM:-~/.oh-my-zsh/custom}/plugins/zsh-syntax-highlighting
# 然后在 ~/.zshrc 的 plugins=(...) 里加这两个；syntax-highlighting 必须放最后。
```

## 关于 M11（原生 SSH）

设计文档把 SSH 多窗口分两条路线，并**推荐先走路线 A**：

- **路线 A（已实现）**：每个标签用系统 `ssh` + **ControlMaster** 复用一条底层连接。
  白嫖系统的密钥 / known_hosts / ProxyJump / MFA，零重复造轮子；同主机第二个标签秒开免认证。
- **路线 B（可选，未实现）**：用 `russh` 自持连接。需自己实现认证（含 keyboard-interactive MFA）、
  known_hosts、keepalive、断线重连，且要把异步 SSH channel 桥到现有同步 PTY 接口。
  这是文档里标注的「后期升级 / Termius 级体验」，本沙箱无 SSH server 可验证，故保留为后续可选项。

## 依赖版本（最新栈，版本可调）

本版本**已用本地工具链编译 + clippy 通过**（debug / release 均 OK，cargo 1.91）。
依赖全部升到**当前最新栈**，并用 caret（`^`）需求而非锁死精确版本——所以这些库的小版本
**可自由上调**（"adjustable version"）。关键耦合点：**glyphon 0.11 在 crates.io 依赖表里
就声明了 `wgpu ^29` + `cosmic-text ^0.18`**，故只要 glyphon 大版本不变，cargo 会自动挑到
与之匹配的 wgpu / cosmic-text，无需手工对齐三者版本。

| crate | 版本 | 关键 API 点（相对旧版 0.19/0.5 的迁移） |
|---|---|---|
| `winit` | `0.30` | `ApplicationHandler` 架构；`run_app`、`EventLoop::with_user_event().build()`、`ActiveEventLoop::create_window`。只与 raw-window-handle 0.6 耦合，独立于 wgpu（0.31 仍是 beta，故用最新稳定 0.30） |
| `wgpu` | `29` | `Instance::new(InstanceDescriptor)`（按值、无 `Default`，用 `new_without_display_handle()`）；`request_adapter`/`request_device` 返回 `Result`；`get_current_texture` 返回 `CurrentSurfaceTexture` **枚举**（非 `Result`）；管线描述符多 `compilation_options`/`cache`、`entry_point: Option<&str>`、`multiview_mask`；`PipelineLayoutDescriptor` 用 `immediate_size`；color attachment 多 `depth_slice` |
| `glyphon` | `0.11` | 引入 `Cache` + `Viewport`：`TextAtlas::with_color_mode(d,q,&cache,fmt,ColorMode::Web)`、`prepare(...)` 收 `&Viewport`、`render(&atlas,&viewport,&mut pass)` 3 参、`TextArea` 多 `custom_glyphs`。**自动耦合 wgpu 29、cosmic-text 0.18** |
| `cosmic-text`（经 glyphon 0.11 传递引入） | `0.18` | `Buffer::set_size(&mut fs, Option<f32>, Option<f32>)`、`set_text(.., &Attrs, shaping, Option<Align>)`、`shape_until_scroll(&mut fs, prune: bool)` |
| `vte` | `0.15` | `Parser::advance(&mut perf, bytes: &[u8])` 收整片（旧版 0.11 逐字节）。`Perform` trait 各回调签名不变，故 `parser.rs` 无需改 |

> 想锁死精确版本（复现构建）可改成 `winit = "=0.30.13"`、`wgpu = "=29.0.3"`、
> `glyphon = "=0.11.0"`、`vte = "=0.15.0"`；但默认用 caret 以保持「版本可调」。

颜色空间：刻意选**非 sRGB（Unorm）**交换链格式 + glyphon `ColorMode::Web`，让矩形与文字都
「按 0–255 当 sRGB 直接写」，所见即所得、深浅一致。若某些 GPU 上只有 sRGB 格式可选，
需要把矩形 shader 改成 sRGB→linear 并把 atlas 换回 `ColorMode::Accurate`。

## 代码结构

```
ConnectionManager   src/manager.rs    顶层；tab 条 = 所有连接的窗口扁平列表
└─ Connection       src/connection.rs 一个 SSH 目标；同目标多窗口复用连接（ControlMaster）
   └─ Window        src/window.rs     PTY + writer + 独立 vte::Parser + 独立 Grid + 监控状态
      ├─ PtyProcess src/pty.rs        PTY 封装（reader / writer / resize）
      ├─ Grid       src/grid.rs       网格 + 光标 + 画笔 + 模式 + 滚动区域 + 备用屏 + scrollback + 事件队列
      └─ Parser     src/parser.rs     vte::Perform → 把动作落到 Grid，副作用冒泡进事件队列

App / 事件循环      src/app.rs        winit ApplicationHandler：多窗口路由 / 键鼠 / 复制粘贴 / 标签 / 通知 / 重绘
键盘编码            src/input.rs      winit 按键 → VT 字节（应用光标键 / F 键 / 修饰编码 / 括号粘贴）
渲染               src/render.rs     wgpu 交换链 + glyphon 文字（bold/italic/underline）+ 矩形管线（背景/光标/选区/下划线/tab 条）
配置               src/config.rs     ~/.config/term/config.toml（字号 / 主题 / scrollback）
```

## 验证「同主机多窗口免认证复用」

机制写在 `src/connection.rs` 的 `build_command()`（ControlMaster）。`Cmd+T` 在同一连接下开新标签即复用。
手动验证：`cargo run -- <host>` 后按 `Cmd+T`，第二个标签应秒进免认证（复用 `~/.ssh/cm-*` master）。
辅助：`ssh -O check <host>` / `ssh -O exit <host>`。

> 远端在 tmux 里跑 Claude Code 时，剪贴板/通知穿透还需远端 `tmux` 配
> `set -g allow-passthrough on`、`set -g set-clipboard on`（见设计文档 §6）。
> 本终端已支持 DCS passthrough 解包，配合上面两行即可让 `ssh → tmux → Claude Code` 的 OSC 52 复制落到本地剪贴板。

## 总验收

在本终端里 `ssh → tmux → Claude Code` 全程：备用屏全屏 UI 正确、流式 token 不闪（同步输出）、
复制能落到本地剪贴板（OSC 52 / DCS passthrough）、跑完弹桌面通知（BEL/OSC9）、tab 实时显示忙/闲/失败（OSC 133）。
