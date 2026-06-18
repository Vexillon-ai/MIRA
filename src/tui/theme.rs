// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/theme.rs
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    // backgrounds
    pub bg:         Color,
    pub bg_panel:   Color,
    pub bg_input:   Color,
    // foregrounds
    pub fg:         Color,
    pub fg_dim:     Color,
    pub fg_user:    Color,
    pub fg_ai:      Color,
    pub fg_system:  Color,
    // accent
    pub accent:     Color,
    pub border:     Color,
    pub highlight:  Color,
}

impl Theme {
    pub fn border_style(&self) -> Style {
        Style::default().fg(self.border)
    }
    pub fn title_style(&self) -> Style {
        Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
    }
    pub fn user_msg_style(&self) -> Style {
        Style::default().fg(self.fg_user).add_modifier(Modifier::BOLD)
    }
    pub fn ai_msg_style(&self) -> Style {
        Style::default().fg(self.fg_ai)
    }
    pub fn system_msg_style(&self) -> Style {
        Style::default().fg(self.fg_system).add_modifier(Modifier::ITALIC)
    }
    pub fn input_style(&self) -> Style {
        Style::default().fg(self.fg).bg(self.bg_input)
    }
    pub fn status_style(&self) -> Style {
        Style::default().fg(self.fg_dim).bg(self.bg_panel)
    }
    pub fn highlight_style(&self) -> Style {
        Style::default().fg(self.bg).bg(self.highlight).add_modifier(Modifier::BOLD)
    }
    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "mira-dark"  => Some(MIRA_DARK.clone()),
            "mira-light" => Some(MIRA_LIGHT.clone()),
            "dracula"    => Some(DRACULA.clone()),
            "gruvbox"    => Some(GRUVBOX.clone()),
            "nord"       => Some(NORD.clone()),
            _            => None,
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &["mira-dark", "mira-light", "dracula", "gruvbox", "nord"]
    }
}

pub const MIRA_DARK: Theme = Theme {
    name:       "mira-dark",
    bg:         Color::Rgb(13, 17, 33),
    bg_panel:   Color::Rgb(22, 27, 48),
    bg_input:   Color::Rgb(30, 36, 58),
    fg:         Color::Rgb(230, 237, 243),
    fg_dim:     Color::Rgb(125, 133, 151),
    fg_user:    Color::Rgb(255, 196, 0),
    fg_ai:      Color::Rgb(100, 210, 255),
    fg_system:  Color::Rgb(125, 133, 151),
    accent:     Color::Rgb(255, 196, 0),
    border:     Color::Rgb(48, 56, 90),
    highlight:  Color::Rgb(255, 196, 0),
};

pub const MIRA_LIGHT: Theme = Theme {
    name:       "mira-light",
    bg:         Color::Rgb(250, 250, 252),
    bg_panel:   Color::Rgb(238, 240, 246),
    bg_input:   Color::Rgb(255, 255, 255),
    fg:         Color::Rgb(24, 24, 37),
    fg_dim:     Color::Rgb(130, 130, 150),
    fg_user:    Color::Rgb(120, 40, 200),
    fg_ai:      Color::Rgb(0, 100, 200),
    fg_system:  Color::Rgb(150, 150, 160),
    accent:     Color::Rgb(120, 40, 200),
    border:     Color::Rgb(200, 200, 215),
    highlight:  Color::Rgb(120, 40, 200),
};

pub const DRACULA: Theme = Theme {
    name:       "dracula",
    bg:         Color::Rgb(40, 42, 54),
    bg_panel:   Color::Rgb(33, 34, 44),
    bg_input:   Color::Rgb(68, 71, 90),
    fg:         Color::Rgb(248, 248, 242),
    fg_dim:     Color::Rgb(98, 114, 164),
    fg_user:    Color::Rgb(255, 121, 198),
    fg_ai:      Color::Rgb(80, 250, 123),
    fg_system:  Color::Rgb(98, 114, 164),
    accent:     Color::Rgb(189, 147, 249),
    border:     Color::Rgb(68, 71, 90),
    highlight:  Color::Rgb(189, 147, 249),
};

pub const GRUVBOX: Theme = Theme {
    name:       "gruvbox",
    bg:         Color::Rgb(40, 40, 40),
    bg_panel:   Color::Rgb(50, 48, 47),
    bg_input:   Color::Rgb(60, 56, 54),
    fg:         Color::Rgb(235, 219, 178),
    fg_dim:     Color::Rgb(146, 131, 116),
    fg_user:    Color::Rgb(250, 189, 47),
    fg_ai:      Color::Rgb(142, 192, 124),
    fg_system:  Color::Rgb(146, 131, 116),
    accent:     Color::Rgb(214, 93, 14),
    border:     Color::Rgb(80, 73, 69),
    highlight:  Color::Rgb(214, 93, 14),
};

pub const NORD: Theme = Theme {
    name:       "nord",
    bg:         Color::Rgb(46, 52, 64),
    bg_panel:   Color::Rgb(59, 66, 82),
    bg_input:   Color::Rgb(67, 76, 94),
    fg:         Color::Rgb(236, 239, 244),
    fg_dim:     Color::Rgb(129, 161, 193),
    fg_user:    Color::Rgb(136, 192, 208),
    fg_ai:      Color::Rgb(163, 190, 140),
    fg_system:  Color::Rgb(129, 161, 193),
    accent:     Color::Rgb(136, 192, 208),
    border:     Color::Rgb(76, 86, 106),
    highlight:  Color::Rgb(136, 192, 208),
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_all_themes_load() {
        let names = ["mira-dark", "mira-light", "dracula", "gruvbox", "nord"];
        for n in names {
            let t = Theme::by_name(n);
            assert!(t.is_some(), "theme '{}' not found", n);
        }
    }
    #[test]
    fn test_theme_border_style_returns_style() {
        let t = Theme::by_name("mira-dark").unwrap();
        let _ = t.border_style();
        let _ = t.title_style();
        let _ = t.user_msg_style();
        let _ = t.ai_msg_style();
        let _ = t.input_style();
        let _ = t.status_style();
    }
}
