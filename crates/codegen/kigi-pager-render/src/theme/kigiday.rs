//! KigiDay theme — neutral gray base (light) with deepened accent colors.
//!
//! Light counterpart to KigiNight. Backgrounds and text use a neutral
//! grayscale ramp (no blue/warm tint). Accent colors are the same hue
//! family as KigiNight but deepened for contrast on light backgrounds.

use ratatui::style::{Color, Modifier};

use super::tokyonight::Theme;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

#[allow(dead_code)]
mod palette {
    use super::*;

    // Backgrounds (neutral light grays)
    // #f5f5f5 — brightest (terminal bg)
    pub const BG: Color = rgb(245, 245, 245);
    pub const BG_DARK: Color = rgb(240, 240, 240);
    pub const BG_STORM_DARK: Color = rgb(234, 234, 234);
    // #eeeeee — main bg
    pub const BG_STORM: Color = rgb(238, 238, 238);
    // #dedede — highlight bg
    pub const BG_HIGHLIGHT: Color = rgb(222, 222, 222);

    // Text / grays (neutral dark)
    // #262626 — primary text
    pub const FG: Color = rgb(38, 38, 38);
    // #444444 — secondary text
    pub const FG_DARK: Color = rgb(68, 68, 68);
    pub const FG_GUTTER: Color = rgb(178, 178, 178);
    // #767676 — muted
    pub const COMMENT: Color = rgb(118, 118, 118);
    // #8e8e8e — medium gray
    pub const DARK3: Color = rgb(142, 142, 142);
    // #626262 — bright gray
    pub const DARK5: Color = rgb(98, 98, 98);

    // Accent colors (deepened for light-background contrast)
    pub const BLUE: Color = rgb(47, 100, 210);
    pub const BLUE0: Color = rgb(40, 68, 138);
    pub const BLUE1: Color = rgb(15, 135, 162);
    pub const CYAN: Color = rgb(0, 130, 170);
    pub const GREEN: Color = rgb(55, 142, 35);
    pub const GREEN1: Color = rgb(12, 148, 124);
    pub const MAGENTA: Color = rgb(125, 75, 198);
    pub const ORANGE: Color = rgb(195, 105, 30);
    pub const PURPLE: Color = rgb(108, 62, 178);
    pub const RED: Color = rgb(205, 48, 72);
    pub const RED1: Color = rgb(175, 35, 35);
    pub const TEAL: Color = rgb(10, 142, 112);
    pub const YELLOW: Color = rgb(162, 118, 18);

    // #F5DADE — diff delete bg
    pub const RED_LIGHT: Color = rgb(245, 218, 222);
    // #DAF2DC — diff insert bg
    pub const GREEN_LIGHT: Color = rgb(218, 242, 220);
}
use palette::*;

impl Theme {
    pub const fn kigiday() -> Self {
        Self {
            bg_base: BG_STORM,
            bg_light: BG_HIGHLIGHT,
            bg_dark: rgb(228, 228, 228),
            bg_highlight: BG_HIGHLIGHT,
            bg_hover: rgb(208, 208, 208),
            bg_terminal: BG,

            accent_user: FG_DARK,
            accent_assistant: MAGENTA,
            accent_thinking: MAGENTA,
            accent_tool: DARK5,
            accent_system: BLUE,
            accent_error: RED,
            accent_success: GREEN,
            accent_running: MAGENTA,
            accent_skill: BLUE,

            text_primary: FG,
            text_secondary: FG_DARK,

            // #a5a5a5 — slightly darker than FG_GUTTER
            gray_dim: rgb(165, 165, 165),
            gray: COMMENT,
            gray_bright: DARK5,

            command: YELLOW,
            path: ORANGE,
            running: CYAN,
            warning: YELLOW,

            fuzzy_accent: BLUE,

            // #A8780A — deep golden
            accent_plan: rgb(168, 120, 10),

            // deep violet (readable on light bg)
            accent_verify: rgb(120, 80, 160),

            accent_feedback: GREEN1,

            // #4CAF50 — Material Design green (readable on light bg)
            accent_remember: rgb(76, 175, 80),

            selection_border: rgb(185, 185, 190),
            // #C8C8CD — dimmer prompt chrome
            prompt_border: rgb(200, 200, 205),
            // #A5A5AF — darker (more apparent) when focused
            prompt_border_active: rgb(165, 165, 175),
            hover_border: rgb(212, 212, 216),

            accent_model: TEAL,

            scrollbar_bg: BG_STORM_DARK,
            scrollbar_fg: BG_HIGHLIGHT,

            diff_delete_bg: RED_LIGHT,
            diff_delete_fg: RED,
            diff_insert_bg: GREEN_LIGHT,
            diff_insert_fg: GREEN,
            diff_equal_fg: COMMENT,
            diff_gutter_fg: COMMENT,

            bg_visual: rgb(198, 198, 198),

            paste_bg: BG_HIGHLIGHT,
            paste_fg: FG_DARK,
            paste_dim: FG_GUTTER,

            md_heading_h1: TEAL,
            md_heading_h1_mod: Modifier::BOLD,
            md_heading_h2: BLUE,
            md_heading_h2_mod: Modifier::BOLD,
            md_heading_h3: PURPLE,
            md_heading_h3_mod: Modifier::BOLD,
            md_heading_h4: DARK5,
            md_heading_h4_mod: Modifier::BOLD,
            md_heading_h5: COMMENT,
            md_heading_h5_mod: Modifier::BOLD,
            md_heading_h6: DARK3,
            md_heading_h6_mod: Modifier::empty(),
            md_code: BLUE1,
            md_task_checked: GREEN,
            md_task_unchecked: FG_DARK,
            md_muted: COMMENT,
            md_code_bg: rgb(228, 228, 228),
            md_text: FG_DARK,
            // #2F64D2 -- deep blue for light bg
            link_fg: BLUE,
        }
    }
}
