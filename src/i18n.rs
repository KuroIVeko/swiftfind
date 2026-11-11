//! English / Chinese UI text.
//!
//! The language follows the Windows display language (Chinese → Chinese,
//! anything else → English). `language=en` or `language=zh` in settings.ini
//! overrides it. Chosen once at startup.

use std::sync::OnceLock;

static ZH: OnceLock<bool> = OnceLock::new();

/// Whether the UI is in Chinese.
pub fn zh() -> bool {
    *ZH.get_or_init(|| match crate::settings::get("language").as_deref().map(str::to_ascii_lowercase) {
        Some(l) if l.starts_with("zh") => true,
        Some(l) if l.starts_with("en") => false,
        _ => system_is_chinese(),
    })
}

fn system_is_chinese() -> bool {
    const LANG_CHINESE: u16 = 0x04;
    let lang = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    lang & 0x3ff == LANG_CHINESE
}

/// `tr!("English", "中文")` picks a `&'static str`.
macro_rules! tr {
    ($en:literal, $zh:literal $(,)?) => {
        if $crate::i18n::zh() { $zh } else { $en }
    };
}

/// `trf!("{n} items", "{n} 个")` formats a `String` (inline or explicit args).
macro_rules! trf {
    ($en:literal, $zh:literal $(, $($arg:tt)+)?) => {
        if $crate::i18n::zh() { format!($zh $(, $($arg)+)?) } else { format!($en $(, $($arg)+)?) }
    };
}
