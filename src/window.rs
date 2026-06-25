//! 一个 shell 窗口（= 一个 tab）：PTY + 独立的 `vte` 解析器 + 独立的字符网格 + 监控状态。
//!
//! 这正是设计文档 §2 里「每会话独立 Parser / Grid / 状态」的落地。
//! 每个 Window 自持 PTY 的 **writer**（键盘回写）；reader 由 App 起线程读，按 (conn,win) 路由回来。
//! 监控状态（M9）：OSC 133 的命令开始/结束驱动 `status`；`activity` 标记非活动 tab 有新输出。

use std::io::Write;

use crate::grid::Grid;
use crate::parser::Performer;
use crate::pty::PtyProcess;
use vte::Parser;

/// 会话运行状态（M9 监控；tab 上画状态点）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WinStatus {
    /// 空闲（提示符等待输入）。
    Idle,
    /// 运行中（命令执行）。
    Running,
    /// 上一条命令失败（非 0 退出）。
    Failed,
}

pub struct Window {
    pub id: usize,
    pub title: String,
    pub pty: PtyProcess,
    pub grid: Grid,
    parser: Parser,
    pub writer: Box<dyn Write + Send>,
    // ---- 监控（M9）----
    pub status: WinStatus,
    /// 非活动 tab 有新输出未查看 → tab 高亮。
    pub activity: bool,
    /// 失焦/非活动时收到「完成提示」（BEL/OSC9）→ tab 角标。
    pub alerted: bool,
    /// OSC 7 上报的远端当前工作目录（file:// URL）。
    pub cwd: Option<String>,
}

impl Window {
    pub fn new(id: usize, title: String, pty: PtyProcess, cols: u16, rows: u16) -> anyhow::Result<Self> {
        let writer = pty.writer()?;
        Ok(Self {
            id,
            title,
            pty,
            grid: Grid::new(cols as usize, rows as usize),
            parser: Parser::new(),
            writer,
            status: WinStatus::Idle,
            activity: false,
            alerted: false,
            cwd: None,
        })
    }

    /// 喂一批 PTY 输出字节：解析器把动作落到网格。
    pub fn feed(&mut self, bytes: &[u8]) {
        // 借用拆分：parser 与 grid 是 self 的不同字段，可同时可变借用。
        let mut perf = Performer::new(&mut self.grid);
        // vte 0.15：`advance` 收整个切片（旧版 0.11 是逐字节喂）。
        self.parser.advance(&mut perf, bytes);
    }

    /// 键盘字节回写 PTY。
    pub fn write(&mut self, bytes: &[u8]) {
        if self.writer.write_all(bytes).is_ok() {
            let _ = self.writer.flush();
        }
    }

    /// 改变窗口尺寸：网格重排 + 通知 PTY（TIOCSWINSZ → SIGWINCH）。
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols as usize, rows as usize);
        let _ = self.pty.resize(cols, rows);
    }
}
