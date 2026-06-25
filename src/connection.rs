//! 一个连接目标（本地 shell 或一个 SSH 主机）。
//!
//! 关键点：**同一个 `Connection` 下的多个 `Window` 复用同一条底层 SSH 连接**。
//! 这正是「侧边栏里同一主机下多开窗口」的实现核心——靠 OpenSSH 的 ControlMaster：
//! 第一个窗口完成认证（含 MFA/OTP），之后的窗口走 master socket，秒开、免再认证。

use crate::pty::PtyProcess;
use crate::window::Window;
use anyhow::Result;
use portable_pty::CommandBuilder;

/// 连接目标。
pub enum Target {
    /// 本地 shell（用 `$SHELL`，回退 `/bin/bash`）。
    Local,
    /// 远程 SSH 主机，形如 `host` 或 `user@host`（也支持 ~/.ssh/config 里的别名）。
    Ssh(String),
}

pub struct Connection {
    pub target: Target,
    pub label: String,
    windows: Vec<Window>,
    next_id: usize,
}

impl Connection {
    pub fn new(target: Target) -> Self {
        let label = match &target {
            Target::Local => "local".to_string(),
            Target::Ssh(h) => h.clone(),
        };
        Self {
            target,
            label,
            windows: Vec::new(),
            next_id: 0,
        }
    }

    /// 在该连接下开一个新窗口。
    pub fn open_window(&mut self, cols: u16, rows: u16) -> Result<usize> {
        let cmd = self.build_command();
        let pty = PtyProcess::spawn(cmd, cols, rows)?;
        let id = self.next_id;
        self.next_id += 1;
        let title = format!("{} · win{}", self.label, id);
        self.windows.push(Window::new(id, title, pty, cols, rows)?);
        Ok(id)
    }

    pub fn window_mut(&mut self, id: usize) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    pub fn window(&self, id: usize) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// 所有窗口 id（按打开顺序）。
    pub fn window_ids(&self) -> Vec<usize> {
        self.windows.iter().map(|w| w.id).collect()
    }

    /// 该连接下所有窗口（侧边栏树用）。
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// 关闭一个窗口（其 PTY 随 Window drop 一并回收）。返回是否真的删掉了。
    pub fn close_window(&mut self, id: usize) -> bool {
        let before = self.windows.len();
        self.windows.retain(|w| w.id != id);
        self.windows.len() != before
    }

    /// 构造启动命令。ssh 路线带上 ControlMaster 复用选项。
    fn build_command(&self) -> CommandBuilder {
        match &self.target {
            Target::Local => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
                let mut cmd = CommandBuilder::new(shell);
                cmd.env("TERM", "xterm-256color");
                cmd
            }
            Target::Ssh(host) => {
                let mut cmd = CommandBuilder::new("ssh");
                // -tt：强制分配远端 PTY（即使本地 stdin 非 tty 也分配）。
                cmd.arg("-tt");

                // ↓↓↓ 连接复用：同一主机的多个窗口共享一条底层 SSH 连接。
                // 第一个窗口建立 master 并认证；之后的窗口走 master socket，秒开免认证。
                cmd.arg("-o");
                cmd.arg("ControlMaster=auto");
                cmd.arg("-o");
                cmd.arg("ControlPath=~/.ssh/cm-%C"); // %C = host/port/user 派生的哈希
                cmd.arg("-o");
                cmd.arg("ControlPersist=10m"); // 最后一个窗口关掉后，master 再保留 10 分钟

                cmd.arg(host);
                cmd.env("TERM", "xterm-256color");
                cmd
            }
        }
    }
}
