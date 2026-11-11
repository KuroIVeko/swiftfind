//! Global hotkey definition and the settings file that stores it.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MAPVK_VK_TO_CHAR, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, MapVirtualKeyW, VkKeyScanW,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Windows virtual-key code.
    pub vk: u32,
}

const VK_F1: u32 = 0x70;
const VK_F24: u32 = 0x87;

/// Keys with a fixed name: (gpui key name, label, virtual-key code).
const NAMED: &[(&str, &str, u32)] = &[
    ("space", "Space", 0x20),
    ("enter", "Enter", 0x0D),
    ("tab", "Tab", 0x09),
    ("backspace", "Backspace", 0x08),
    ("insert", "Insert", 0x2D),
    ("delete", "Delete", 0x2E),
    ("home", "Home", 0x24),
    ("end", "End", 0x23),
    ("pageup", "PageUp", 0x21),
    ("pagedown", "PageDown", 0x22),
    ("up", "Up", 0x26),
    ("down", "Down", 0x28),
    ("left", "Left", 0x25),
    ("right", "Right", 0x27),
];

impl Hotkey {
    pub const DEFAULT: Hotkey = Hotkey { ctrl: true, alt: false, shift: true, win: false, vk: b'E' as u32 };

    /// Builds a hotkey from a gpui keystroke (`key` is gpui's key name).
    pub fn from_keystroke(ctrl: bool, alt: bool, shift: bool, win: bool, key: &str) -> Option<Hotkey> {
        let vk = if let Some(&(_, _, vk)) = NAMED.iter().find(|n| n.0 == key) {
            vk
        } else if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
            VK_F1 + n - 1
        } else {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return None };
            let (vk, needs_shift) = char_to_vk(c)?;
            // gpui may report a shifted character ("!") without Shift.
            return Some(Hotkey { ctrl, alt, shift: shift || needs_shift, win, vk });
        };
        Some(Hotkey { ctrl, alt, shift, win, vk })
    }

    /// Plain keys and Shift+key would swallow normal typing system-wide.
    pub fn validate(&self) -> Result<(), &'static str> {
        let is_fkey = (VK_F1..=VK_F24).contains(&self.vk);
        if self.ctrl || self.alt || self.win || is_fkey {
            Ok(())
        } else {
            Err(tr!(
                "Include Ctrl, Alt or Win (except for F1–F24), or it would get in the way of typing",
                "需要包含 Ctrl、Alt 或 Win（F1–F24 除外），否则会干扰正常打字"
            ))
        }
    }

    pub fn modifiers(&self) -> HOT_KEY_MODIFIERS {
        let mut m = HOT_KEY_MODIFIERS(0);
        for (on, bit) in [(self.ctrl, MOD_CONTROL), (self.alt, MOD_ALT), (self.shift, MOD_SHIFT), (self.win, MOD_WIN)] {
            if on {
                m |= bit;
            }
        }
        m
    }

    /// Packs into one word for passing through a window message.
    pub fn pack(&self) -> usize {
        ((self.modifiers().0 as usize) << 16) | self.vk as usize
    }

    pub fn unpack(w: usize) -> Hotkey {
        let m = (w >> 16) as u32;
        Hotkey {
            ctrl: m & MOD_CONTROL.0 != 0,
            alt: m & MOD_ALT.0 != 0,
            shift: m & MOD_SHIFT.0 != 0,
            win: m & MOD_WIN.0 != 0,
            vk: (w & 0xFFFF) as u32,
        }
    }

    /// e.g. "Ctrl+Shift+E". Also the settings-file format.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (on, name) in [(self.win, "Win"), (self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")] {
            if on {
                parts.push(name.into());
            }
        }
        parts.push(key_label(self.vk));
        parts.join("+")
    }

    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut h = Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0 };
        let parts: Vec<&str> = s.trim().split('+').collect();
        let (key, mods) = parts.split_last()?;
        for m in mods {
            match m.trim().to_ascii_lowercase().as_str() {
                "ctrl" => h.ctrl = true,
                "alt" => h.alt = true,
                "shift" => h.shift = true,
                "win" => h.win = true,
                _ => return None,
            }
        }
        // "Ctrl++" splits into an empty last part; the key is '+'.
        let key = if key.is_empty() && s.ends_with("++") { "+" } else { key.trim() };
        let lower = key.to_ascii_lowercase();
        h.vk = Hotkey::from_keystroke(false, false, false, false, &lower)?.vk;
        Some(h)
    }
}

/// (virtual key, needs Shift) for a character on the current keyboard layout.
fn char_to_vk(c: char) -> Option<(u32, bool)> {
    let mut buf = [0u16; 2];
    let [u] = c.encode_utf16(&mut buf) else { return None };
    let r = unsafe { VkKeyScanW(*u) };
    // Low byte: virtual key; high byte: shift state; -1 if the layout has no such key.
    if r == -1 {
        return None;
    }
    let (vk, state) = ((r as u16 & 0xFF) as u32, (r as u16 >> 8) as u8);
    // Letters come in lower case from gpui, so 'E' must not imply Shift.
    Some((vk, state & 1 != 0 && !c.is_ascii_alphabetic()))
}

fn key_label(vk: u32) -> String {
    if let Some(&(_, label, _)) = NAMED.iter().find(|n| n.2 == vk) {
        return label.into();
    }
    if (VK_F1..=VK_F24).contains(&vk) {
        return format!("F{}", vk - VK_F1 + 1);
    }
    let c = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) } & 0xFFFF;
    match char::from_u32(c).filter(|c| !c.is_control() && *c != '\0') {
        Some(c) => c.to_uppercase().collect(),
        None => format!("VK{vk:#04x}"),
    }
}

/// Configured hotkey, or the default if unset / unreadable.
pub fn load() -> Hotkey {
    crate::settings::get("hotkey").and_then(|v| Hotkey::parse(&v)).unwrap_or(Hotkey::DEFAULT)
}

pub fn save(h: &Hotkey) -> std::io::Result<()> {
    crate::settings::set("hotkey", &h.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_roundtrip() {
        for s in ["Ctrl+Shift+E", "Alt+Space", "Win+Shift+F", "F8", "Ctrl+Alt+PageDown", "Ctrl+F12"] {
            let h = Hotkey::parse(s).unwrap();
            assert_eq!(h.label(), s);
            assert_eq!(Hotkey::unpack(h.pack()), h);
        }
        assert_eq!(Hotkey::parse("ctrl+shift+e"), Some(Hotkey::DEFAULT));
        assert!(Hotkey::parse("Hyper+E").is_none());
        assert!(Hotkey::parse("Shift+E").unwrap().validate().is_err());
        assert!(Hotkey::parse("F8").unwrap().validate().is_ok());
    }
}
