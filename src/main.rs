//! Ianua（雅努斯之门）—— 为 AI 编码 agent 定制、与 agent 双向交互的 GPU 终端。
//!
//! 相对 M0（crossterm 透传）的变化：本程序自己开窗（winit）、用 GPU 渲染（wgpu+glyphon）、
//! 把 PTY 字节经 `vte` 解析成字符网格再绘制，并把键盘编码成终端字节写回 PTY。
//! 这样就跑通了数据流闭环：PTY → 解析 → 网格 → 渲染 / 键盘 → 编码 → PTY。
//!
//! 用法：
//!   cargo run                          # 本地 shell
//!   cargo run -- gadi                  # ssh 到 ~/.ssh/config 里的别名 gadi
//!   cargo run -- tz1597@gadi.nci.org.au
//!
//! 退出：在 shell 里 `exit` / Ctrl-D，或直接关窗口。

mod app;
mod config;
mod connection;
mod grid;
mod input;
mod manager;
mod parser;
mod pty;
mod render;
mod theme;
mod window;

use anyhow::Result;
use app::{spawn_reader, App, UserEvent};
use connection::{Connection, Target};
use manager::ConnectionManager;
use winit::event_loop::{ControlFlow, EventLoop};

fn main() -> Result<()> {
    // 1) 解析参数：有参数 = ssh 主机；否则本地 shell。
    let target = match std::env::args().nth(1) {
        Some(host) => Target::Ssh(host),
        None => Target::Local,
    };

    // 2) 顶层管理器 → 一个连接 →（第一个）窗口。结构已为多窗口准备好。
    let mut manager = ConnectionManager::new();
    let conn_idx = manager.add(Connection::new(target));

    // 初始用 80x24 起 shell；窗口与渲染器就绪后会按真实窗口尺寸 resize（见 app.rs）。
    let (win_id, reader, title) = {
        let conn = manager.connection_mut(conn_idx).unwrap();
        let win_id = conn.open_window(80, 24)?;
        let w = conn.window(win_id).unwrap();
        (win_id, w.pty.reader()?, w.title.clone())
    };

    eprintln!("[ianua] 启动窗口 `{title}`。Cmd+T 新标签 / Cmd+W 关标签 / Cmd+C·V 复制粘贴；关窗或 exit 退出。");

    // 3) 事件循环（带自定义用户事件）+ 代理。
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // 4) 第一个窗口的 PTY 读线程（按 conn/win 路由；新标签由 App 再起线程）。
    spawn_reader(proxy.clone(), conn_idx, win_id, reader);

    // 5) 跑应用。manager 交给 App 持有——它持有各 PTY master，必须活到退出。
    let cfg = config::Config::load();
    let mut app = App::new(manager, conn_idx, win_id, proxy, cfg);
    event_loop.run_app(&mut app)?;
    Ok(())
}
