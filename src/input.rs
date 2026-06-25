//! 键盘 → VT 字节编码（M4：输入完整）。
//!
//! 数据流闭环里「输入」那半边：winit 的按键事件编码成终端字节，写回 PTY。
//! 覆盖：可打印字符、Enter/Backspace/Tab/Esc、方向键（含**应用光标键** DECCKM）、
//! Home/End/PgUp/PgDn/Del/Ins、**F1–F12**、Ctrl+字母 控制码、Alt 前缀 ESC，
//! 以及 **Shift/Alt/Ctrl 修饰编码**（xterm `CSI 1;mod 〈letter〉` / `CSI 〈n〉;mod ~`）。
//! Cmd（super）组合不在这里编码——交给 app.rs 当本地快捷键（复制/粘贴/标签）。

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// 把一次按键编码成要写给 PTY 的字节。无对应字节（如纯修饰键）返回 `None`。
/// `app_cursor` = DECCKM（应用光标键模式）：方向/Home/End 用 `ESC O` 而非 `ESC [`。
pub fn encode(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();
    // xterm 修饰参数：1 + shift(1) + alt(2) + ctrl(4)。无修饰 = 1（省略）。
    let m = 1 + (shift as u8) + 2 * (alt as u8) + 4 * (ctrl as u8);

    if let Key::Named(named) = key {
        // Ctrl+Space → NUL（0x00）；否则下面会把 Space 当普通空格。
        if *named == NamedKey::Space && ctrl && !alt {
            return Some(maybe_alt(&[0x00], alt));
        }
        // 方向键 / Home / End：受 app_cursor + 修饰影响。
        let cursor = match named {
            NamedKey::ArrowUp => Some(b'A'),
            NamedKey::ArrowDown => Some(b'B'),
            NamedKey::ArrowRight => Some(b'C'),
            NamedKey::ArrowLeft => Some(b'D'),
            NamedKey::Home => Some(b'H'),
            NamedKey::End => Some(b'F'),
            _ => None,
        };
        if let Some(letter) = cursor {
            return Some(cursor_seq(letter, m, app_cursor));
        }

        // tilde 键：PgUp/PgDn/Ins/Del（+修饰）。
        let tilde = match named {
            NamedKey::Insert => Some(2),
            NamedKey::Delete => Some(3),
            NamedKey::PageUp => Some(5),
            NamedKey::PageDown => Some(6),
            _ => None,
        };
        if let Some(n) = tilde {
            return Some(tilde_seq(n, m));
        }

        // 功能键 F1–F12。
        if let Some(seq) = function_key(named, m) {
            return Some(seq);
        }

        let seq: Vec<u8> = match named {
            NamedKey::Enter => b"\r".to_vec(),
            NamedKey::Backspace => b"\x7f".to_vec(),
            NamedKey::Escape => b"\x1b".to_vec(),
            NamedKey::Tab => {
                if shift {
                    b"\x1b[Z".to_vec()
                } else {
                    b"\t".to_vec()
                }
            }
            NamedKey::Space => b" ".to_vec(),
            _ => return None,
        };
        return Some(maybe_alt(&seq, alt));
    }

    // Ctrl + 字母/符号 → C0 控制码（Ctrl-A=0x01 … Ctrl-Z=0x1A 等）。
    if ctrl && !alt {
        if let Key::Character(s) = key {
            if let Some(b) = ctrl_byte(s) {
                return Some(vec![b]);
            }
        }
    }

    // 普通可打印输入：优先用 winit 给的 text（已含 shift/布局结果）。
    let base: Vec<u8> = if let Some(t) = text {
        t.as_bytes().to_vec()
    } else if let Key::Character(s) = key {
        s.as_bytes().to_vec()
    } else {
        return None;
    };
    if base.is_empty() {
        return None;
    }
    Some(maybe_alt(&base, alt))
}

/// 方向键 / Home / End：无修饰时按 app_cursor 选 `ESC O x` 或 `ESC [ x`；有修饰则 `ESC [ 1 ; m x`。
fn cursor_seq(letter: u8, m: u8, app_cursor: bool) -> Vec<u8> {
    if m == 1 {
        if app_cursor {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    } else {
        format!("\x1b[1;{m}{}", letter as char).into_bytes()
    }
}

/// tilde 键：`ESC [ n ~`，有修饰 `ESC [ n ; m ~`。
fn tilde_seq(n: u8, m: u8) -> Vec<u8> {
    if m == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{m}~").into_bytes()
    }
}

/// F1–F12 的 xterm 编码（含修饰）。F1–F4 用 SS3（无修饰），其余用 `CSI n ~`。
fn function_key(named: &NamedKey, m: u8) -> Option<Vec<u8>> {
    // F1–F4：无修饰 ESC O P/Q/R/S；有修饰 ESC [ 1 ; m P…。
    let pqrs = match named {
        NamedKey::F1 => Some(b'P'),
        NamedKey::F2 => Some(b'Q'),
        NamedKey::F3 => Some(b'R'),
        NamedKey::F4 => Some(b'S'),
        _ => None,
    };
    if let Some(letter) = pqrs {
        return Some(if m == 1 {
            vec![0x1b, b'O', letter]
        } else {
            format!("\x1b[1;{m}{}", letter as char).into_bytes()
        });
    }
    // F5–F12：CSI n ~（n 见下表）。
    let n: u8 = match named {
        NamedKey::F5 => 15,
        NamedKey::F6 => 17,
        NamedKey::F7 => 18,
        NamedKey::F8 => 19,
        NamedKey::F9 => 20,
        NamedKey::F10 => 21,
        NamedKey::F11 => 23,
        NamedKey::F12 => 24,
        _ => return None,
    };
    Some(tilde_seq(n, m))
}

/// Alt 修饰：在序列前加一个 ESC（meta 前缀，xterm 习惯）。
fn maybe_alt(seq: &[u8], alt: bool) -> Vec<u8> {
    if alt {
        let mut v = Vec::with_capacity(seq.len() + 1);
        v.push(0x1b);
        v.extend_from_slice(seq);
        v
    } else {
        seq.to_vec()
    }
}

/// Ctrl 组合 → 控制字节。仅处理单字符的常见组合。
fn ctrl_byte(s: &str) -> Option<u8> {
    let c = s.chars().next()?;
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

/// 把一段要粘贴的文本编码（M4/M6：括号粘贴）。`bracketed` 时用 `ESC[200~ … ESC[201~` 包裹，
/// 并剔除内部可能伪造的结束符，防止粘贴注入。
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    // 统一换行为 CR（终端约定），并去掉可能存在的结束标记。
    let cleaned = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // 循环剔除粘贴标记，防止「拼接攻击」：如 `\x1b[20`+`\x1b[201~`+`1~` 单次 replace 后会重新拼出 `\x1b[201~`。
        let mut safe = cleaned;
        while safe.contains("\x1b[201~") || safe.contains("\x1b[200~") {
            safe = safe.replace("\x1b[201~", "").replace("\x1b[200~", "");
        }
        let mut out = Vec::with_capacity(safe.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(safe.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        cleaned.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_splice_cannot_reform_end_marker() {
        // 拼接攻击：单次 replace 后会重新拼出 \x1b[201~；循环剔除必须挡住。
        let evil = "\x1b[20\x1b[201~1~rm -rf ~";
        let out = encode_paste(evil, true);
        let s = String::from_utf8_lossy(&out);
        // body（去掉首尾包裹）里不得再含有结束标记。
        let body = &s[..s.len() - "\x1b[201~".len()];
        let body = &body["\x1b[200~".len()..];
        assert!(!body.contains("\x1b[201~"), "paste body 仍含结束标记: {body:?}");
    }

    #[test]
    fn paste_wraps_and_normalizes_newlines() {
        let out = encode_paste("a\nb", true);
        assert_eq!(out, b"\x1b[200~a\rb\x1b[201~");
        let plain = encode_paste("a\r\nb", false);
        assert_eq!(plain, b"a\rb");
    }
}
