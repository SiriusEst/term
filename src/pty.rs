//! PTY 封装：把一个子进程（shell / ssh）挂到伪终端上，拿到可读/可写句柄。
//!
//! 这是数据流闭环里「PTY 主设备」那一格。后续里程碑里，reader 出来的字节
//! 会喂给 `vte` 解析器，而不是像 M0 这样直接打到 stdout。

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};

/// 一个挂在 PTY 上运行的子进程。
pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyProcess {
    /// 在一个新建的 PTY 里启动 `cmd`，初始尺寸 `cols x rows`。
    pub fn spawn(cmd: CommandBuilder, cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // 在 slave 端启动子进程。
        let child = pair.slave.spawn_command(cmd)?;
        // spawn 之后 slave 句柄就不需要了；显式 drop，
        // 这样子进程退出时 master 端能正确读到 EOF。
        drop(pair.slave);

        Ok(Self {
            master: pair.master,
            child,
        })
    }

    /// 克隆一个读取端：从子进程读输出字节（独立于 master 生命周期）。
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader()
    }

    /// 取出写入端：把键盘字节写给子进程（只能取一次）。
    pub fn writer(&self) -> Result<Box<dyn Write + Send>> {
        self.master.take_writer()
    }

    /// 窗口尺寸变化时同步给 PTY（底层 ioctl(TIOCSWINSZ) → 子进程收到 SIGWINCH）。
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// 阻塞等待子进程结束。
    #[allow(dead_code)]
    pub fn wait(&mut self) -> Result<()> {
        self.child.wait()?;
        Ok(())
    }
}
