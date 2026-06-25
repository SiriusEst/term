//! 配置（M10 + 偏好面板）：`~/.config/term/config.toml`。
//!
//! 可调：字号、scrollback 行数、配色方案、主题色（前景/背景/光标/选区）、完整 16 色 ANSI 调色板。
//! 文件缺失或字段缺省都回退默认，绝不因配置出错而起不来。偏好面板（Cmd+,）改完会写回此文件。
//!
//! 示例：
//! ```toml
//! font_size = 16.0
//! scrollback = 10000
//! [theme]
//! scheme = "dracula"            # 内置：catppuccin/dracula/nord/gruvbox/solarized/tokyonight
//! fg = "#f8f8f2"               # 可在方案之上单独覆盖
//! palette = ["#000000", ...]   # 可选：覆盖 16 个 ANSI 基础色
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::theme::Theme;

#[derive(Default, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct Config {
    /// 逻辑字号（像素，未乘缩放因子）。默认 15。
    pub font_size: Option<f32>,
    /// 主屏 scrollback 行数。默认 5000。
    pub scrollback: Option<usize>,
    pub theme: ThemeCfg,
}

#[derive(Default, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct ThemeCfg {
    /// 内置配色方案名。
    pub scheme: Option<String>,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    /// 可选：覆盖 16 个 ANSI 基础色（多余的忽略，不足的保留方案值）。
    pub palette: Option<Vec<String>>,
}

impl Config {
    /// 读取配置文件（不存在 / 解析失败都回退默认）。
    pub fn load() -> Self {
        match config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) => toml::from_str(&text).unwrap_or_else(|e| {
                eprintln!("[term] 配置解析失败，用默认值：{e}");
                Config::default()
            }),
            None => Config::default(),
        }
    }

    /// 写回配置文件（偏好面板改设置后调用；失败仅打日志，不影响运行）。
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("[term] 写配置失败：{e}");
                }
            }
            Err(e) => eprintln!("[term] 序列化配置失败：{e}"),
        }
    }

    /// 解析成最终主题：内置方案（缺省 Catppuccin）→ 单色覆盖 → 16 色调色板覆盖。
    pub fn theme(&self) -> Theme {
        let mut t = self
            .theme
            .scheme
            .as_deref()
            .and_then(Theme::by_name)
            .unwrap_or_default();
        if let Some(c) = self.theme.fg.as_deref().and_then(parse_hex) {
            t.fg = c;
        }
        if let Some(c) = self.theme.bg.as_deref().and_then(parse_hex) {
            t.bg = c;
        }
        if let Some(c) = self.theme.cursor.as_deref().and_then(parse_hex) {
            t.cursor = c;
        }
        if let Some(c) = self.theme.selection.as_deref().and_then(parse_hex) {
            t.selection = c;
        }
        if let Some(pal) = &self.theme.palette {
            for (i, s) in pal.iter().take(16).enumerate() {
                if let Some(c) = parse_hex(s) {
                    t.ansi[i] = c;
                }
            }
        }
        t
    }
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/term/config.toml"))
}

/// `#rrggbb` 或 `rrggbb` → [u8;3]。
fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some([r, g, b])
}
