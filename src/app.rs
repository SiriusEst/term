//! winit 应用：把窗口、渲染器、多个 PTY 会话接起来，跑事件循环。
//!
//! 数据流闭环：
//!   - 输出：各窗口 PTY 读线程 →（带 conn/win 的）用户事件 → `Window::feed` 解析 → 取事件 → 落平台 → 重绘
//!   - 输入：winit 键鼠 → 本地快捷键（复制/粘贴/标签/回看）或 `input::encode` → 写回**活动**窗口 PTY
//!   - 控制：resize → 重配交换链 + 全窗口网格重排 + `TIOCSWINSZ`
//!
//! 覆盖里程碑：M4 输入、M5 resize、M6 复制粘贴/备用屏/同步输出/焦点、M7 多标签、M8 通知、M9 监控。

use std::io::Read;
use std::sync::Arc;
use std::thread;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window as WinitWindow, WindowId};

use crate::grid::{MouseProto, Selection, TermEvent};
use crate::input;
use crate::manager::ConnectionManager;
use crate::render::{Renderer, SidebarItem};
use crate::window::{WinStatus, Window};

/// 侧边栏某一行点击后的目标。
enum SidebarTarget {
    /// host 分组头：点击在该连接下新开一个窗口。
    Host(usize),
    /// 窗口行：切到该窗口。
    Win(usize, usize),
}

/// 投递到事件循环的自定义事件（来自各 PTY 读线程，带会话标识）。
pub enum UserEvent {
    /// 某窗口的一批 PTY 输出字节。
    Pty {
        conn: usize,
        win: usize,
        bytes: Vec<u8>,
    },
    /// 某窗口的 PTY 关闭（子进程退出）。
    Closed { conn: usize, win: usize },
}

/// 用系统默认程序打开一个 URL（OSC 8 超链接点击）。只放行常见安全 scheme。
fn open_url(url: &str) {
    let ok = ["http://", "https://", "file://", "mailto:"]
        .iter()
        .any(|p| url.starts_with(p));
    if !ok {
        return;
    }
    let prog = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(prog).arg(url).spawn();
}

/// 起一个 PTY 读线程：把输出按 (conn,win) 路由回事件循环。
pub fn spawn_reader(
    proxy: EventLoopProxy<UserEvent>,
    conn: usize,
    win: usize,
    mut reader: Box<dyn Read + Send>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = proxy.send_event(UserEvent::Closed { conn, win });
                    break;
                }
                Ok(n) => {
                    if proxy
                        .send_event(UserEvent::Pty {
                            conn,
                            win,
                            bytes: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

pub struct App {
    manager: ConnectionManager,
    conn_idx: usize,
    win_id: usize,
    /// 所有标签的 (conn, win)，决定 tab 顺序。
    tabs: Vec<(usize, usize)>,
    proxy: EventLoopProxy<UserEvent>,

    window: Option<Arc<WinitWindow>>,
    renderer: Option<Renderer>,
    mods: ModifiersState,
    focused: bool,

    clipboard: Option<arboard::Clipboard>,

    /// 当前配置（M10）；偏好面板改它并写回磁盘。
    config: crate::config::Config,

    // 鼠标 / 选区
    mouse_pos: (f64, f64),
    mouse_down: bool,
    selecting: bool,
    selection: Option<Selection>,
    /// 同步输出(2026)期间连续跳过的刷新数——超过阈值强刷一帧，防异常 app 永久冻屏。
    sync_skip_count: u32,
    /// 侧边栏显隐覆盖：None = 自动（>1 窗口才显示）；Some(b) = 用户 Cmd+B 强制。
    sidebar_override: Option<bool>,
    /// 偏好面板（Cmd+,）：Some = 打开，记录选中行。
    pref_row: Option<usize>,

    // ---- 补全浮层 ----
    /// 自上次提示符/换行以来「连续敲入」的字符（任何导航/控制键都会清空它）。
    input_buf: String,
    /// 命令历史（去重，最近在后）。
    history: Vec<String>,
    /// $PATH 下可执行名缓存（懒加载）。
    path_bins: Option<Vec<String>>,
    /// 当前补全候选浮层。
    compl: Option<Completion>,
}

/// 补全候选浮层状态。
struct Completion {
    /// (显示文本, 接受时要补发给 PTY 的后缀)。
    items: Vec<(String, String)>,
    sel: usize,
    /// 浮层锚点 = 打开时光标所在单元格 (col,row)。
    anchor: (usize, usize),
}

/// 偏好面板的行数（字号 / 配色 / scrollback / 侧边栏）。
const PREF_ROWS: usize = 4;

impl App {
    pub fn new(
        mut manager: ConnectionManager,
        conn_idx: usize,
        win_id: usize,
        proxy: EventLoopProxy<UserEvent>,
        config: crate::config::Config,
    ) -> Self {
        let tabs = manager.all_windows();
        // 把配置的 scrollback 应用到已存在的窗口。
        if let Some(n) = config.scrollback {
            for (c, w) in &tabs {
                if let Some(win) = manager.window_mut(*c, *w) {
                    win.grid.set_scrollback_max(n);
                }
            }
        }
        Self {
            manager,
            conn_idx,
            win_id,
            tabs,
            proxy,
            window: None,
            renderer: None,
            mods: ModifiersState::empty(),
            focused: true,
            clipboard: arboard::Clipboard::new().ok(),
            config,
            mouse_pos: (0.0, 0.0),
            mouse_down: false,
            selecting: false,
            selection: None,
            sync_skip_count: 0,
            sidebar_override: None,
            pref_row: None,
            input_buf: String::new(),
            history: Vec::new(),
            path_bins: None,
            compl: None,
        }
    }

    /// 侧边栏当前是否显示（默认 >1 窗口才显示；Cmd+B 可强制）。
    fn show_sidebar(&self) -> bool {
        self.sidebar_override.unwrap_or(self.tabs.len() > 1)
    }

    /// 把窗口数 + 侧边栏显隐喂给渲染器（影响可用列数）。在 sync_all_sizes / render 前调用。
    fn apply_layout(&mut self) {
        let n = self.tabs.len();
        let show = self.show_sidebar();
        if let Some(r) = self.renderer.as_mut() {
            r.set_layout(n, show);
        }
    }

    /// 构造侧边栏行 + 各行点击目标（host 分组 → 其下窗口）。
    fn build_sidebar(&self) -> (Vec<SidebarItem>, Vec<SidebarTarget>) {
        let mut items = Vec::new();
        let mut targets = Vec::new();
        for (ci, conn) in self.manager.connections.iter().enumerate() {
            items.push(SidebarItem {
                label: conn.label.clone(),
                is_host: true,
                status: WinStatus::Idle,
                activity: false,
                alerted: false,
                active: false,
            });
            targets.push(SidebarTarget::Host(ci));
            for w in conn.windows() {
                let active = ci == self.conn_idx && w.id == self.win_id;
                items.push(SidebarItem {
                    label: w.title.clone(),
                    is_host: false,
                    status: w.status,
                    activity: w.activity,
                    alerted: w.alerted,
                    active,
                });
                targets.push(SidebarTarget::Win(ci, w.id));
            }
        }
        (items, targets)
    }

    fn active(&mut self) -> Option<&mut Window> {
        self.manager.window_mut(self.conn_idx, self.win_id)
    }

    /// 写字节到活动窗口 PTY。
    fn write_active(&mut self, bytes: &[u8]) {
        if let Some(w) = self.active() {
            w.write(bytes);
        }
    }

    /// 活动窗口当前是否处于同步输出（2026）——是则本帧不刷新，避免撕裂/闪烁。
    fn active_syncing(&mut self) -> bool {
        self.active().map(|w| w.grid.modes.sync_output).unwrap_or(false)
    }

    fn request_redraw(&self) {
        if let Some(win) = self.window.as_ref() {
            win.request_redraw();
        }
    }

    /// 标签集合变化后：重建 tab 列表 → 把新标签数喂给渲染器（决定 tab 条高度）→ 全窗口按新尺寸重排。
    /// 顺序很关键：必须先 set_tab_count 再 sync_all_sizes，否则 PTY 会按旧 tab 条高度算行数。
    fn refresh_tabs(&mut self) {
        self.tabs = self.manager.all_windows();
        self.apply_layout();
        self.sync_all_sizes();
    }

    /// 把渲染器算出的 (列,行) 同步到**所有**窗口（背景窗口也要跟随，切回去才正确）。
    fn sync_all_sizes(&mut self) {
        let dims = self.renderer.as_ref().map(|r| r.cols_rows());
        if let Some((cols, rows)) = dims {
            for (conn, win) in self.tabs.clone() {
                if let Some(w) = self.manager.window_mut(conn, win) {
                    w.resize(cols as u16, rows as u16);
                }
            }
        }
    }

    // ---- 复制 / 粘贴 ----

    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            return;
        }
        let (a, b) = sel.ordered();
        let text = match self.active() {
            Some(w) => w.grid.text_in_span(a, b),
            None => return,
        };
        self.set_clipboard(text);
    }

    fn set_clipboard(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
    }

    fn paste(&mut self) {
        let text = match self.clipboard.as_mut() {
            Some(cb) => cb.get_text().unwrap_or_default(),
            None => return,
        };
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .active()
            .map(|w| w.grid.modes.bracketed_paste)
            .unwrap_or(false);
        let bytes = input::encode_paste(&text, bracketed);
        self.write_active(&bytes);
    }

    // ---- 标签管理（M7）----

    fn new_tab(&mut self) {
        self.new_tab_in(self.conn_idx);
    }

    /// 在指定连接下新开一个窗口（侧边栏点 host 头 / Cmd+T 都走这里）。
    fn new_tab_in(&mut self, conn: usize) {
        let dims = self.renderer.as_ref().map(|r| r.cols_rows()).unwrap_or((80, 24));
        let new_win = match self.manager.connection_mut(conn) {
            Some(c) => match c.open_window(dims.0 as u16, dims.1 as u16) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("[ianua] 新建标签失败: {e}");
                    return;
                }
            },
            None => return,
        };
        // 起读线程。
        if let Some(c) = self.manager.connection(conn) {
            if let Some(w) = c.window(new_win) {
                if let Ok(reader) = w.pty.reader() {
                    spawn_reader(self.proxy.clone(), conn, new_win, reader);
                }
            }
        }
        if let Some(n) = self.config.scrollback {
            if let Some(w) = self.manager.window_mut(conn, new_win) {
                w.grid.set_scrollback_max(n);
            }
        }
        self.refresh_tabs();
        self.switch_to(conn, new_win);
    }

    fn close_active_tab(&mut self, event_loop: &ActiveEventLoop) {
        let (conn, win) = (self.conn_idx, self.win_id);
        if let Some(c) = self.manager.connection_mut(conn) {
            c.close_window(win);
        }
        self.refresh_tabs(); // tab 条可能消失 → 终端区域变大，所有窗口要重排
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        let (nc, nw) = *self.tabs.last().unwrap();
        self.switch_to(nc, nw);
    }

    fn switch_to(&mut self, conn: usize, win: usize) {
        self.conn_idx = conn;
        self.win_id = win;
        self.selection = None;
        self.selecting = false;
        self.reset_input(); // 切窗口后输入跟踪/补全作废
        if let Some(w) = self.active() {
            w.activity = false;
            w.alerted = false;
            w.grid.reset_view();
        }
        if let Some(win) = self.window.as_ref() {
            win.set_title(&self.active_title());
        }
        self.request_redraw();
    }

    fn select_tab(&mut self, idx: usize) {
        if let Some(&(c, w)) = self.tabs.get(idx) {
            self.switch_to(c, w);
        }
    }

    fn cycle_tab(&mut self, forward: bool) {
        if self.tabs.len() < 2 {
            return;
        }
        let cur = self
            .tabs
            .iter()
            .position(|&(c, w)| c == self.conn_idx && w == self.win_id)
            .unwrap_or(0);
        let n = self.tabs.len();
        let next = if forward { (cur + 1) % n } else { (cur + n - 1) % n };
        let (c, w) = self.tabs[next];
        self.switch_to(c, w);
    }

    fn active_title(&self) -> String {
        self.manager
            .connection(self.conn_idx)
            .and_then(|c| c.window(self.win_id))
            .map(|w| w.title.clone())
            .unwrap_or_else(|| "ianua".into())
    }

    // ---- 通知（M8）----

    fn notify(&mut self, conn: usize, win: usize, title: String, body: String) {
        let is_active = conn == self.conn_idx && win == self.win_id;
        // 标签角标：失焦或非当前标签时点亮。
        if !self.focused || !is_active {
            if let Some(w) = self.manager.window_mut(conn, win) {
                w.alerted = true;
            }
        }
        // 桌面通知：仅在失焦或非当前标签时弹（避免打扰当前正看的窗口）。
        if !self.focused || !is_active {
            thread::spawn(move || {
                let _ = notify_rust::Notification::new()
                    .summary(if title.is_empty() { "ianua" } else { &title })
                    .body(&body)
                    .show();
            });
        }
        self.request_redraw();
    }

    fn on_bell(&mut self, conn: usize, win: usize) {
        // BEL → 完成提示：失焦/非当前标签时弹通知 + 角标。
        let is_active = conn == self.conn_idx && win == self.win_id;
        if !self.focused || !is_active {
            let title = self
                .manager
                .connection(conn)
                .and_then(|c| c.window(win))
                .map(|w| w.title.clone())
                .unwrap_or_else(|| "ianua".into());
            self.notify(conn, win, title, "完成".into());
        }
    }

    // ---- 鼠标上报（DECSET 1000/1002/1003 + 1006）----

    fn mouse_report(&mut self, btn: u8, press: bool) {
        let (proto, sgr) = match self.active() {
            Some(w) => (w.grid.modes.mouse_proto, w.grid.modes.mouse_sgr),
            None => return,
        };
        if proto == MouseProto::None {
            return;
        }
        let (col, row) = match self.renderer.as_ref() {
            Some(r) => r.cell_at(self.mouse_pos.0, self.mouse_pos.1),
            None => return,
        };
        let seq = if sgr {
            format!(
                "\x1b[<{btn};{};{}{}",
                col + 1,
                row + 1,
                if press { 'M' } else { 'm' }
            )
            .into_bytes()
        } else {
            // 传统编码：ESC [ M  Cb Cx Cy（按钮码与坐标各 +32，越界夹断到 223）。
            let button = if press { btn } else { 3 };
            let cb = 32 + button.min(223);
            let cx = 32 + ((col + 1).min(223)) as u8;
            let cy = 32 + ((row + 1).min(223)) as u8;
            vec![0x1b, b'[', b'M', cb, cx, cy]
        };
        self.write_active(&seq);
    }

    // ---- 偏好面板（Cmd+,）----

    /// 构造偏好面板视图。
    fn build_pref_view(&self) -> crate::render::PrefView {
        let font = self.config.font_size.unwrap_or(15.0) as i32;
        let scheme = self
            .config
            .theme
            .scheme
            .clone()
            .unwrap_or_else(|| "catppuccin".into());
        let scrollback = self.config.scrollback.unwrap_or(5000);
        let sidebar = match self.sidebar_override {
            None => "自动",
            Some(true) => "开",
            Some(false) => "关",
        };
        crate::render::PrefView {
            title: "偏好  ·  ↑↓ 选行  ◀▶ 改值  Esc 保存关闭".into(),
            rows: vec![
                ("字号 Font size".into(), font.to_string()),
                ("配色 Color scheme".into(), scheme),
                ("回看 Scrollback".into(), scrollback.to_string()),
                ("侧边栏 Sidebar".into(), sidebar.into()),
            ],
            selected: self.pref_row.unwrap_or(0),
        }
    }

    /// 偏好面板打开时的按键。
    fn handle_pref_key(&mut self, key: &Key) {
        let close = matches!(key, Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter))
            || (self.mods.super_key() && matches!(key, Key::Character(s) if s.as_str() == ","));
        if close {
            self.pref_row = None;
            self.config.save();
            self.request_redraw();
            return;
        }
        if let Key::Named(n) = key {
            let r = self.pref_row.unwrap_or(0);
            match n {
                NamedKey::ArrowUp => self.pref_row = Some((r + PREF_ROWS - 1) % PREF_ROWS),
                NamedKey::ArrowDown => self.pref_row = Some((r + 1) % PREF_ROWS),
                NamedKey::ArrowLeft => self.pref_change(false),
                NamedKey::ArrowRight => self.pref_change(true),
                _ => {}
            }
            self.request_redraw();
        }
    }

    /// 改当前选中行的值（←/→）。
    fn pref_change(&mut self, inc: bool) {
        match self.pref_row.unwrap_or(0) {
            0 => {
                // 字号 ±1（8..32）。
                let cur = self.config.font_size.unwrap_or(15.0);
                let next = (cur + if inc { 1.0 } else { -1.0 }).clamp(8.0, 32.0);
                self.config.font_size = Some(next);
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor() as f32)
                    .unwrap_or(1.0);
                if let Some(r) = self.renderer.as_mut() {
                    r.set_font_size(next, scale);
                }
                self.apply_layout();
                self.sync_all_sizes();
            }
            1 => {
                // 配色方案循环。
                let schemes = crate::theme::Theme::SCHEMES;
                let cur = self
                    .config
                    .theme
                    .scheme
                    .clone()
                    .unwrap_or_else(|| schemes[0].to_string());
                let idx = schemes.iter().position(|s| *s == cur).unwrap_or(0);
                let n = schemes.len();
                let next = if inc { (idx + 1) % n } else { (idx + n - 1) % n };
                self.config.theme.scheme = Some(schemes[next].to_string());
                let theme = self.config.theme();
                if let Some(r) = self.renderer.as_mut() {
                    r.set_theme(theme);
                }
            }
            2 => {
                // scrollback ±1000（0..1e6）。
                let cur = self.config.scrollback.unwrap_or(5000);
                let next = if inc {
                    (cur + 1000).min(1_000_000)
                } else {
                    cur.saturating_sub(1000)
                };
                self.config.scrollback = Some(next);
                for (c, w) in self.tabs.clone() {
                    if let Some(win) = self.manager.window_mut(c, w) {
                        win.grid.set_scrollback_max(next.max(1));
                    }
                }
            }
            3 => {
                // 侧边栏：自动 → 开 → 关 → 自动。
                self.sidebar_override = match self.sidebar_override {
                    None => Some(true),
                    Some(true) => Some(false),
                    Some(false) => None,
                };
                self.apply_layout();
                self.sync_all_sizes();
            }
            _ => {}
        }
    }

    // ---- 补全浮层 ----

    /// 清掉输入跟踪 + 关浮层（切窗口 / 新提示符时）。
    fn reset_input(&mut self) {
        self.input_buf.clear();
        self.compl = None;
    }

    /// 懒加载 $PATH 下可执行名。
    fn ensure_path_bins(&mut self) {
        if self.path_bins.is_some() {
            return;
        }
        let mut names = Vec::new();
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for ent in rd.flatten() {
                        if let Some(n) = ent.file_name().to_str() {
                            names.push(n.to_string());
                        }
                    }
                }
            }
        }
        names.sort();
        names.dedup();
        self.path_bins = Some(names);
    }

    /// 活动窗口 OSC 7 上报的 cwd（`file://host/path` → 本地路径）。
    fn active_cwd(&self) -> Option<std::path::PathBuf> {
        let w = self
            .manager
            .connection(self.conn_idx)?
            .window(self.win_id)?;
        let cwd = w.cwd.as_deref()?;
        let path = if let Some(rest) = cwd.strip_prefix("file://") {
            let i = rest.find('/')?;
            &rest[i..]
        } else if cwd.starts_with('/') {
            cwd
        } else {
            return None;
        };
        Some(std::path::PathBuf::from(path))
    }

    fn active_cursor(&self) -> (usize, usize) {
        self.manager
            .connection(self.conn_idx)
            .and_then(|c| c.window(self.win_id))
            .map(|w| (w.grid.cx, w.grid.cy))
            .unwrap_or((0, 0))
    }

    /// 根据当前输入缓冲收集补全候选：历史整行 + PATH 可执行（首词）+ cwd 文件（最后一段）。
    fn gather_candidates(&self) -> Vec<(String, String)> {
        let buf = self.input_buf.as_str();
        let trimmed = buf.trim_start();
        let mut items: Vec<(String, String)> = Vec::new();
        // 1) 历史整行（buf 是前缀）——重复长命令最有用。
        for line in self.history.iter().rev() {
            if line.len() > buf.len() && line.starts_with(buf) {
                items.push((line.clone(), line[buf.len()..].to_string()));
                if items.len() >= 6 {
                    break;
                }
            }
        }
        // 2) 最后一个 token 补全。
        let token = buf.rsplit(' ').next().unwrap_or("");
        if !token.is_empty() {
            let is_first = trimmed == token; // 整行只有一个词 → 补命令名
            if is_first {
                if let Some(bins) = &self.path_bins {
                    for name in bins {
                        if name.starts_with(token) && name.len() > token.len() {
                            items.push((name.clone(), name[token.len()..].to_string()));
                        }
                    }
                }
            }
            if let Some(cwd) = self.active_cwd() {
                let (dir, prefix) = match token.rfind('/') {
                    Some(s) => (cwd.join(&token[..=s]), token[s + 1..].to_string()),
                    None => (cwd, token.to_string()),
                };
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for ent in rd.flatten() {
                        if let Some(name) = ent.file_name().to_str() {
                            if name.starts_with(&prefix) && name.len() > prefix.len() {
                                let mut sfx = name[prefix.len()..].to_string();
                                if ent.path().is_dir() {
                                    sfx.push('/');
                                }
                                items.push((name.to_string(), sfx));
                            }
                        }
                    }
                }
            }
        }
        // 按后缀全局去重，截断 8 个。
        let mut seen = std::collections::HashSet::new();
        items.retain(|(_, sfx)| seen.insert(sfx.clone()));
        items.truncate(8);
        items
    }

    /// 重算补全候选并更新浮层。
    fn recompute_completion(&mut self) {
        if self.input_buf.trim_start().is_empty() {
            self.compl = None;
            return;
        }
        self.ensure_path_bins();
        let items = self.gather_candidates();
        if items.is_empty() {
            self.compl = None;
        } else {
            let anchor = self.active_cursor();
            self.compl = Some(Completion {
                items,
                sel: 0,
                anchor,
            });
        }
    }

    /// 接受当前选中候选：把后缀补发给 PTY。
    fn compl_accept(&mut self) {
        if let Some(c) = self.compl.take() {
            if let Some((_, sfx)) = c.items.get(c.sel) {
                let sfx = sfx.clone();
                self.input_buf.push_str(&sfx);
                self.write_active(sfx.as_bytes());
            }
        }
        self.request_redraw();
    }

    fn compl_nav(&mut self, delta: i32) {
        if let Some(c) = self.compl.as_mut() {
            let n = c.items.len() as i32;
            if n > 0 {
                c.sel = (((c.sel as i32 + delta) % n + n) % n) as usize;
            }
        }
        self.request_redraw();
    }

    /// 按本次按键更新输入跟踪缓冲（再触发补全重算）。
    /// 只跟踪「连续敲入的可见字符」：任何导航/控制键都放弃跟踪，避免与 shell 行编辑错位。
    fn update_input_buffer(&mut self, key: &Key, text: Option<&str>) {
        let ctrl = self.mods.control_key();
        let alt = self.mods.alt_key();
        match key {
            Key::Named(NamedKey::Enter) => {
                let line = self.input_buf.trim().to_string();
                if !line.is_empty() && self.history.last() != Some(&line) {
                    self.history.push(line);
                    if self.history.len() > 1000 {
                        self.history.remove(0);
                    }
                }
                self.input_buf.clear();
            }
            Key::Named(NamedKey::Backspace) => {
                self.input_buf.pop();
            }
            Key::Named(NamedKey::Space) => self.input_buf.push(' '),
            Key::Named(_) => self.input_buf.clear(), // 方向/功能/Tab/Esc 等：放弃跟踪
            Key::Character(_) if ctrl || alt => self.input_buf.clear(),
            Key::Character(_) => {
                if let Some(t) = text {
                    if t.chars().all(|c| !c.is_control()) {
                        self.input_buf.push_str(t);
                    } else {
                        self.input_buf.clear();
                    }
                }
            }
            _ => {}
        }
        self.recompute_completion();
    }

    // ---- 键盘 ----

    /// 返回 true = 已作为本地快捷键处理（不再编码给 PTY）。
    fn handle_shortcut(&mut self, event_loop: &ActiveEventLoop, key: &Key) -> bool {
        let sup = self.mods.super_key();
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();

        // 回看 scrollback：Shift+PgUp / Shift+PgDn。
        if shift {
            if let Key::Named(n) = key {
                match n {
                    NamedKey::PageUp => {
                        if let Some(w) = self.active() {
                            let step = w.grid.rows.max(1) - 1;
                            w.grid.scroll_view_up(step);
                        }
                        self.selection = None; // 选区按可见坐标存，视图一动就作废
                        self.request_redraw();
                        return true;
                    }
                    NamedKey::PageDown => {
                        if let Some(w) = self.active() {
                            let step = w.grid.rows.max(1) - 1;
                            w.grid.scroll_view_down(step);
                        }
                        self.selection = None;
                        self.request_redraw();
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // 复制/粘贴/标签：Cmd（mac）或 Ctrl+Shift（Linux 习惯）。
        let combo = sup || (ctrl && shift);
        if combo {
            if let Key::Character(s) = key {
                match s.to_lowercase().as_str() {
                    "c" => {
                        // Cmd+C / Ctrl+Shift+C 一律当复制（无选区则空操作），
                        // 不下放成 0x03 —— 纯 Ctrl+C（无 shift）才发 SIGINT，走下面的编码路径。
                        self.copy_selection();
                        return true;
                    }
                    "v" => {
                        self.paste();
                        return true;
                    }
                    "t" => {
                        self.new_tab();
                        return true;
                    }
                    "w" => {
                        self.close_active_tab(event_loop);
                        return true;
                    }
                    "b" => {
                        // 切换侧边栏显隐。
                        let cur = self.show_sidebar();
                        self.sidebar_override = Some(!cur);
                        self.apply_layout();
                        self.sync_all_sizes();
                        self.request_redraw();
                        return true;
                    }
                    "," => {
                        // 打开偏好面板。
                        self.pref_row = Some(0);
                        self.request_redraw();
                        return true;
                    }
                    "[" => {
                        self.cycle_tab(false);
                        return true;
                    }
                    "]" => {
                        self.cycle_tab(true);
                        return true;
                    }
                    d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                        let n = (d.as_bytes()[0] - b'0') as usize;
                        if n >= 1 {
                            self.select_tab(n - 1);
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        // 其它 Cmd 组合：吞掉，不发给 PTY。
        sup
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }
        let attrs = WinitWindow::default_attributes()
            .with_title(self.active_title())
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("[ianua] 创建窗口失败: {e}");
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(window.clone(), self.config.font_size, self.config.theme()) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("[ianua] 初始化渲染器失败: {e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.apply_layout();
        self.sync_all_sizes();
        self.request_redraw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Pty { conn, win, bytes } => {
                let is_active = conn == self.conn_idx && win == self.win_id;
                let mut globals: Vec<TermEvent> = Vec::new();
                // 活动窗口若正回看 scrollback，新输出会把视图拉回底部 → 选区坐标失效，置空。
                let mut drop_selection = false;
                if let Some(w) = self.manager.window_mut(conn, win) {
                    if is_active && w.grid.view_offset > 0 {
                        drop_selection = true;
                    }
                    w.feed(&bytes);
                    for ev in w.grid.drain_events() {
                        match ev {
                            TermEvent::Reply(b) => w.write(&b),
                            TermEvent::Title(t) => w.title = t,
                            TermEvent::PromptStart => w.status = WinStatus::Running,
                            TermEvent::CommandEnd(code) => {
                                w.status = match code {
                                    Some(0) | None => WinStatus::Idle,
                                    _ => WinStatus::Failed,
                                };
                            }
                            other => globals.push(other),
                        }
                    }
                    if !is_active {
                        w.activity = true;
                    }
                }
                if drop_selection {
                    self.selection = None;
                }
                if is_active {
                    if let Some(win) = self.window.as_ref() {
                        win.set_title(&self.active_title());
                    }
                }
                for ev in globals {
                    match ev {
                        TermEvent::SetClipboard(s) => self.set_clipboard(s),
                        TermEvent::Bell => self.on_bell(conn, win),
                        TermEvent::Notify(t, b) => self.notify(conn, win, t, b),
                        TermEvent::Cwd(path) => {
                            if let Some(w) = self.manager.window_mut(conn, win) {
                                w.cwd = Some(path);
                            }
                        }
                        _ => {}
                    }
                }
                // 同步输出期间不刷新（2026 去闪烁）；否则请求重绘。
                // 安全阀：异常 app 若只发 2026h 不发 2026l，连续跳过过多帧后强刷一次，避免永久冻屏。
                if self.active_syncing() {
                    self.sync_skip_count += 1;
                    if self.sync_skip_count > 64 {
                        self.sync_skip_count = 0;
                        self.request_redraw();
                    }
                } else {
                    self.sync_skip_count = 0;
                    self.request_redraw();
                }
            }
            UserEvent::Closed { conn, win } => {
                if conn == self.conn_idx && win == self.win_id {
                    // 活动标签的子进程退出 → 切到别的标签，没有了就退出。
                    if let Some(c) = self.manager.connection_mut(conn) {
                        c.close_window(win);
                    }
                    self.refresh_tabs();
                    match self.tabs.last() {
                        Some(&(nc, nw)) => self.switch_to(nc, nw),
                        None => {
                            eprintln!("[ianua] 最后一个会话已退出。");
                            event_loop.exit();
                        }
                    }
                } else {
                    // 背景标签退出：移除它。
                    if let Some(c) = self.manager.connection_mut(conn) {
                        c.close_window(win);
                    }
                    self.refresh_tabs();
                    self.request_redraw();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),

            WindowEvent::Focused(f) => {
                self.focused = f;
                if let Some(w) = self.active() {
                    if w.grid.modes.focus_event {
                        let seq: &[u8] = if f { b"\x1b[I" } else { b"\x1b[O" };
                        w.write(seq);
                    }
                    if f {
                        w.alerted = false;
                    }
                }
                self.request_redraw();
            }

            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                self.apply_layout();
                self.sync_all_sizes();
                self.request_redraw();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // 偏好面板打开时，按键全归它（导航/改值/关闭），不发给 PTY。
                if self.pref_row.is_some() {
                    self.handle_pref_key(&event.logical_key);
                    return;
                }
                // 补全浮层打开时：Tab 接受、↑↓ 选择、Esc 关闭；其余按键照常落到下面。
                if self.compl.is_some() {
                    match &event.logical_key {
                        Key::Named(NamedKey::Tab) => {
                            self.compl_accept();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            self.compl_nav(1);
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.compl_nav(-1);
                            return;
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.compl = None;
                            self.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }
                if self.handle_shortcut(event_loop, &event.logical_key) {
                    return;
                }
                let app_cursor = self
                    .active()
                    .map(|w| w.grid.modes.app_cursor_keys)
                    .unwrap_or(false);
                if let Some(bytes) =
                    input::encode(&event.logical_key, event.text.as_deref(), self.mods, app_cursor)
                {
                    // 任意键入回到实时底部；清掉旧选区。
                    if let Some(w) = self.active() {
                        w.grid.reset_view();
                    }
                    self.selection = None;
                    self.write_active(&bytes);
                    // 更新输入跟踪缓冲 + 重算补全浮层。
                    self.update_input_buffer(&event.logical_key, event.text.as_deref());
                    self.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                if self.selecting {
                    if let Some(r) = self.renderer.as_ref() {
                        let head = r.cell_at(position.x, position.y);
                        if let Some(sel) = self.selection.as_mut() {
                            sel.head = head;
                        }
                    }
                    self.request_redraw();
                } else if self.mouse_down {
                    // 鼠标上报的拖动（按住移动）：ButtonEvent/AnyEvent 才发。
                    let proto = self.active().map(|w| w.grid.modes.mouse_proto);
                    if matches!(proto, Some(MouseProto::ButtonEvent) | Some(MouseProto::AnyEvent)) {
                        self.mouse_report(32, true); // 32 = 拖动标志位 + 左键
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button != MouseButton::Left {
                    return;
                }
                let pressed = state == ElementState::Pressed;
                self.mouse_down = pressed;
                let (x, y) = self.mouse_pos;

                // 侧边栏点击：窗口行切换；host 头新开窗口。
                if pressed {
                    if let Some(row) = self.renderer.as_ref().and_then(|r| r.sidebar_row_at(x, y)) {
                        let (_, targets) = self.build_sidebar();
                        match targets.get(row) {
                            Some(SidebarTarget::Win(c, w)) => self.switch_to(*c, *w),
                            Some(SidebarTarget::Host(c)) => self.new_tab_in(*c),
                            None => {}
                        }
                        return;
                    }
                }

                let mouse_mode = self
                    .active()
                    .map(|w| w.grid.modes.mouse_proto != MouseProto::None)
                    .unwrap_or(false);
                let force_select = self.mods.shift_key();

                if mouse_mode && !force_select {
                    self.mouse_report(0, pressed); // 0 = 左键
                } else if pressed {
                    // 开始本地选择。
                    if let Some(r) = self.renderer.as_ref() {
                        let cell = r.cell_at(x, y);
                        self.selection = Some(Selection { anchor: cell, head: cell });
                        self.selecting = true;
                    }
                    self.request_redraw();
                } else {
                    self.selecting = false;
                    // 纯点击（无拖动）落在 OSC 8 超链接上 → 打开。
                    if let Some(sel) = self.selection {
                        if sel.is_empty() {
                            let (col, row) = sel.anchor;
                            let uri = self.active().and_then(|w| w.grid.link_at(col, row));
                            if let Some(u) = uri {
                                open_url(&u);
                            }
                            self.selection = None;
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(PhysicalPosition { y, .. }) => y / 20.0,
                };
                if lines == 0.0 {
                    return; // 水平滚动 / 抖动归零，忽略
                }
                let step = lines.abs().round().max(1.0) as usize;
                let mouse_mode = self
                    .active()
                    .map(|w| w.grid.modes.mouse_proto != MouseProto::None)
                    .unwrap_or(false);
                let on_alt = self.active().map(|w| w.grid.on_alt()).unwrap_or(false);

                if mouse_mode || on_alt {
                    // 上报滚轮（64=上，65=下）；备用屏里也交给应用处理。
                    let btn = if lines > 0.0 { 64 } else { 65 };
                    for _ in 0..step.min(5) {
                        self.mouse_report(btn, true);
                    }
                } else {
                    if let Some(w) = self.active() {
                        if lines > 0.0 {
                            w.grid.scroll_view_up(step);
                        } else {
                            w.grid.scroll_view_down(step);
                        }
                    }
                    self.selection = None; // 视图滚动 → 选区作废
                    self.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // 先把布局喂给渲染器、快照侧边栏 + 偏好视图（避免与活动 grid 借用冲突），再渲染。
                self.apply_layout();
                let (items, _) = self.build_sidebar();
                let pref = self.pref_row.map(|_| self.build_pref_view());
                let compl = self.compl.as_ref().map(|c| crate::render::ComplView {
                    items: c.items.iter().map(|(d, _)| d.clone()).collect(),
                    selected: c.sel,
                    col: c.anchor.0,
                    row: c.anchor.1,
                });
                let selection = self.selection;
                let (conn_idx, win_id) = (self.conn_idx, self.win_id);
                if let Some(renderer) = self.renderer.as_mut() {
                    if let Some(win) = self
                        .manager
                        .connection_mut(conn_idx)
                        .and_then(|c| c.window_mut(win_id))
                    {
                        renderer.render(&win.grid, selection, &items, pref.as_ref(), compl.as_ref());
                    }
                }
            }

            _ => {}
        }
    }
}
