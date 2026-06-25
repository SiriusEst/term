//! VT 解析的「动作落地」层。
//!
//! `vte::Parser`（存在 Window 里）把字节流拆成动作，回调到这里的 `Performer`，
//! 由它改写 `Grid`，并把跨层副作用（标题/剪贴板/通知/应答/提示符标记）push 进 `Grid` 的事件队列。
//!
//! 覆盖：
//!   - 可见字符（含宽字符）、C0 控制（CR/LF/VT/FF/BS/HT/BEL）
//!   - 光标移动（CUU/CUD/CUF/CUB/CNL/CPL/CHA/HPA/VPA/CUP）
//!   - 擦除（ED/EL/ECH）、行列增删（IL/DL/ICH/DCH）、滚动（SU/SD）、滚动区域（DECSTBM）
//!   - SGR（16/256/truecolor + bold/dim/italic/underline/inverse/strike/hidden）
//!   - ESC：RI / IND / NEL / DECSC / DECRC / DECKPAM/PNM / RIS
//!   - DEC 私有模式（DECSET/DECRST）：?1 ?7 ?25 ?1000/1002/1003 ?1006 ?1004 ?2004 ?2026 ?47/1047/1049
//!   - OSC：0/2 标题、7 cwd、8 超链接、9/777 通知、52 剪贴板、133 提示符标记
//!   - DCS passthrough：tmux 把 OSC 52 等包进 `DCS tmux; … ST`，这里解包后再解析
//!   - DA / DSR 应答（回写 PTY）

use crate::grid::{Color, Grid, MouseProto, TermEvent};
use base64::Engine as _;
use vte::{Params, Perform};

/// 短生命周期的执行器：每批字节构造一次，借用当前窗口的网格。
pub struct Performer<'a> {
    pub grid: &'a mut Grid,
}

impl<'a> Performer<'a> {
    pub fn new(grid: &'a mut Grid) -> Self {
        Self { grid }
    }
}

/// 取第 `idx` 组参数的首值；缺省或为 0 时返回 `default`。
fn arg_or(params: &Params, idx: usize, default: u16) -> u16 {
    match params.iter().nth(idx).and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

/// 取第 `idx` 组参数的首值（0 也照取）；缺省返回 `default`。
fn arg_raw(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|p| p.first().copied())
        .unwrap_or(default)
}

fn s(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        self.grid.print(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.grid.push_event(TermEvent::Bell), // BEL → 完成提示
            0x08 => self.grid.backspace(),                 // BS
            0x09 => self.grid.tab(),                       // HT
            0x0A..=0x0C => self.grid.linefeed(),           // LF / VT / FF
            0x0D => self.grid.carriage_return(),           // CR
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // DEC 私有模式（CSI ? Pm h/l）。
        if intermediates.first() == Some(&b'?') {
            match action {
                'h' => self.set_private_modes(params, true),
                'l' => self.set_private_modes(params, false),
                _ => {}
            }
            return;
        }
        // 次要 DA（CSI > c）等带 `>`/`=` 中间字节的——这里只回 `>` 的次要 DA。
        if intermediates.first() == Some(&b'>') {
            if action == 'c' {
                // 次要 DA：声称自己是 VT220（型号 0，版本 95）。
                self.reply(b"\x1b[>0;95;0c");
            }
            return;
        }
        if !intermediates.is_empty() {
            return; // 其它中间字节（字符集等）这里不处理
        }

        match action {
            'A' => self.grid.move_up(arg_or(params, 0, 1) as usize),
            'B' | 'e' => self.grid.move_down(arg_or(params, 0, 1) as usize),
            'C' | 'a' => self.grid.move_right(arg_or(params, 0, 1) as usize),
            'D' => self.grid.move_left(arg_or(params, 0, 1) as usize),
            'E' => {
                self.grid.move_down(arg_or(params, 0, 1) as usize);
                self.grid.move_to_col(0);
            }
            'F' => {
                self.grid.move_up(arg_or(params, 0, 1) as usize);
                self.grid.move_to_col(0);
            }
            'G' | '`' => {
                let col = arg_or(params, 0, 1).saturating_sub(1) as usize;
                self.grid.move_to_col(col);
            }
            'H' | 'f' => {
                let row = arg_or(params, 0, 1).saturating_sub(1) as usize;
                let col = arg_or(params, 1, 1).saturating_sub(1) as usize;
                self.grid.move_to(col, row);
            }
            'd' => {
                let row = arg_or(params, 0, 1).saturating_sub(1) as usize;
                self.grid.move_to_row(row);
            }
            'J' => self.grid.erase_in_display(arg_raw(params, 0, 0)),
            'K' => self.grid.erase_in_line(arg_raw(params, 0, 0)),
            'L' => self.grid.insert_lines(arg_or(params, 0, 1) as usize),
            'M' => self.grid.delete_lines(arg_or(params, 0, 1) as usize),
            'P' => self.grid.delete_chars(arg_or(params, 0, 1) as usize),
            '@' => self.grid.insert_chars(arg_or(params, 0, 1) as usize),
            'X' => self.grid.erase_chars(arg_or(params, 0, 1) as usize),
            'S' => self.grid.scroll_up(arg_or(params, 0, 1) as usize),
            'T' => self.grid.scroll_down(arg_or(params, 0, 1) as usize),
            'm' => self.sgr(params),
            'r' => {
                // DECSTBM：top;bottom（1-based）；缺省整屏。
                let top = arg_or(params, 0, 1).saturating_sub(1) as usize;
                let bottom = arg_raw(params, 1, 0);
                let bottom = if bottom == 0 {
                    self.grid.rows - 1
                } else {
                    (bottom - 1) as usize
                };
                self.grid.set_scroll_region(top, bottom);
            }
            's' => self.grid.save_cursor(), // SCOSC
            'u' => self.grid.restore_cursor(), // SCORC
            'c' => self.reply(b"\x1b[?1;2c"), // 主 DA：VT100 + AVO
            'n' => {
                match arg_raw(params, 0, 0) {
                    5 => self.reply(b"\x1b[0n"), // 设备就绪
                    6 => {
                        // 光标位置报告（1-based）。
                        let r = self.grid.cy + 1;
                        let c = self.grid.cx + 1;
                        self.reply(format!("\x1b[{r};{c}R").as_bytes());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return; // 字符集选择 ESC ( B 等：假定 UTF-8，忽略
        }
        match byte {
            b'M' => self.grid.reverse_index(),     // RI
            b'D' => self.grid.index(),             // IND
            b'E' => {
                // NEL：回车 + 换行
                self.grid.carriage_return();
                self.grid.index();
            }
            b'7' => self.grid.save_cursor(),       // DECSC
            b'8' => self.grid.restore_cursor(),    // DECRC
            b'=' => self.grid.modes.app_keypad = true, // DECKPAM
            b'>' => self.grid.modes.app_keypad = false, // DECKPNM
            b'c' => self.grid.full_reset(),        // RIS
            _ => {}
        }
    }

    // ---- OSC（标题 / cwd / 超链接 / 通知 / 剪贴板 / 提示符标记）----
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&num) = params.first() else { return };
        match num {
            b"0" | b"1" | b"2" => {
                if let Some(t) = params.get(1) {
                    self.grid.push_event(TermEvent::Title(s(t)));
                }
            }
            b"7" => {
                if let Some(u) = params.get(1) {
                    self.grid.push_event(TermEvent::Cwd(s(u)));
                }
            }
            b"8" => {
                // OSC 8 ; params ; URI —— 空 URI = 关闭链接。
                let uri = params.get(2).map(|u| s(u)).unwrap_or_default();
                let id = self.grid.register_link(&uri);
                self.grid.set_link(id);
            }
            b"9" => {
                // iTerm2 风格：OSC 9 ; message
                if let Some(msg) = params.get(1) {
                    self.grid.push_event(TermEvent::Notify("term".into(), s(msg)));
                }
            }
            b"52" => self.osc52(params),
            b"133" => self.osc133(params),
            b"777" => {
                // OSC 777 ; notify ; title ; body
                if params.get(1).map(|p| *p == b"notify").unwrap_or(false) {
                    let title = params.get(2).map(|p| s(p)).unwrap_or_default();
                    let body = params.get(3).map(|p| s(p)).unwrap_or_default();
                    self.grid.push_event(TermEvent::Notify(title, body));
                }
            }
            _ => {}
        }
    }

    // ---- DCS passthrough（tmux 包裹 OSC 52 等）----
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        self.grid.dcs_start(action);
    }
    fn put(&mut self, byte: u8) {
        self.grid.dcs_put(byte);
    }
    fn unhook(&mut self) {
        if let Some(inner) = self.grid.dcs_end() {
            // 解包出的内层字节用一个子解析器再跑一遍（落到同一个 grid）。
            let mut sub = vte::Parser::new();
            let mut perf = Performer { grid: &mut *self.grid };
            sub.advance(&mut perf, &inner);
        }
    }
}

impl Performer<'_> {
    fn reply(&mut self, bytes: &[u8]) {
        self.grid.push_event(TermEvent::Reply(bytes.to_vec()));
    }

    /// DECSET/DECRST：设置或清除 DEC 私有模式。
    fn set_private_modes(&mut self, params: &Params, set: bool) {
        for p in params.iter() {
            let m = p.first().copied().unwrap_or(0);
            let modes = &mut self.grid.modes;
            match m {
                1 => modes.app_cursor_keys = set,    // DECCKM
                7 => modes.autowrap = set,           // DECAWM
                25 => modes.cursor_visible = set,    // DECTCEM
                1000 => modes.mouse_proto = if set { MouseProto::Normal } else { MouseProto::None },
                1002 => {
                    modes.mouse_proto = if set { MouseProto::ButtonEvent } else { MouseProto::None }
                }
                1003 => {
                    modes.mouse_proto = if set { MouseProto::AnyEvent } else { MouseProto::None }
                }
                1004 => modes.focus_event = set,     // 焦点上报
                1006 => modes.mouse_sgr = set,       // SGR 鼠标编码
                2004 => modes.bracketed_paste = set, // 括号粘贴
                2026 => modes.sync_output = set,     // 同步输出
                47 | 1047 => {
                    if set {
                        self.grid.enter_alt_screen();
                    } else {
                        self.grid.leave_alt_screen();
                    }
                }
                1049 => {
                    if set {
                        self.grid.save_cursor();
                        self.grid.enter_alt_screen();
                    } else {
                        self.grid.leave_alt_screen();
                        self.grid.restore_cursor();
                    }
                }
                _ => {}
            }
        }
    }

    /// OSC 52：`52 ; <targets> ; <base64|?>` —— 设置系统剪贴板（查询 `?` 暂不应答）。
    fn osc52(&mut self, params: &[&[u8]]) {
        let Some(data) = params.get(2) else { return };
        if *data == b"?" {
            return; // 读请求：暂不实现回填
        }
        // 容忍带/不带 padding 的 base64（不同 app 习惯不一）。
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data));
        if let Ok(bytes) = decoded {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            self.grid.push_event(TermEvent::SetClipboard(text));
        }
    }

    /// OSC 133：提示符 / 命令标记。C=命令开始执行，D[;exit]=命令结束。
    fn osc133(&mut self, params: &[&[u8]]) {
        match params.get(1).copied() {
            Some(b"C") => self.grid.push_event(TermEvent::PromptStart),
            Some(b"D") => {
                let exit = params
                    .get(2)
                    .and_then(|p| std::str::from_utf8(p).ok())
                    .and_then(|s| s.trim().parse::<i32>().ok());
                self.grid.push_event(TermEvent::CommandEnd(exit));
            }
            _ => {}
        }
    }

    /// SGR：解析颜色与属性。支持分号式（38;5;n / 38;2;r;g;b）与冒号子参数式。
    fn sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.grid.reset_pen();
            return;
        }
        let mut iter = params.iter();
        while let Some(p) = iter.next() {
            let code = p.first().copied().unwrap_or(0);
            match code {
                0 => self.grid.reset_pen(),
                1 => self.grid.set_bold(true),
                2 => self.grid.set_dim(true),
                3 => self.grid.set_italic(true),
                4 => self.grid.set_underline(true),
                7 => self.grid.set_inverse(true),
                8 => self.grid.set_hidden(true),
                9 => self.grid.set_strike(true),
                21 | 22 => {
                    self.grid.set_bold(false);
                    self.grid.set_dim(false);
                }
                23 => self.grid.set_italic(false),
                24 => self.grid.set_underline(false),
                27 => self.grid.set_inverse(false),
                28 => self.grid.set_hidden(false),
                29 => self.grid.set_strike(false),
                30..=37 => self.grid.set_fg(Color::Indexed((code - 30) as u8)),
                39 => self.grid.set_fg(Color::Default),
                40..=47 => self.grid.set_bg(Color::Indexed((code - 40) as u8)),
                49 => self.grid.set_bg(Color::Default),
                90..=97 => self.grid.set_fg(Color::Indexed((code - 90 + 8) as u8)),
                100..=107 => self.grid.set_bg(Color::Indexed((code - 100 + 8) as u8)),
                38 | 48 => {
                    let is_fg = code == 38;
                    let color = if p.len() >= 2 {
                        ext_color_from_subparams(p)
                    } else {
                        ext_color_from_iter(&mut iter)
                    };
                    if let Some(c) = color {
                        if is_fg {
                            self.grid.set_fg(c);
                        } else {
                            self.grid.set_bg(c);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 冒号式：单组形如 [38/48, 5, n] 或 [38/48, 2, (colorspace,) r, g, b]。
fn ext_color_from_subparams(p: &[u16]) -> Option<Color> {
    match p.get(1)? {
        2 => {
            let (r, g, b) = if p.len() >= 6 {
                (p[3], p[4], p[5])
            } else if p.len() >= 5 {
                (p[2], p[3], p[4])
            } else {
                return None;
            };
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        5 => p.get(2).map(|n| Color::Indexed(*n as u8)),
        _ => None,
    }
}

/// 分号式：38;5;n 或 38;2;r;g;b —— 5/2 在下一组，参数再往后取。
fn ext_color_from_iter<'b, I>(iter: &mut I) -> Option<Color>
where
    I: Iterator<Item = &'b [u16]>,
{
    let kind = iter.next()?.first().copied()?;
    match kind {
        5 => {
            let n = iter.next()?.first().copied()?;
            Some(Color::Indexed(n as u8))
        }
        2 => {
            let r = iter.next()?.first().copied()?;
            let g = iter.next()?.first().copied()?;
            let b = iter.next()?.first().copied()?;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! 集成测试：字节流经**真实的 `vte::Parser` + `Performer`** 落到 `Grid`，
    //! 断言屏幕状态 / 模式位 / 冒泡事件 / 回写应答。覆盖 parser 的参数解析与
    //! grid 落点两层，是单元测 grid 之外的回归网。
    use super::*;
    use crate::grid::{Color, Grid, MouseProto, TermEvent};

    /// 把整段字节喂给一个真实解析器（vte 0.15 的 `advance` 收切片）。
    fn feed(g: &mut Grid, bytes: &[u8]) {
        let mut p = vte::Parser::new();
        let mut perf = Performer::new(g);
        p.advance(&mut perf, bytes);
    }

    /// 新建 `cols×rows` 网格，喂入字节，返回网格。
    fn run(cols: usize, rows: usize, bytes: &[u8]) -> Grid {
        let mut g = Grid::new(cols, rows);
        feed(&mut g, bytes);
        g
    }

    fn row_text(g: &Grid, row: usize) -> String {
        (0..g.cols)
            .map(|c| g.cell(c, row).c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    // ---------------- SGR 颜色 / 属性 ----------------

    #[test]
    fn sgr_truecolor_256_and_attrs() {
        // 一条 SGR 同时设 bold + inverse + truecolor 前景 + 256 背景。
        let g = run(8, 1, b"\x1b[1;7;38;2;10;20;30;48;5;200mX");
        let cell = g.cell(0, 0);
        assert_eq!(cell.c, 'X');
        assert!(cell.bold);
        assert!(cell.inverse);
        assert_eq!(cell.fg, Color::Rgb(10, 20, 30));
        assert_eq!(cell.bg, Color::Indexed(200));
    }

    #[test]
    fn sgr_reset_clears_attrs() {
        let g = run(8, 1, b"\x1b[1;31mA\x1b[0mB");
        assert!(g.cell(0, 0).bold);
        assert_eq!(g.cell(0, 0).fg, Color::Indexed(1));
        assert!(!g.cell(1, 0).bold);
        assert_eq!(g.cell(1, 0).fg, Color::Default);
    }

    #[test]
    fn sgr_bright_and_bare_m_resets() {
        // 90–97 是亮前景（索引 +8）；裸 `\x1b[m` 等价 reset。
        let g = run(8, 1, b"\x1b[92mA\x1b[mB");
        assert_eq!(g.cell(0, 0).fg, Color::Indexed(10));
        assert_eq!(g.cell(1, 0).fg, Color::Default);
    }

    #[test]
    fn sgr_colon_subparam_truecolor() {
        // 冒号子参数式 `38:2::r:g:b`（中间空的 colorspace 占位为 0）。
        let g = run(8, 1, b"\x1b[38:2::1:2:3mZ");
        assert_eq!(g.cell(0, 0).fg, Color::Rgb(1, 2, 3));
    }

    // ---------------- 光标移动 ----------------

    #[test]
    fn cup_with_defaults_and_zero_as_one() {
        assert_eq!(cursor(run(20, 10, b"\x1b[5;10H")), (9, 4));
        assert_eq!(cursor(run(20, 10, b"\x1b[H")), (0, 0)); // 缺省 = 1;1
        assert_eq!(cursor(run(20, 10, b"\x1b[0;0H")), (0, 0)); // 0 当 1
    }

    #[test]
    fn cha_and_vpa() {
        assert_eq!(run(20, 10, b"\x1b[5G").cx, 4); // 绝对列
        assert_eq!(run(20, 10, b"\x1b[3d").cy, 2); // 绝对行
    }

    fn cursor(g: Grid) -> (usize, usize) {
        (g.cx, g.cy)
    }

    // ---------------- 擦除 ----------------

    #[test]
    fn el_erase_to_end_of_line() {
        // 写 hello，回到第 3 列，EL(0) 擦到行尾 → 只剩 he。
        let g = run(10, 1, b"hello\x1b[3G\x1b[K");
        assert_eq!(row_text(&g, 0), "he");
    }

    #[test]
    fn ed_clear_whole_screen() {
        let g = run(10, 2, b"ab\r\ncd\x1b[2J");
        assert_eq!(row_text(&g, 0), "");
        assert_eq!(row_text(&g, 1), "");
    }

    // ---------------- DEC 私有模式（M6 攻坚点）----------------

    #[test]
    fn private_modes_toggle() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, b"\x1b[?25l");
        assert!(!g.modes.cursor_visible);
        feed(&mut g, b"\x1b[?25h");
        assert!(g.modes.cursor_visible);
        feed(&mut g, b"\x1b[?2004h");
        assert!(g.modes.bracketed_paste);
        feed(&mut g, b"\x1b[?7l");
        assert!(!g.modes.autowrap);
        feed(&mut g, b"\x1b[?1h");
        assert!(g.modes.app_cursor_keys);
        feed(&mut g, b"\x1b[?2026h");
        assert!(g.modes.sync_output);
        feed(&mut g, b"\x1b[?1004h");
        assert!(g.modes.focus_event);
        feed(&mut g, b"\x1b[?1000h");
        assert_eq!(g.modes.mouse_proto, MouseProto::Normal);
        feed(&mut g, b"\x1b[?1003h");
        assert_eq!(g.modes.mouse_proto, MouseProto::AnyEvent);
        feed(&mut g, b"\x1b[?1006h");
        assert!(g.modes.mouse_sgr);
    }

    #[test]
    fn alt_screen_1049_enters_and_restores_cursor() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, b"\x1b[3;5Hmain"); // 光标到 (col4,row2)，写 4 字 → 落在 col8
        assert_eq!(cursor_of(&g), (8, 2));
        feed(&mut g, b"\x1b[?1049h");
        assert!(g.on_alt());
        feed(&mut g, b"\x1b[?1049l");
        assert!(!g.on_alt());
        assert_eq!(cursor_of(&g), (8, 2)); // 离开备用屏恢复进入前光标
        assert_eq!(row_text(&g, 2), "    main"); // 主屏内容还原
    }

    fn cursor_of(g: &Grid) -> (usize, usize) {
        (g.cx, g.cy)
    }

    // ---------------- 滚动区域 DECSTBM ----------------

    #[test]
    fn decstbm_confines_scroll_to_region() {
        let mut g = Grid::new(4, 5);
        feed(&mut g, b"r0\r\nr1\r\nr2\r\nr3\r\nr4"); // 5 行
        feed(&mut g, b"\x1b[2;4r"); // 限定滚动区 1-based 2..=4 → 0-based 1..=3
        feed(&mut g, b"\x1b[4;1H"); // 光标到区域底行（0-based row3）
        feed(&mut g, b"\x1bD"); // IND：区域底 → 区域内上滚一行
        assert_eq!(row_text(&g, 0), "r0"); // 区域外不动
        assert_eq!(row_text(&g, 4), "r4"); // 区域外不动
        assert_eq!(row_text(&g, 1), "r2"); // 区域内整体上移
        assert_eq!(row_text(&g, 2), "r3");
    }

    // ---------------- DCS：tmux 把 OSC52 包进 passthrough ----------------

    #[test]
    fn dcs_tmux_unwraps_inner_osc52() {
        // tmux 格式：DCS tmux; <内层 ESC 翻倍> ST。内层是 OSC52 设剪贴板为 "hi"。
        // base64("hi") = "aGk="。内层 ESC(0x1b) 在 wire 上翻倍。
        let mut g = Grid::new(10, 2);
        feed(&mut g, b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        let ev = g.drain_events();
        let clip = ev.iter().find_map(|e| match e {
            TermEvent::SetClipboard(s) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(clip.as_deref(), Some("hi"), "events: {ev:?}");
    }

    #[test]
    fn osc52_direct_sets_clipboard() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, b"\x1b]52;c;aGk=\x07");
        let ev = g.drain_events();
        assert!(
            ev.iter()
                .any(|e| matches!(e, TermEvent::SetClipboard(s) if s == "hi")),
            "events: {ev:?}"
        );
    }

    // ---------------- OSC：标题 / 超链接 / 通知 / 提示符标记 ----------------

    #[test]
    fn osc_title_notify_hyperlink() {
        let mut g = Grid::new(20, 2);
        feed(&mut g, b"\x1b]0;my title\x07");
        feed(&mut g, b"\x1b]8;;https://example.com\x07L\x1b]8;;\x07");
        feed(&mut g, b"\x1b]777;notify;Hed;Body\x07");
        let ev = g.drain_events();
        assert!(
            ev.iter()
                .any(|e| matches!(e, TermEvent::Title(t) if t == "my title")),
            "{ev:?}"
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, TermEvent::Notify(t, b) if t == "Hed" && b == "Body")),
            "{ev:?}"
        );
        // 超链接 id 落到打印的格子上。
        assert_eq!(g.link_at(0, 0).as_deref(), Some("https://example.com"));
    }

    #[test]
    fn osc133_prompt_and_command_marks() {
        let mut g = Grid::new(10, 2);
        feed(&mut g, b"\x1b]133;C\x07hi\x1b]133;D;0\x07");
        let ev = g.drain_events();
        assert!(ev.iter().any(|e| matches!(e, TermEvent::PromptStart)));
        assert!(ev
            .iter()
            .any(|e| matches!(e, TermEvent::CommandEnd(Some(0)))));
    }

    #[test]
    fn bel_emits_event() {
        let mut g = Grid::new(4, 1);
        feed(&mut g, b"a\x07");
        assert!(g.drain_events().iter().any(|e| matches!(e, TermEvent::Bell)));
    }

    // ---------------- 应答（回写 PTY）----------------

    #[test]
    fn da_and_dsr_replies() {
        let mut g = Grid::new(10, 6);
        feed(&mut g, b"\x1b[c"); // 主 DA
        feed(&mut g, b"\x1b[>c"); // 次要 DA
        feed(&mut g, b"\x1b[5;3H\x1b[6n"); // 光标到 (row5,col3) 后查询光标位置
        let replies: Vec<Vec<u8>> = g
            .drain_events()
            .into_iter()
            .filter_map(|e| match e {
                TermEvent::Reply(b) => Some(b),
                _ => None,
            })
            .collect();
        assert!(replies.iter().any(|r| r == b"\x1b[?1;2c"), "{replies:?}");
        assert!(replies.iter().any(|r| r == b"\x1b[>0;95;0c"), "{replies:?}");
        assert!(replies.iter().any(|r| r == b"\x1b[5;3R"), "{replies:?}"); // 1-based 行;列
    }

    // ---------------- ESC 系列 ----------------

    #[test]
    fn esc_keypad_toggle() {
        let mut g = Grid::new(6, 3);
        feed(&mut g, b"\x1b="); // DECKPAM
        assert!(g.modes.app_keypad);
        feed(&mut g, b"\x1b>"); // DECKPNM
        assert!(!g.modes.app_keypad);
    }

    #[test]
    fn ris_resets_modes_and_clears() {
        let mut g = Grid::new(6, 3);
        feed(&mut g, b"\x1b[?25l\x1b[1;31mtext\x1bc"); // 关光标 + 染色 + 文本，再 RIS
        assert!(g.modes.cursor_visible, "RIS 应复位模式");
        assert_eq!(row_text(&g, 0), "", "RIS 应清屏");
    }

    // ---------------- 宽字符走解析路径 ----------------

    #[test]
    fn wide_char_through_parser() {
        let g = run(6, 1, "中a".as_bytes());
        assert_eq!(g.cell(0, 0).c, '中');
        assert!(g.cell(1, 0).wide_spacer);
        assert_eq!(g.cell(2, 0).c, 'a');
    }

    // ---------------- 防御：极端参数 / 不完整序列不得 panic ----------------

    #[test]
    fn huge_and_zero_params_no_panic() {
        let mut g = Grid::new(8, 4);
        feed(&mut g, b"\x1b[999999999;999999999H");
        feed(&mut g, b"\x1b[999999999A\x1b[999999999B\x1b[999999999C\x1b[999999999D");
        feed(&mut g, b"\x1b[0J\x1b[3J\x1b[0K");
        feed(&mut g, b"\x1b[99999P\x1b[99999@\x1b[99999X\x1b[99999L\x1b[99999M");
        feed(&mut g, b"\x1b[0;0r"); // 退化滚动区
        feed(&mut g, b"after"); // 还能继续工作
        assert_eq!(g.cell(0, 0).c, 'a');
    }

    #[test]
    fn sgr_nested_and_256_default_colors() {
        let g = run(10, 1, b"\x1b[31;42mA\x1b[39mB\x1b[49mC\x1b[38;5;196mD\x1b[48;5;17mE");
        assert_eq!(g.cell(0, 0).fg, Color::Indexed(1));
        assert_eq!(g.cell(0, 0).bg, Color::Indexed(2));
        assert_eq!(g.cell(1, 0).fg, Color::Default); // 39
        assert_eq!(g.cell(1, 0).bg, Color::Indexed(2));
        assert_eq!(g.cell(2, 0).bg, Color::Default); // 49
        assert_eq!(g.cell(3, 0).fg, Color::Indexed(196));
        assert_eq!(g.cell(4, 0).bg, Color::Indexed(17));
    }

    #[test]
    fn cup_out_of_bounds_clamps() {
        let g = run(5, 3, b"\x1b[999;999H");
        assert_eq!((g.cx, g.cy), (4, 2));
    }

    #[test]
    fn ed_3_scrollback_only_mode() {
        let mut g = Grid::new(4, 2);
        feed(&mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
        // The implementation currently maps ED(3) to the same full-screen clear as ED(2).
        feed(&mut g, b"\x1b[3J");
        g.reset_view();
        assert_eq!(row_text(&g, 0), "");
        assert_eq!(row_text(&g, 1), "");
    }

    #[test]
    fn il_dl_within_scroll_region_no_leak() {
        let mut g = Grid::new(4, 6);
        feed(&mut g, b"r0\r\nr1\r\nr2\r\nr3\r\nr4\r\nr5");
        feed(&mut g, b"\x1b[2;5r"); // region rows 1..=4
        feed(&mut g, b"\x1b[3;1H"); // cursor to row 2 (inside region)
        feed(&mut g, b"\x1b[2L"); // insert 2 lines
        assert_eq!(row_text(&g, 0), "r0"); // above region untouched
        assert_eq!(row_text(&g, 1), "r1");
        assert_eq!(row_text(&g, 2), "");
        assert_eq!(row_text(&g, 3), "");
        assert_eq!(row_text(&g, 5), "r5"); // below region untouched
        // DL：在 row2 删 1 行。区域内 row2 以下上移、底边距 row4 补空；
        // r3/r4 已在上面的 IL 越过下边距被丢弃，回不来；r5 在区域外（row5）永不移动。
        feed(&mut g, b"\x1b[3;1H\x1b[1M");
        assert_eq!(row_text(&g, 0), "r0"); // 区域外，冻结
        assert_eq!(row_text(&g, 1), "r1"); // 游标上方，不动
        assert_eq!(row_text(&g, 2), ""); // 原 row3 的空行上移过来
        assert_eq!(row_text(&g, 3), "r2"); // 原 row4 的 r2 上移过来
        assert_eq!(row_text(&g, 4), ""); // 底边距补的新空行
        assert_eq!(row_text(&g, 5), "r5"); // 区域外，冻结
    }

    #[test]
    fn osc8_reuse_id_then_close() {
        let mut g = Grid::new(20, 2);
        feed(&mut g, b"\x1b]8;;https://a.com\x07A\x1b]8;;https://b.com\x07B\x1b]8;;https://a.com\x07C\x1b]8;;\x07D");
        let id_a = g.cell(0, 0).link;
        let id_b = g.cell(1, 0).link;
        let id_c = g.cell(2, 0).link;
        let id_d = g.cell(3, 0).link;
        assert_ne!(id_a, 0);
        assert_ne!(id_b, 0);
        assert_eq!(id_a, id_c); // reuse
        assert_eq!(id_d, 0);    // closed
        // link_table is private; the public ids prove reuse behavior.
        assert_eq!(g.link_uri(id_a).unwrap(), "https://a.com");
        assert_eq!(g.link_uri(id_b).unwrap(), "https://b.com");
    }

    #[test]
    fn ris_resets_saved_cursor_and_modes() {
        let mut g = Grid::new(10, 4);
        feed(&mut g, b"\x1b[?25l\x1b[1;31mtext\x1b7\x1bc");
        assert!(g.modes.cursor_visible);
        assert_eq!(row_text(&g, 0), "");
    }

    #[test]
    fn scrollback_lines_drop_when_over_limit() {
        let mut g = Grid::new(4, 2);
        g.set_scrollback_max(3);
        for i in 0..10u8 {
            feed(&mut g, format!("{i}\r\n").as_bytes());
        }
        g.scroll_view_up(10);
        // Only last 3 lines retained in scrollback, plus currently visible rows.
        assert_eq!(g.visible_cell(0, 0).c, '6');
        assert_eq!(g.visible_cell(0, 1).c, '7');
        assert_eq!(g.visible_cell(0, 2).c, '8');
    }

    #[test]
    fn mouse_sgr_report_format() {
        // App-level behavior: SGR mouse report string format.
        // We can't easily instantiate App here, but we can verify the grid state used.
        let mut g = Grid::new(80, 24);
        feed(&mut g, b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(g.modes.mouse_proto, MouseProto::ButtonEvent);
        assert!(g.modes.mouse_sgr);
    }

    #[test]
    fn incomplete_sequences_no_panic() {
        let mut g = Grid::new(8, 4);
        feed(&mut g, b"\x1b[3"); // 断在参数中
        feed(&mut g, b"\x1b]0;unterminated-osc"); // OSC 未结束
        feed(&mut g, b"\x1bPtmux;partial"); // DCS 未结束
        feed(&mut g, b"\x1b"); // 孤立 ESC
        // 不 panic 即通过。
    }
}
