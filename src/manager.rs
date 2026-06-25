//! 顶层管理器：持有所有连接。
//!
//! 侧边栏（M7）就是 `connections → windows` 这棵树的视图。
//! M0 里只放一个连接、一个窗口，但结构已经为多连接·多窗口准备好。

use crate::connection::Connection;

#[derive(Default)]
pub struct ConnectionManager {
    pub connections: Vec<Connection>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入一个连接，返回它的下标。
    pub fn add(&mut self, conn: Connection) -> usize {
        self.connections.push(conn);
        self.connections.len() - 1
    }

    pub fn connection_mut(&mut self, idx: usize) -> Option<&mut Connection> {
        self.connections.get_mut(idx)
    }

    pub fn connection(&self, idx: usize) -> Option<&Connection> {
        self.connections.get(idx)
    }

    /// 直达某个窗口（跨连接）。
    pub fn window_mut(&mut self, conn: usize, win: usize) -> Option<&mut crate::window::Window> {
        self.connections.get_mut(conn)?.window_mut(win)
    }

    /// 所有窗口的 (conn_idx, win_id) 扁平列表（按连接、再按窗口打开顺序）——侧边栏/tab 用。
    pub fn all_windows(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (ci, conn) in self.connections.iter().enumerate() {
            for wid in conn.window_ids() {
                out.push((ci, wid));
            }
        }
        out
    }
}
