use ratatui::style::Color;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub label: &'static str,

    // Status colors
    pub idle: Color,
    pub working: Color,
    pub awaiting_a: Color,
    pub awaiting_b: Color,
    pub spawning: Color,
    pub exited: Color,

    // UI accents
    pub accent: Color,
    pub title: Color,
    pub title_fallback: Color,
    pub body: Color,
    pub dim: Color,
    pub cwd: Color,
    pub model: Color,
    pub cost: Color,
    pub tool: Color,

    // Context bar fills (low / mid / high)
    pub context_low: Color,
    pub context_mid: Color,
    pub context_high: Color,

    // Mode-specific colors
    pub footer_scroll: Color,
    pub footer_prefix: Color,

    // Subtle row-wide bg used to flag "needs you" sessions in the sidebar.
    // Dark variant of the awaiting palette; should be muted enough not to
    // overwhelm the foreground text.
    pub awaiting_bg: Color,
}

pub const DEFAULT: Theme = Theme {
    name: "default",
    label: "default",
    idle: Color::Green,
    working: Color::Yellow,
    awaiting_a: Color::LightMagenta,
    awaiting_b: Color::Magenta,
    spawning: Color::DarkGray,
    exited: Color::DarkGray,
    accent: Color::Cyan,
    title: Color::White,
    title_fallback: Color::Gray,
    body: Color::Gray,
    dim: Color::DarkGray,
    cwd: Color::Blue,
    model: Color::Magenta,
    cost: Color::Green,
    tool: Color::Cyan,
    context_low: Color::Green,
    context_mid: Color::Yellow,
    context_high: Color::Red,
    footer_scroll: Color::Magenta,
    footer_prefix: Color::Yellow,
    awaiting_bg: Color::Indexed(53),
};

pub const CATPPUCCIN: Theme = Theme {
    name: "catppuccin",
    label: "catppuccin mocha",
    idle: Color::Rgb(166, 227, 161),
    working: Color::Rgb(249, 226, 175),
    awaiting_a: Color::Rgb(245, 194, 231),
    awaiting_b: Color::Rgb(203, 166, 247),
    spawning: Color::Rgb(108, 112, 134),
    exited: Color::Rgb(88, 91, 112),
    accent: Color::Rgb(148, 226, 213),
    title: Color::Rgb(205, 214, 244),
    title_fallback: Color::Rgb(166, 173, 200),
    body: Color::Rgb(186, 194, 222),
    dim: Color::Rgb(108, 112, 134),
    cwd: Color::Rgb(137, 180, 250),
    model: Color::Rgb(203, 166, 247),
    cost: Color::Rgb(166, 227, 161),
    tool: Color::Rgb(148, 226, 213),
    context_low: Color::Rgb(166, 227, 161),
    context_mid: Color::Rgb(249, 226, 175),
    context_high: Color::Rgb(243, 139, 168),
    footer_scroll: Color::Rgb(203, 166, 247),
    footer_prefix: Color::Rgb(249, 226, 175),
    awaiting_bg: Color::Rgb(49, 30, 60),
};

pub const TOKYO_NIGHT: Theme = Theme {
    name: "tokyo-night",
    label: "tokyo night",
    idle: Color::Rgb(158, 206, 106),
    working: Color::Rgb(224, 175, 104),
    awaiting_a: Color::Rgb(247, 118, 142),
    awaiting_b: Color::Rgb(187, 154, 247),
    spawning: Color::Rgb(86, 95, 137),
    exited: Color::Rgb(65, 72, 104),
    accent: Color::Rgb(125, 207, 255),
    title: Color::Rgb(192, 202, 245),
    title_fallback: Color::Rgb(154, 165, 206),
    body: Color::Rgb(169, 177, 214),
    dim: Color::Rgb(86, 95, 137),
    cwd: Color::Rgb(122, 162, 247),
    model: Color::Rgb(187, 154, 247),
    cost: Color::Rgb(158, 206, 106),
    tool: Color::Rgb(125, 207, 255),
    context_low: Color::Rgb(158, 206, 106),
    context_mid: Color::Rgb(224, 175, 104),
    context_high: Color::Rgb(247, 118, 142),
    footer_scroll: Color::Rgb(187, 154, 247),
    footer_prefix: Color::Rgb(224, 175, 104),
    awaiting_bg: Color::Rgb(45, 30, 65),
};

pub const NORD: Theme = Theme {
    name: "nord",
    label: "nord",
    idle: Color::Rgb(163, 190, 140),
    working: Color::Rgb(235, 203, 139),
    awaiting_a: Color::Rgb(180, 142, 173),
    awaiting_b: Color::Rgb(143, 188, 187),
    spawning: Color::Rgb(76, 86, 106),
    exited: Color::Rgb(67, 76, 94),
    accent: Color::Rgb(136, 192, 208),
    title: Color::Rgb(236, 239, 244),
    title_fallback: Color::Rgb(216, 222, 233),
    body: Color::Rgb(216, 222, 233),
    dim: Color::Rgb(76, 86, 106),
    cwd: Color::Rgb(129, 161, 193),
    model: Color::Rgb(180, 142, 173),
    cost: Color::Rgb(163, 190, 140),
    tool: Color::Rgb(143, 188, 187),
    context_low: Color::Rgb(163, 190, 140),
    context_mid: Color::Rgb(235, 203, 139),
    context_high: Color::Rgb(191, 97, 106),
    footer_scroll: Color::Rgb(180, 142, 173),
    footer_prefix: Color::Rgb(235, 203, 139),
    awaiting_bg: Color::Rgb(50, 38, 60),
};

pub const MONO: Theme = Theme {
    name: "mono",
    label: "monochrome",
    idle: Color::Rgb(220, 220, 220),
    working: Color::Rgb(255, 255, 255),
    awaiting_a: Color::Rgb(255, 255, 255),
    awaiting_b: Color::Rgb(180, 180, 180),
    spawning: Color::Rgb(100, 100, 100),
    exited: Color::Rgb(80, 80, 80),
    accent: Color::Rgb(220, 220, 220),
    title: Color::Rgb(255, 255, 255),
    title_fallback: Color::Rgb(160, 160, 160),
    body: Color::Rgb(190, 190, 190),
    dim: Color::Rgb(100, 100, 100),
    cwd: Color::Rgb(200, 200, 200),
    model: Color::Rgb(180, 180, 180),
    cost: Color::Rgb(220, 220, 220),
    tool: Color::Rgb(220, 220, 220),
    context_low: Color::Rgb(220, 220, 220),
    context_mid: Color::Rgb(180, 180, 180),
    context_high: Color::Rgb(255, 255, 255),
    footer_scroll: Color::Rgb(220, 220, 220),
    footer_prefix: Color::Rgb(220, 220, 220),
    awaiting_bg: Color::Rgb(40, 40, 40),
};

pub static ALL: &[Theme] = &[DEFAULT, CATPPUCCIN, TOKYO_NIGHT, NORD, MONO];

static ACTIVE: LazyLock<RwLock<Theme>> = LazyLock::new(|| RwLock::new(DEFAULT));

pub fn current() -> Theme {
    ACTIVE.read().unwrap().clone()
}

pub fn set(t: Theme) {
    *ACTIVE.write().unwrap() = t;
}

fn persist_path() -> Option<PathBuf> {
    crate::paths::state_dir().ok().map(|p| p.join("theme"))
}

pub fn load() {
    if let Some(p) = persist_path() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            let name = s.trim();
            if let Some(t) = ALL.iter().find(|t| t.name == name) {
                set(t.clone());
            }
        }
    }
}

pub fn save(t: &Theme) {
    if let Some(p) = persist_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, t.name);
    }
}
