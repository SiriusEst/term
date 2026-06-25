//! 配色主题：基础前景/背景/光标/选区 + 完整 16 色 ANSI 调色板 + 内置方案。
//!
//! `Color::Indexed(0..=15)`（SGR 30–37/90–97、256 色前 16）走 `Theme::ansi` 调色板，
//! 所以换方案/改 16 色会同时影响 `ls`/`vim` 等的着色。16–255 仍按 xterm 立方/灰阶算。

/// 渲染主题。
#[derive(Clone, Copy, PartialEq)]
pub struct Theme {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor: [u8; 3],
    pub selection: [u8; 3],
    /// 16 个基础 ANSI 颜色。
    pub ansi: [[u8; 3]; 16],
}

impl Default for Theme {
    fn default() -> Self {
        catppuccin_mocha()
    }
}

impl Theme {
    /// 内置配色方案名（偏好面板按此循环切换）。
    pub const SCHEMES: &'static [&'static str] =
        &["catppuccin", "dracula", "nord", "gruvbox", "solarized", "tokyonight"];

    /// 按名取内置方案（大小写 / 分隔符不敏感）。
    pub fn by_name(name: &str) -> Option<Theme> {
        let key: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        Some(match key.as_str() {
            "catppuccin" | "catppuccinmocha" | "mocha" => catppuccin_mocha(),
            "dracula" => dracula(),
            "nord" => nord(),
            "gruvbox" | "gruvboxdark" => gruvbox_dark(),
            "solarized" | "solarizeddark" => solarized_dark(),
            "tokyonight" | "tokyo" => tokyo_night(),
            _ => return None,
        })
    }
}

/// `"rrggbb"` / `"#rrggbb"` → [u8;3]（仅用于硬编码方案，非法即 panic 暴露笔误）。
fn hx(s: &str) -> [u8; 3] {
    let h = s.trim_start_matches('#');
    [
        u8::from_str_radix(&h[0..2], 16).unwrap(),
        u8::from_str_radix(&h[2..4], 16).unwrap(),
        u8::from_str_radix(&h[4..6], 16).unwrap(),
    ]
}

fn catppuccin_mocha() -> Theme {
    Theme {
        fg: hx("cdd6f4"),
        bg: hx("1e1e2e"),
        cursor: hx("f5e0dc"),
        selection: hx("45475a"),
        ansi: [
            hx("45475a"), hx("f38ba8"), hx("a6e3a1"), hx("f9e2af"),
            hx("89b4fa"), hx("f5c2e7"), hx("94e2d5"), hx("bac2de"),
            hx("585b70"), hx("f38ba8"), hx("a6e3a1"), hx("f9e2af"),
            hx("89b4fa"), hx("f5c2e7"), hx("94e2d5"), hx("a6adc8"),
        ],
    }
}

fn dracula() -> Theme {
    Theme {
        fg: hx("f8f8f2"),
        bg: hx("282a36"),
        cursor: hx("f8f8f2"),
        selection: hx("44475a"),
        ansi: [
            hx("21222c"), hx("ff5555"), hx("50fa7b"), hx("f1fa8c"),
            hx("bd93f9"), hx("ff79c6"), hx("8be9fd"), hx("f8f8f2"),
            hx("6272a4"), hx("ff6e6e"), hx("69ff94"), hx("ffffa5"),
            hx("d6acff"), hx("ff92df"), hx("a4ffff"), hx("ffffff"),
        ],
    }
}

fn nord() -> Theme {
    Theme {
        fg: hx("d8dee9"),
        bg: hx("2e3440"),
        cursor: hx("d8dee9"),
        selection: hx("434c5e"),
        ansi: [
            hx("3b4252"), hx("bf616a"), hx("a3be8c"), hx("ebcb8b"),
            hx("81a1c1"), hx("b48ead"), hx("88c0d0"), hx("e5e9f0"),
            hx("4c566a"), hx("bf616a"), hx("a3be8c"), hx("ebcb8b"),
            hx("81a1c1"), hx("b48ead"), hx("8fbcbb"), hx("eceff4"),
        ],
    }
}

fn gruvbox_dark() -> Theme {
    Theme {
        fg: hx("ebdbb2"),
        bg: hx("282828"),
        cursor: hx("ebdbb2"),
        selection: hx("3c3836"),
        ansi: [
            hx("282828"), hx("cc241d"), hx("98971a"), hx("d79921"),
            hx("458588"), hx("b16286"), hx("689d6a"), hx("a89984"),
            hx("928374"), hx("fb4934"), hx("b8bb26"), hx("fabd2f"),
            hx("83a598"), hx("d3869b"), hx("8ec07c"), hx("ebdbb2"),
        ],
    }
}

fn solarized_dark() -> Theme {
    Theme {
        fg: hx("839496"),
        bg: hx("002b36"),
        cursor: hx("93a1a1"),
        selection: hx("073642"),
        ansi: [
            hx("073642"), hx("dc322f"), hx("859900"), hx("b58900"),
            hx("268bd2"), hx("d33682"), hx("2aa198"), hx("eee8d5"),
            hx("002b36"), hx("cb4b16"), hx("586e75"), hx("657b83"),
            hx("839496"), hx("6c71c4"), hx("93a1a1"), hx("fdf6e3"),
        ],
    }
}

fn tokyo_night() -> Theme {
    Theme {
        fg: hx("c0caf5"),
        bg: hx("1a1b26"),
        cursor: hx("c0caf5"),
        selection: hx("283457"),
        ansi: [
            hx("15161e"), hx("f7768e"), hx("9ece6a"), hx("e0af68"),
            hx("7aa2f7"), hx("bb9af7"), hx("7dcfff"), hx("a9b1d6"),
            hx("414868"), hx("f7768e"), hx("9ece6a"), hx("e0af68"),
            hx("7aa2f7"), hx("bb9af7"), hx("7dcfff"), hx("c0caf5"),
        ],
    }
}
