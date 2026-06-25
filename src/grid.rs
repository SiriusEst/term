//! 终端屏幕状态模型（grid）。
//!
//! 数据流闭环里「状态模型」那一格：`vte` 解析出的动作（见 parser.rs）落到这里，
//! 渲染层（render.rs）再把它画到窗口。
//!
//! 覆盖：主屏 + **备用屏(1049)** + **scrollback** + 光标 + 画笔（含下划线/斜体/暗淡/删除线）
//! + **滚动区域(DECSTBM)** + 保存/恢复光标(DECSC/DECRC) + 行/字符的插入删除(IL/DL/ICH/DCH/ECH)
//! + **DEC 私有模式**（光标显隐/自动换行/应用光标键/鼠标/焦点/同步输出/括号粘贴）
//! + 一个 **事件队列**（标题 / 剪贴板(OSC52) / 响铃 / 通知(OSC9,777) / 提示符标记(OSC133)），
//!   由 App 在 feed 之后取走，落到平台（剪贴板、桌面通知、tab 状态）。

use std::collections::VecDeque;

/// 单元格颜色。`Default` = 跟随主题前景/背景；其余按 SGR 落具体值。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Color {
    Default,
    /// xterm 256 色索引（0–15 基本色，16–231 立方，232–255 灰阶）。
    Indexed(u8),
    /// 24-bit truecolor。
    Rgb(u8, u8, u8),
}

// 主题（含 16 色调色板 + 内置方案）在 theme 模块；re-export 让既有 `crate::grid::Theme` 引用不变。
pub use crate::theme::Theme;

impl Color {
    /// 解析成 RGB。`is_fg` 决定 `Default` 取主题的前景还是背景。
    /// 索引 0–15 走主题调色板（换方案即生效），16–255 按 xterm 立方/灰阶算。
    pub fn to_rgb(self, theme: &Theme, is_fg: bool) -> [u8; 3] {
        match self {
            Color::Default => {
                if is_fg {
                    theme.fg
                } else {
                    theme.bg
                }
            }
            Color::Rgb(r, g, b) => [r, g, b],
            Color::Indexed(i) if i < 16 => theme.ansi[i as usize],
            Color::Indexed(i) => indexed_to_rgb(i),
        }
    }
}

/// xterm 256 色索引 → RGB。
fn indexed_to_rgb(i: u8) -> [u8; 3] {
    // 0–15：标准 16 色。
    const BASE: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xCD, 0x00, 0x00],
        [0x00, 0xCD, 0x00],
        [0xCD, 0xCD, 0x00],
        [0x00, 0x00, 0xEE],
        [0xCD, 0x00, 0xCD],
        [0x00, 0xCD, 0xCD],
        [0xE5, 0xE5, 0xE5],
        [0x7F, 0x7F, 0x7F],
        [0xFF, 0x00, 0x00],
        [0x00, 0xFF, 0x00],
        [0xFF, 0xFF, 0x00],
        [0x5C, 0x5C, 0xFF],
        [0xFF, 0x00, 0xFF],
        [0x00, 0xFF, 0xFF],
        [0xFF, 0xFF, 0xFF],
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            // 6×6×6 立方。
            let x = i - 16;
            let r = x / 36;
            let g = (x / 6) % 6;
            let b = x % 6;
            let conv = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + 40 * v
                }
            };
            [conv(r), conv(g), conv(b)]
        }
        _ => {
            // 232–255：24 级灰阶。
            let v = 8 + 10 * (i - 232);
            [v, v, v]
        }
    }
}

/// 单元格：一个字符 + 前景/背景 + 属性。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strike: bool,
    pub hidden: bool,
    /// 宽字符（CJK/emoji）的右半占位格：不渲染字形，仅占位。
    pub wide_spacer: bool,
    /// OSC 8 超链接 id（0 = 无）。仅用于点击/下划线提示，渲染层可忽略。
    pub link: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            strike: false,
            hidden: false,
            wide_spacer: false,
            link: 0,
        }
    }
}

impl Cell {
    /// 渲染用的有效 (前景, 背景)：把 `inverse` 与外部传入的 `extra_inverse`（光标/选区）
    /// 一并折叠成最终颜色对。bold 把 0–7 基本色提亮到 8–15；dim 压暗前景；hidden 让前景=背景。
    pub fn effective_colors(&self, theme: &Theme, extra_inverse: bool) -> ([u8; 3], [u8; 3]) {
        let mut fg = self.fg;
        if self.bold {
            if let Color::Indexed(i @ 0..=7) = fg {
                fg = Color::Indexed(i + 8);
            }
        }
        let mut fg_rgb = fg.to_rgb(theme, true);
        let mut bg_rgb = self.bg.to_rgb(theme, false);
        if self.dim {
            fg_rgb = [fg_rgb[0] / 2, fg_rgb[1] / 2, fg_rgb[2] / 2];
        }
        if self.hidden {
            fg_rgb = bg_rgb;
        }
        if self.inverse ^ extra_inverse {
            std::mem::swap(&mut fg_rgb, &mut bg_rgb);
        }
        (fg_rgb, bg_rgb)
    }
}

/// 鼠标拖选的选区（可见坐标，线性选择）。`anchor` = 起点，`head` = 当前点。
#[derive(Clone, Copy)]
pub struct Selection {
    pub anchor: (usize, usize), // (col, row)
    pub head: (usize, usize),
}

impl Selection {
    /// 规整成 (start, end)，按行优先排序。
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor.1, self.anchor.0);
        let b = (self.head.1, self.head.0);
        if a <= b {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
    /// 可见坐标 (col,row) 是否落在选区内（线性选择语义）。
    pub fn contains(&self, col: usize, row: usize) -> bool {
        let (s, e) = self.ordered();
        if row < s.1 || row > e.1 {
            return false;
        }
        let left = if row == s.1 { s.0 } else { 0 };
        let right = if row == e.1 { e.0 } else { usize::MAX };
        col >= left && col <= right
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// 鼠标上报协议（DECSET 1000/1002/1003 选定模式，1006 决定 SGR 编码）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseProto {
    #[default]
    None,
    /// 1000：仅按下/抬起。
    Normal,
    /// 1002：按下/抬起 + 拖动（按住移动）。
    ButtonEvent,
    /// 1003：所有移动都上报。
    AnyEvent,
}

/// DEC 私有模式集合（DECSET/DECRST，`CSI ? Pm h/l`）。
#[derive(Clone, Copy)]
pub struct Modes {
    pub cursor_visible: bool, // ?25
    pub autowrap: bool,       // ?7（DECAWM）
    pub app_cursor_keys: bool, // ?1（DECCKM）
    pub app_keypad: bool,     // DECKPAM/DECKPNM（ESC =/>）
    pub bracketed_paste: bool, // ?2004
    pub focus_event: bool,    // ?1004
    pub sync_output: bool,    // ?2026（同步输出/原子刷新）
    pub mouse_proto: MouseProto, // 1000/1002/1003
    pub mouse_sgr: bool,      // 1006
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            cursor_visible: true,
            autowrap: true,
            app_cursor_keys: false,
            app_keypad: false,
            bracketed_paste: false,
            focus_event: false,
            sync_output: false,
            mouse_proto: MouseProto::None,
            mouse_sgr: false,
        }
    }
}

/// 从解析层冒泡给 App 的事件（feed 之后由 App 取走落到平台）。
#[derive(Clone, Debug)]
pub enum TermEvent {
    /// OSC 0/2：设置窗口标题。
    Title(String),
    /// OSC 52：应用要把这段文本写进系统剪贴板。
    SetClipboard(String),
    /// BEL：响铃 → 完成提示（失焦时弹通知）。
    Bell,
    /// OSC 9 / OSC 777：桌面通知（title, body）。
    Notify(String, String),
    /// OSC 7：远端当前工作目录（file:// URL）。
    Cwd(String),
    /// OSC 133 提示符标记：命令开始（C）。
    PromptStart,
    /// OSC 133：命令结束（D），带退出码（若有）。
    CommandEnd(Option<i32>),
    /// 需要回写给 PTY 的应答字节（DA 设备属性 / DSR 光标位置等）。
    Reply(Vec<u8>),
}

/// DECSC/DECRC 保存的光标状态。
#[derive(Clone, Copy)]
struct SavedCursor {
    cx: usize,
    cy: usize,
    pen: Pen,
    wrap_pending: bool,
}

/// 当前画笔（后续 print 的属性）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    strike: bool,
    hidden: bool,
    link: u16,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            strike: false,
            hidden: false,
            link: 0,
        }
    }
}

/// 进入备用屏(1049)时暂存的主屏，用于离开时还原。
struct AltSaved {
    cells: Vec<Cell>,
    cx: usize,
    cy: usize,
    saved_cursor: Option<SavedCursor>,
}

/// 字符网格 + 光标 + 画笔 + 模式 + 滚动区域 + 备用屏 + scrollback。
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>, // 行优先，长度 = rows*cols（当前活动屏）
    pub cx: usize,    // 光标列
    pub cy: usize,    // 光标行
    pen: Pen,
    pub modes: Modes,
    // 滚动区域（0-based，闭区间）。默认整屏 0..=rows-1。
    scroll_top: usize,
    scroll_bottom: usize,
    saved_cursor: Option<SavedCursor>,
    // 备用屏：Some = 当前在备用屏，内含暂存的主屏。
    alt: Option<AltSaved>,
    // scrollback（仅主屏，VecDeque of 行）。
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_max: usize,
    /// 向上回看的偏移（0 = 看实时底部）。供 render 与选区取字用。
    pub view_offset: usize,
    // 行尾延迟换行（DEC 兼容：写满最后一列时光标停在列尾，下一个可见字符才换行）。
    wrap_pending: bool,
    // 事件队列：解析层 push，App drain。
    events: Vec<TermEvent>,
    // 当前 OSC 8 链接 id（0 = 无）。
    cur_link: u16,
    // OSC 8 超链接表（1-based id → URI）。
    link_table: Vec<String>,
    // DCS passthrough 累积缓冲（tmux 把 OSC 52 等包进 DCS）。
    dcs_buf: Vec<u8>,
    dcs_action: char,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cx: 0,
            cy: 0,
            pen: Pen::default(),
            modes: Modes::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            saved_cursor: None,
            alt: None,
            scrollback: VecDeque::new(),
            scrollback_max: 5000,
            view_offset: 0,
            wrap_pending: false,
            events: Vec::new(),
            cur_link: 0,
            link_table: Vec::new(),
            dcs_buf: Vec::new(),
            dcs_action: '\0',
        }
    }

    // ---- OSC 8 超链接表 ----

    /// 注册（或复用）一个超链接 URI，返回 1-based id。
    pub fn register_link(&mut self, uri: &str) -> u16 {
        if uri.is_empty() {
            return 0;
        }
        if let Some(pos) = self.link_table.iter().position(|u| u == uri) {
            return (pos + 1) as u16;
        }
        self.link_table.push(uri.to_string());
        self.link_table.len() as u16
    }
    /// 按 id 查 URI（供点击打开 / 渲染提示）。
    pub fn link_uri(&self, id: u16) -> Option<&str> {
        if id == 0 {
            return None;
        }
        self.link_table.get(id as usize - 1).map(|s| s.as_str())
    }
    /// 可见坐标处单元格的 OSC 8 超链接 URI（点击打开用）。
    pub fn link_at(&self, col: usize, row: usize) -> Option<String> {
        let id = self.visible_cell(col, row).link;
        self.link_uri(id).map(|s| s.to_string())
    }

    // ---- DCS passthrough（tmux 把 OSC 52 等包进 `DCS tmux; ... ST`）----

    pub fn dcs_start(&mut self, action: char) {
        self.dcs_action = action;
        self.dcs_buf.clear();
    }
    pub fn dcs_put(&mut self, byte: u8) {
        if self.dcs_buf.len() < (1 << 20) {
            self.dcs_buf.push(byte);
        }
    }
    /// DCS 结束：若是 tmux passthrough，返回**解包后的内层字节**供重新解析；否则 None。
    pub fn dcs_end(&mut self) -> Option<Vec<u8>> {
        let mut full = Vec::with_capacity(self.dcs_buf.len() + 1);
        if self.dcs_action.is_ascii() {
            full.push(self.dcs_action as u8);
        }
        full.extend_from_slice(&self.dcs_buf);
        self.dcs_buf.clear();
        self.dcs_action = '\0';
        if let Some(inner) = full.strip_prefix(b"tmux;") {
            // tmux 把内层每个 ESC(0x1b) 翻倍，这里还原。
            let mut out = Vec::with_capacity(inner.len());
            let mut i = 0;
            while i < inner.len() {
                if inner[i] == 0x1b && inner.get(i + 1) == Some(&0x1b) {
                    out.push(0x1b);
                    i += 2;
                } else {
                    out.push(inner[i]);
                    i += 1;
                }
            }
            Some(out)
        } else {
            None
        }
    }

    // ---- 事件队列 ----

    pub fn push_event(&mut self, ev: TermEvent) {
        self.events.push(ev);
    }
    pub fn drain_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.events)
    }
    pub fn on_alt(&self) -> bool {
        self.alt.is_some()
    }

    #[inline]
    fn idx(&self, col: usize, row: usize) -> usize {
        row * self.cols + col
    }

    /// 只读访问活动屏某格（越界返回默认空格）。
    pub fn cell(&self, col: usize, row: usize) -> Cell {
        if col < self.cols && row < self.rows {
            self.cells[self.idx(col, row)]
        } else {
            Cell::default()
        }
    }

    /// 考虑 scrollback 回看偏移后，可见区域第 `row` 行第 `col` 列的格子。
    /// `view_offset` 越大看得越靠上；偏移覆盖到的行取自 scrollback。
    pub fn visible_cell(&self, col: usize, row: usize) -> Cell {
        if self.view_offset == 0 {
            return self.cell(col, row);
        }
        // 可见窗口顶端在「实时顶端」之上 view_offset 行。
        // 把可见行号映射到 [scrollback || 活动屏] 的全局行。
        let total_back = self.scrollback.len();
        let global = row as isize + total_back as isize - self.view_offset as isize;
        if global < 0 {
            return Cell::default();
        }
        let global = global as usize;
        if global < total_back {
            // 落在 scrollback 里。
            let line = &self.scrollback[global];
            line.get(col).copied().unwrap_or_default()
        } else {
            let r = global - total_back;
            self.cell(col, r)
        }
    }

    /// 设置 scrollback 上限（配置）。
    pub fn set_scrollback_max(&mut self, n: usize) {
        self.scrollback_max = n.max(1);
        while self.scrollback.len() > self.scrollback_max {
            self.scrollback.pop_front();
        }
    }
    pub fn scroll_view_up(&mut self, n: usize) {
        if self.on_alt() {
            return; // 备用屏无 scrollback
        }
        self.view_offset = (self.view_offset + n).min(self.scrollback.len());
    }
    pub fn scroll_view_down(&mut self, n: usize) {
        self.view_offset = self.view_offset.saturating_sub(n);
    }
    pub fn reset_view(&mut self) {
        self.view_offset = 0;
    }

    /// 改变网格尺寸：保留左上角已有内容，光标夹到新范围内，滚动区域复位整屏。
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cells = reflow_keep_topleft(&self.cells, self.cols, self.rows, cols, rows);
        if let Some(alt) = self.alt.as_mut() {
            alt.cells = reflow_keep_topleft(&alt.cells, self.cols, self.rows, cols, rows);
        }
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.wrap_pending = false;
        self.view_offset = 0;
    }

    // ---- 画笔（SGR）----

    pub fn reset_pen(&mut self) {
        self.pen = Pen {
            link: self.cur_link,
            ..Pen::default()
        };
    }
    pub fn set_fg(&mut self, c: Color) {
        self.pen.fg = c;
    }
    pub fn set_bg(&mut self, c: Color) {
        self.pen.bg = c;
    }
    pub fn set_bold(&mut self, on: bool) {
        self.pen.bold = on;
    }
    pub fn set_dim(&mut self, on: bool) {
        self.pen.dim = on;
    }
    pub fn set_italic(&mut self, on: bool) {
        self.pen.italic = on;
    }
    pub fn set_underline(&mut self, on: bool) {
        self.pen.underline = on;
    }
    pub fn set_inverse(&mut self, on: bool) {
        self.pen.inverse = on;
    }
    pub fn set_strike(&mut self, on: bool) {
        self.pen.strike = on;
    }
    pub fn set_hidden(&mut self, on: bool) {
        self.pen.hidden = on;
    }
    /// 设置 / 清除当前 OSC 8 超链接 id。
    pub fn set_link(&mut self, link: u16) {
        self.cur_link = link;
        self.pen.link = link;
    }

    fn pen_cell(&self, c: char) -> Cell {
        Cell {
            c,
            fg: self.pen.fg,
            bg: self.pen.bg,
            bold: self.pen.bold,
            dim: self.pen.dim,
            italic: self.pen.italic,
            underline: self.pen.underline,
            inverse: self.pen.inverse,
            strike: self.pen.strike,
            hidden: self.pen.hidden,
            wide_spacer: false,
            link: self.pen.link,
        }
    }

    // ---- 写入与光标 ----

    /// 打印一个可见字符到光标处，并前移光标（带行尾延迟换行、宽字符占位）。
    pub fn print(&mut self, c: char) {
        self.reset_view();
        let w = char_width(c);
        // 组合字符（宽 0）：单字符单元格放不下，丢弃以免把后续内容整体右推。
        // （已知限制：grapheme 簇组合需要「每格存字符串」，留作后续；多数文本是 NFC 预组合。）
        if w == 0 {
            return;
        }
        if self.wrap_pending {
            if self.modes.autowrap {
                self.cx = 0;
                self.index(); // 换到下一行（受滚动区域约束）
            }
            self.wrap_pending = false;
        }
        // 宽字符需要两列，行尾放不下则先换行。
        if w == 2 && self.cx + 1 >= self.cols {
            if self.modes.autowrap {
                self.cx = 0;
                self.index();
            } else {
                return; // 不换行则丢弃尾部宽字符
            }
        }
        let (col, row) = (self.cx, self.cy);
        // 覆盖到某个宽字符的一半时，先清掉它的另一半，避免残留半个字形。
        self.clear_wide_partner(col, row);
        if w == 2 {
            self.clear_wide_partner(col + 1, row);
        }
        let i = self.idx(col, row);
        self.cells[i] = self.pen_cell(c);
        if w == 2 && col + 1 < self.cols {
            let mut spacer = self.pen_cell(' ');
            spacer.wide_spacer = true;
            let j = self.idx(col + 1, row);
            self.cells[j] = spacer;
        }
        let adv = w;
        if self.cx + adv >= self.cols {
            self.cx = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cx += adv;
        }
    }

    /// 若 (col,row) 是某宽字符的一半，把它的另一半清成空格，避免残留半个字形。
    fn clear_wide_partner(&mut self, col: usize, row: usize) {
        if col >= self.cols {
            return;
        }
        let cell = self.cells[self.idx(col, row)];
        if cell.wide_spacer && col > 0 {
            let lead = self.idx(col - 1, row);
            self.cells[lead] = Cell::default();
        } else if char_width(cell.c) == 2 && col + 1 < self.cols {
            let sp = self.idx(col + 1, row);
            if self.cells[sp].wide_spacer {
                self.cells[sp] = Cell::default();
            }
        }
    }

    /// IND（向下换行，受滚动区域约束）：在区域底部则区域上滚一行。
    pub fn index(&mut self) {
        self.wrap_pending = false;
        if self.cy == self.scroll_bottom {
            self.scroll_up_region(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    /// LF（换行）：等同 IND（不含 CR）。
    pub fn linefeed(&mut self) {
        self.reset_view();
        self.index();
    }

    /// RI（ESC M，反向换行）：在区域顶部则区域下滚一行。
    pub fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cy == self.scroll_top {
            self.scroll_down_region(1);
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    /// 回车（CR）：光标回到行首。
    pub fn carriage_return(&mut self) {
        self.wrap_pending = false;
        self.cx = 0;
    }

    /// 退格（BS）：光标左移一格（不擦除）。
    pub fn backspace(&mut self) {
        self.wrap_pending = false;
        if self.cx > 0 {
            self.cx -= 1;
        }
    }

    /// 制表（HT）：移到下一个 8 列制表位。
    pub fn tab(&mut self) {
        self.wrap_pending = false;
        let next = ((self.cx / 8) + 1) * 8;
        self.cx = next.min(self.cols - 1);
    }

    /// 区域内上滚 n 行：顶部行（若区域顶=0 且在主屏）进 scrollback，底部补空行。
    fn scroll_up_region(&mut self, n: usize) {
        let n = n.min(self.scroll_bottom - self.scroll_top + 1);
        let to_scrollback = self.scroll_top == 0 && !self.on_alt();
        for _ in 0..n {
            if to_scrollback {
                let top_row: Vec<Cell> =
                    self.cells[self.idx(0, self.scroll_top)..self.idx(0, self.scroll_top) + self.cols]
                        .to_vec();
                self.scrollback.push_back(top_row);
                while self.scrollback.len() > self.scrollback_max {
                    self.scrollback.pop_front();
                }
            }
            // 区域内整体上移一行。
            for r in self.scroll_top..self.scroll_bottom {
                let (dst0, src0) = (self.idx(0, r), self.idx(0, r + 1));
                self.cells.copy_within(src0..src0 + self.cols, dst0);
            }
            let blank = self.blank();
            let start = self.idx(0, self.scroll_bottom);
            for cell in &mut self.cells[start..start + self.cols] {
                *cell = blank;
            }
        }
    }

    /// 区域内下滚 n 行：底部行移出，顶部补空行。
    fn scroll_down_region(&mut self, n: usize) {
        let n = n.min(self.scroll_bottom - self.scroll_top + 1);
        let blank = self.blank();
        for _ in 0..n {
            let mut r = self.scroll_bottom;
            while r > self.scroll_top {
                let (dst0, src0) = (self.idx(0, r), self.idx(0, r - 1));
                self.cells.copy_within(src0..src0 + self.cols, dst0);
                r -= 1;
            }
            let start = self.idx(0, self.scroll_top);
            for cell in &mut self.cells[start..start + self.cols] {
                *cell = blank;
            }
        }
    }

    /// SU（CSI Ps S）：整区域上滚。
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_up_region(n.max(1));
    }
    /// SD（CSI Ps T）：整区域下滚。
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_down_region(n.max(1));
    }

    /// DECSTBM：设滚动区域（外部传 0-based 闭区间），并把光标移到左上。
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows - 1);
        let bottom = bottom.min(self.rows - 1);
        // 退化区域（top>=bottom）按 xterm 直接忽略，不动光标也不改区域。
        if top >= bottom {
            return;
        }
        self.scroll_top = top;
        self.scroll_bottom = bottom;
        self.cx = 0;
        self.cy = self.scroll_top;
        self.wrap_pending = false;
    }

    // ---- 光标移动 ----

    /// 绝对定位（CUP/HVP，外部传 0-based）。
    pub fn move_to(&mut self, col: usize, row: usize) {
        self.wrap_pending = false;
        self.cx = col.min(self.cols - 1);
        self.cy = row.min(self.rows - 1);
    }
    pub fn move_up(&mut self, n: usize) {
        self.wrap_pending = false;
        // CUU 只在光标处于滚动区域内时以区域顶为下界；区域外则以屏顶为界。
        let target = self.cy.saturating_sub(n.max(1));
        self.cy = if self.cy >= self.scroll_top {
            target.max(self.scroll_top)
        } else {
            target
        };
    }
    pub fn move_down(&mut self, n: usize) {
        self.wrap_pending = false;
        let target = (self.cy + n.max(1)).min(self.rows - 1);
        self.cy = if self.cy <= self.scroll_bottom {
            target.min(self.scroll_bottom)
        } else {
            target
        };
    }
    pub fn move_left(&mut self, n: usize) {
        self.wrap_pending = false;
        self.cx = self.cx.saturating_sub(n.max(1));
    }
    pub fn move_right(&mut self, n: usize) {
        self.wrap_pending = false;
        self.cx = (self.cx + n.max(1)).min(self.cols - 1);
    }
    /// 设光标列（CHA，0-based）。
    pub fn move_to_col(&mut self, col: usize) {
        self.wrap_pending = false;
        self.cx = col.min(self.cols - 1);
    }
    /// 设光标行（VPA，0-based），列不变。
    pub fn move_to_row(&mut self, row: usize) {
        self.wrap_pending = false;
        self.cy = row.min(self.rows - 1);
    }

    // ---- DECSC / DECRC ----

    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            cx: self.cx,
            cy: self.cy,
            pen: self.pen,
            wrap_pending: self.wrap_pending,
        });
    }
    pub fn restore_cursor(&mut self) {
        if let Some(s) = self.saved_cursor {
            self.cx = s.cx.min(self.cols - 1);
            self.cy = s.cy.min(self.rows - 1);
            self.pen = s.pen;
            self.wrap_pending = s.wrap_pending;
        }
    }

    // ---- 行 / 字符的插入删除 ----

    /// ICH（CSI Ps @）：在光标处插入 n 个空格，行内右侧右移。
    pub fn insert_chars(&mut self, n: usize) {
        let n = n.max(1).min(self.cols - self.cx);
        let row = self.cy;
        let start = self.idx(0, row);
        let blank = self.blank();
        for c in (self.cx..self.cols).rev() {
            if c >= self.cx + n {
                self.cells[start + c] = self.cells[start + c - n];
            } else {
                self.cells[start + c] = blank;
            }
        }
    }

    /// DCH（CSI Ps P）：删除光标处 n 个字符，右侧左移，尾部补空。
    pub fn delete_chars(&mut self, n: usize) {
        let n = n.max(1).min(self.cols - self.cx);
        let row = self.cy;
        let start = self.idx(0, row);
        let blank = self.blank();
        for c in self.cx..self.cols {
            if c + n < self.cols {
                self.cells[start + c] = self.cells[start + c + n];
            } else {
                self.cells[start + c] = blank;
            }
        }
    }

    /// ECH（CSI Ps X）：从光标起擦除 n 个字符（不移动右侧）。
    pub fn erase_chars(&mut self, n: usize) {
        let n = n.max(1).min(self.cols - self.cx);
        let row = self.cy;
        let start = self.idx(0, row);
        let blank = self.blank();
        for c in self.cx..self.cx + n {
            self.cells[start + c] = blank;
        }
    }

    /// IL（CSI Ps L）：在光标行插入 n 行（仅在滚动区域内），区域下部下移。
    pub fn insert_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bottom {
            return;
        }
        self.cx = 0; // IL/DL 把光标移到左边距
        self.wrap_pending = false;
        let n = n.max(1).min(self.scroll_bottom - self.cy + 1);
        let blank = self.blank();
        let mut r = self.scroll_bottom;
        while r >= self.cy + n {
            let (dst0, src0) = (self.idx(0, r), self.idx(0, r - n));
            self.cells.copy_within(src0..src0 + self.cols, dst0);
            if r == 0 {
                break;
            }
            r -= 1;
        }
        for r in self.cy..(self.cy + n).min(self.scroll_bottom + 1) {
            let start = self.idx(0, r);
            for cell in &mut self.cells[start..start + self.cols] {
                *cell = blank;
            }
        }
    }

    /// DL（CSI Ps M）：删除光标行起 n 行（仅在滚动区域内），区域下部上移。
    pub fn delete_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bottom {
            return;
        }
        self.cx = 0; // IL/DL 把光标移到左边距
        self.wrap_pending = false;
        let n = n.max(1).min(self.scroll_bottom - self.cy + 1);
        let blank = self.blank();
        for r in self.cy..=self.scroll_bottom {
            if r + n <= self.scroll_bottom {
                let (dst0, src0) = (self.idx(0, r), self.idx(0, r + n));
                self.cells.copy_within(src0..src0 + self.cols, dst0);
            } else {
                let start = self.idx(0, r);
                for cell in &mut self.cells[start..start + self.cols] {
                    *cell = blank;
                }
            }
        }
    }

    // ---- 擦除 ----

    fn blank(&self) -> Cell {
        // 擦除用当前背景（这样 reverse/着色背景能正确清屏），但不带字符属性/链接。
        Cell {
            c: ' ',
            fg: self.pen.fg,
            bg: self.pen.bg,
            ..Cell::default()
        }
    }

    /// ED：擦除显示。mode 0=光标到屏末，1=屏首到光标，2/3=整屏。
    pub fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank();
        let cursor = self.idx(self.cx, self.cy);
        let total = self.cells.len();
        match mode {
            0 => {
                for cell in &mut self.cells[cursor..total] {
                    *cell = blank;
                }
            }
            1 => {
                for cell in &mut self.cells[0..=cursor.min(total - 1)] {
                    *cell = blank;
                }
            }
            _ => {
                for cell in &mut self.cells[..] {
                    *cell = blank;
                }
            }
        }
    }

    /// EL：擦除行。mode 0=光标到行尾，1=行首到光标，2=整行。
    pub fn erase_in_line(&mut self, mode: u16) {
        let blank = self.blank();
        let row_start = self.idx(0, self.cy);
        let (from, to) = match mode {
            0 => (row_start + self.cx, row_start + self.cols),
            1 => (row_start, row_start + self.cx + 1),
            _ => (row_start, row_start + self.cols),
        };
        let len = self.cells.len();
        for cell in &mut self.cells[from..to.min(len)] {
            *cell = blank;
        }
    }

    // ---- 备用屏（1049）----

    /// 进入备用屏：暂存主屏（含光标 + DECSC），清空备用屏，光标归位。
    pub fn enter_alt_screen(&mut self) {
        if self.on_alt() {
            return;
        }
        let saved = AltSaved {
            cells: std::mem::replace(&mut self.cells, vec![Cell::default(); self.cols * self.rows]),
            cx: self.cx,
            cy: self.cy,
            saved_cursor: self.saved_cursor.take(),
        };
        self.alt = Some(saved);
        self.cx = 0;
        self.cy = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.wrap_pending = false;
        self.view_offset = 0;
    }

    /// 离开备用屏：还原主屏（含光标 + DECSC）。
    pub fn leave_alt_screen(&mut self) {
        if let Some(saved) = self.alt.take() {
            // 备用屏尺寸可能与还原时不一致（resize 已统一过尺寸），按当前尺寸裁剪。
            self.cells = fit_cells(&saved.cells, self.cols, self.rows);
            self.cx = saved.cx.min(self.cols - 1);
            self.cy = saved.cy.min(self.rows - 1);
            self.saved_cursor = saved.saved_cursor;
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
            self.wrap_pending = false;
            self.view_offset = 0;
        }
    }

    /// RIS（ESC c）：硬复位——回主屏、清屏、复位光标/画笔/模式/滚动区域（保留 scrollback）。
    pub fn full_reset(&mut self) {
        if self.on_alt() {
            self.leave_alt_screen();
        }
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.cx = 0;
        self.cy = 0;
        self.pen = Pen::default();
        self.cur_link = 0;
        self.modes = Modes::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.saved_cursor = None;
        self.wrap_pending = false;
        self.view_offset = 0;
    }

    // ---- 选区取字（复制）----

    /// 取可见区域 [start, end] 跨行选区的文本（线性选择：整行取到行尾，中间整行）。
    /// `start`/`end` 是 (col, row) 可见坐标，函数内部会规整先后顺序。每行去掉行尾空白。
    pub fn text_in_span(&self, a: (usize, usize), b: (usize, usize)) -> String {
        let (start, end) = if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) };
        let mut out = String::new();
        for row in start.1..=end.1 {
            let col_from = if row == start.1 { start.0 } else { 0 };
            let col_to = if row == end.1 { end.0 + 1 } else { self.cols };
            let col_to = col_to.min(self.cols);
            let mut line = String::new();
            let mut c = col_from;
            while c < col_to {
                let cell = self.visible_cell(c, row);
                if cell.wide_spacer {
                    c += 1;
                    continue;
                }
                line.push(cell.c);
                c += 1;
            }
            // 去掉行尾空白。
            let trimmed = line.trim_end_matches(' ');
            out.push_str(trimmed);
            if row != end.1 {
                out.push('\n');
            }
        }
        out
    }
}

/// 把 `src`（old_cols×old_rows）左上角内容搬到新尺寸缓冲（保左上、不重排折行）。
fn reflow_keep_topleft(
    src: &[Cell],
    old_cols: usize,
    old_rows: usize,
    cols: usize,
    rows: usize,
) -> Vec<Cell> {
    let mut next = vec![Cell::default(); cols * rows];
    let copy_rows = rows.min(old_rows);
    let copy_cols = cols.min(old_cols);
    for r in 0..copy_rows {
        for c in 0..copy_cols {
            next[r * cols + c] = src[r * old_cols + c];
        }
    }
    next
}

/// 把一块 cells 适配到 cols×rows（按行裁剪/补空），用于备用屏还原。
fn fit_cells(src: &[Cell], cols: usize, rows: usize) -> Vec<Cell> {
    if src.len() == cols * rows {
        return src.to_vec();
    }
    vec![Cell::default(); cols * rows]
}

/// 字符显示宽度（0 = 组合字符；2 = 宽字符 CJK/emoji；其余 1）。
pub fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(g: &mut Grid, s: &str) {
        for c in s.chars() {
            g.print(c);
        }
    }
    fn row_text(g: &Grid, row: usize) -> String {
        (0..g.cols)
            .map(|c| g.cell(c, row).c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn print_and_wrap() {
        let mut g = Grid::new(4, 3);
        put(&mut g, "abcdef"); // 自动换行：abcd / ef
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!(row_text(&g, 1), "ef");
    }

    #[test]
    fn linefeed_scrolls_into_scrollback() {
        let mut g = Grid::new(3, 2);
        put(&mut g, "aa");
        g.carriage_return();
        g.linefeed();
        put(&mut g, "bb");
        g.carriage_return();
        g.linefeed(); // 到底，顶行 "aa" 进 scrollback
        put(&mut g, "cc");
        assert_eq!(row_text(&g, 0), "bb");
        assert_eq!(row_text(&g, 1), "cc");
        g.scroll_view_up(1);
        assert_eq!(g.visible_cell(0, 0).c, 'a'); // 回看看到 "aa"
    }

    #[test]
    fn scroll_region_and_insert_delete_lines_no_panic() {
        let mut g = Grid::new(10, 6);
        g.set_scroll_region(1, 4); // 中间 4 行
        g.move_to(0, 2);
        g.insert_lines(2);
        g.delete_lines(3);
        g.insert_lines(100); // 越界夹断
        g.delete_lines(100);
        // 整屏滚动区域外不受影响 + 不 panic
        g.move_to(0, 5);
        g.insert_lines(1);
    }

    #[test]
    fn ich_dch_ech_bounds() {
        let mut g = Grid::new(5, 1);
        put(&mut g, "abcde");
        g.move_to(0, 0);
        g.insert_chars(2); // 插 2 空格，右移挤掉尾部 "de"
        assert_eq!(row_text(&g, 0), "  abc");
        g.move_to(0, 0);
        g.delete_chars(100); // 越界夹断，不 panic
        g.erase_chars(100);
    }

    #[test]
    fn wide_char_occupies_two_cells() {
        let mut g = Grid::new(6, 1);
        put(&mut g, "中a");
        assert_eq!(g.cell(0, 0).c, '中');
        assert!(g.cell(1, 0).wide_spacer);
        assert_eq!(g.cell(2, 0).c, 'a');
    }

    #[test]
    fn alt_screen_roundtrip() {
        let mut g = Grid::new(4, 2);
        put(&mut g, "main");
        g.enter_alt_screen();
        assert!(g.on_alt());
        put(&mut g, "alt");
        assert_eq!(row_text(&g, 0), "alt");
        g.leave_alt_screen();
        assert!(!g.on_alt());
        assert_eq!(row_text(&g, 0), "main"); // 主屏还原
    }

    #[test]
    fn selection_text_multiline() {
        let mut g = Grid::new(5, 2);
        put(&mut g, "hello");
        g.carriage_return();
        g.linefeed();
        put(&mut g, "world");
        let t = g.text_in_span((0, 0), (4, 1));
        assert_eq!(t, "hello\nworld");
    }

    #[test]
    fn reverse_index_at_top_scrolls_down() {
        let mut g = Grid::new(3, 3);
        put(&mut g, "aa");
        g.move_to(0, 0);
        g.reverse_index(); // 顶部 RI → 区域下滚，顶行变空
        assert_eq!(row_text(&g, 0), "");
        assert_eq!(row_text(&g, 1), "aa");
    }

    #[test]
    fn resize_keeps_topleft_no_panic() {
        let mut g = Grid::new(10, 5);
        put(&mut g, "hello world");
        g.resize(4, 2);
        g.resize(20, 10);
        g.resize(1, 1);
        // 不 panic 即通过
    }

    #[test]
    fn il_dl_move_cursor_to_col0() {
        let mut g = Grid::new(6, 4);
        g.move_to(3, 1);
        g.insert_lines(1);
        assert_eq!(g.cx, 0); // IL 把光标移到左边距
        g.move_to(3, 1);
        g.delete_lines(1);
        assert_eq!(g.cx, 0);
    }

    #[test]
    fn cursor_move_outside_region_not_misclamped() {
        let mut g = Grid::new(5, 10);
        g.set_scroll_region(2, 5); // 区域 = 行 2..=5
        g.move_to(0, 8); // 光标在区域下方
        g.move_up(1);
        assert_eq!(g.cy, 7); // 不被错误夹到 scroll_top(2)，正常上移一行
        g.move_to(0, 0); // 区域上方
        g.move_down(1);
        assert_eq!(g.cy, 1); // 不被错误夹到 scroll_bottom
    }

    #[test]
    fn degenerate_decstbm_ignored() {
        let mut g = Grid::new(5, 6);
        g.set_scroll_region(0, 5); // 先设全屏
        g.move_to(3, 3);
        g.set_scroll_region(4, 4); // 退化（top>=bottom）→ 忽略，不动光标
        assert_eq!((g.cx, g.cy), (3, 3));
    }

    #[test]
    fn combining_char_does_not_consume_cell() {
        let mut g = Grid::new(5, 1);
        put(&mut g, "e\u{0301}x"); // e + 组合尖音符 + x
        assert_eq!(g.cell(0, 0).c, 'e');
        assert_eq!(g.cell(1, 0).c, 'x'); // 组合符没占格，x 紧跟 e
    }

    #[test]
    fn overwrite_wide_clears_orphan_half() {
        let mut g = Grid::new(6, 1);
        put(&mut g, "中文"); // 占 4 列
        g.move_to(1, 0); // 落在「中」的右半占位
        put(&mut g, "a");
        assert!(!g.cell(0, 0).wide_spacer); // 「中」的左半被清，不残留半个字形
        assert_eq!(g.cell(0, 0).c, ' ');
        assert_eq!(g.cell(1, 0).c, 'a');
    }

    #[test]
    fn erase_display_and_line() {
        let mut g = Grid::new(4, 2);
        put(&mut g, "abcd");
        g.carriage_return();
        g.linefeed();
        put(&mut g, "efgh");
        g.move_to(2, 0);
        g.erase_in_line(0); // 光标到行尾
        assert_eq!(row_text(&g, 0), "ab");
        g.erase_in_display(2); // 整屏
        assert_eq!(row_text(&g, 1), "");
    }
}
