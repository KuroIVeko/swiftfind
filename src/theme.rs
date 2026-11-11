//! Light / dark colors, following the Windows app theme.

use gpui::{App, Global, Rgba, Window, WindowAppearance, rgb, rgba};

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Rgba,
    pub text: Rgba,
    /// Toolbar, column header and status bar.
    pub chrome: Rgba,
    pub chrome_border: Rgba,
    pub muted: Rgba,
    pub faint: Rgba,
    pub hover: Rgba,
    pub selected: Rgba,
    pub divider: Rgba,
    pub button_hover: Rgba,
    pub popup: Rgba,
    pub popup_border: Rgba,
    pub separator: Rgba,
    pub input_bg: Rgba,
    pub input_border: Rgba,
    pub field: Rgba,
    pub field_active: Rgba,
    pub accent: Rgba,
    pub on_accent: Rgba,
    pub error: Rgba,
    pub ok: Rgba,
    pub overlay: Rgba,
    pub placeholder: Rgba,
    pub text_selection: Rgba,
    pub cursor: Rgba,
}

impl Global for Theme {}

impl Theme {
    fn light() -> Theme {
        Theme {
            bg: rgb(0xffffff),
            text: rgb(0x1e1e1e),
            chrome: rgb(0xf3f3f3),
            chrome_border: rgb(0xdddddd),
            muted: rgb(0x555555),
            faint: rgb(0x888888),
            hover: rgb(0xe5f3ff),
            selected: rgb(0xcce8ff),
            divider: rgb(0xe2e2e2),
            button_hover: rgb(0xe0e0e0),
            popup: rgb(0xffffff),
            popup_border: rgb(0xcccccc),
            separator: rgb(0xe5e5e5),
            input_bg: rgb(0xffffff),
            input_border: rgb(0xb0b0b0),
            field: rgb(0xfafafa),
            field_active: rgb(0xeef5ff),
            accent: rgb(0x1f6fd6),
            on_accent: rgb(0xffffff),
            error: rgb(0xc42b1c),
            ok: rgb(0x107c10),
            overlay: rgba(0x00000033),
            placeholder: rgba(0x00000066),
            text_selection: rgba(0x3311ff30),
            cursor: rgb(0x1f6fd6),
        }
    }

    fn dark() -> Theme {
        Theme {
            bg: rgb(0x1e1e1e),
            text: rgb(0xe4e4e4),
            chrome: rgb(0x2a2a2a),
            chrome_border: rgb(0x3a3a3a),
            muted: rgb(0xb4b4b4),
            faint: rgb(0x8a8a8a),
            hover: rgb(0x2c3a4a),
            selected: rgb(0x26486e),
            divider: rgb(0x3a3a3a),
            button_hover: rgb(0x3a3a3a),
            popup: rgb(0x2b2b2b),
            popup_border: rgb(0x474747),
            separator: rgb(0x404040),
            input_bg: rgb(0x1e1e1e),
            input_border: rgb(0x555555),
            field: rgb(0x333333),
            field_active: rgb(0x1d3352),
            accent: rgb(0x2f78d8),
            on_accent: rgb(0xffffff),
            error: rgb(0xff7b6e),
            ok: rgb(0x6ccb5f),
            overlay: rgba(0x00000080),
            placeholder: rgba(0xffffff66),
            text_selection: rgba(0x4a90e260),
            cursor: rgb(0x6cb4ff),
        }
    }

    fn for_appearance(a: WindowAppearance) -> Theme {
        match a {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Theme::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Theme::light(),
        }
    }
}

pub fn get(cx: &App) -> Theme {
    *cx.global::<Theme>()
}

/// Installs the theme for `window` and keeps it in sync with the system
/// setting (gpui also switches the title bar itself).
pub fn follow_system(window: &mut Window, cx: &mut App) {
    cx.set_global(Theme::for_appearance(window.appearance()));
    window
        .observe_window_appearance(|window, cx| {
            cx.set_global(Theme::for_appearance(window.appearance()));
            window.refresh();
        })
        .detach();
}
