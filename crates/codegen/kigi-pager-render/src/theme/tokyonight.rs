//! TokyoNight theme for the pager.
//!
//! All colors come from the `Theme` struct. NO hardcoded colors elsewhere.
//!
//! The named constants below match the TokyoNight Night/Storm palette from
//! `kigi-tui/src/ui/style.rs` for consistency. The `Theme` struct maps
//! these constants to semantic roles.

use ratatui::style::{Color, Modifier, Style};

/// Helper for concise const Color::Rgb definitions.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

// TokyoNight palette constants (Night/Storm variant).
// Keep in sync with kigi-tui TokyoNightNight.
#[allow(dead_code)]
pub mod palette {
    use super::*;
    // #1a1b26 - Night
    pub const BG: Color = rgb(26, 27, 38);
    pub const BG_DARK: Color = rgb(22, 22, 30);
    pub const BG_HIGHLIGHT: Color = rgb(41, 46, 66);
    // #24283b - Storm
    pub const BG_STORM: Color = rgb(36, 40, 59);
    pub const BG_STORM_DARK: Color = rgb(31, 35, 53);
    pub const FG: Color = rgb(192, 202, 245);
    pub const FG_DARK: Color = rgb(169, 177, 214);
    pub const FG_GUTTER: Color = rgb(59, 66, 97);
    pub const COMMENT: Color = rgb(86, 95, 137);
    pub const DARK3: Color = rgb(84, 92, 126);
    pub const DARK5: Color = rgb(115, 122, 162);
    pub const BLUE: Color = rgb(122, 162, 247);
    pub const BLUE0: Color = rgb(61, 89, 161);
    pub const BLUE1: Color = rgb(42, 195, 222);
    pub const CYAN: Color = rgb(125, 207, 255);
    pub const GREEN: Color = rgb(158, 206, 106);
    pub const GREEN1: Color = rgb(115, 218, 202);
    pub const MAGENTA: Color = rgb(187, 154, 247);
    pub const ORANGE: Color = rgb(255, 158, 100);
    pub const PURPLE: Color = rgb(157, 124, 216);
    pub const RED: Color = rgb(247, 118, 142);
    pub const RED1: Color = rgb(219, 75, 75);
    pub const TEAL: Color = rgb(26, 188, 156);
    pub const YELLOW: Color = rgb(224, 175, 104);
}
use palette::*;

/// Theme for v3 pager rendering.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // Backgrounds
    pub bg_base: Color,
    pub bg_light: Color,
    pub bg_dark: Color,
    pub bg_highlight: Color,
    // Mouse hover row in dropdowns — between bg_highlight and bg_visual
    pub bg_hover: Color,
    // For terminal output blocks (currently unused, using bg_dark instead)
    pub bg_terminal: Color,

    // Accent colors (for vertical lines)
    pub accent_user: Color,
    pub accent_assistant: Color,
    pub accent_thinking: Color,
    pub accent_tool: Color,
    pub accent_system: Color,
    pub accent_error: Color,
    pub accent_success: Color,
    // For tools that are currently running
    pub accent_running: Color,
    // For skill invocations (slash command skills)
    pub accent_skill: Color,

    // Text colors
    pub text_primary: Color,
    pub text_secondary: Color,

    // Gray scale (dim → medium → bright)
    // Every theme defines these three; they provide a consistent hierarchy
    // for secondary/meta text across all themes.
    // Dimmest — meta punctuation (`$`, `(+N/-M)`, etc.)
    pub gray_dim: Color,
    // Medium — muted text, comments, collapsed content
    pub gray: Color,
    // Brightest — tool accents, secondary labels
    pub gray_bright: Color,

    // Semantic colors
    // Yellow for shell commands
    pub command: Color,
    // Orange for file paths
    pub path: Color,
    // Cyan for running indicator
    pub running: Color,
    // Yellow/amber for warnings
    pub warning: Color,

    // Search
    // Highlight color for fuzzy search matches
    pub fuzzy_accent: Color,

    // Plan mode
    // Golden accent for plan mode indicator
    pub accent_plan: Color,

    // Context-window overhead category (context info block)
    // Violet accent — distinct from plan gold and feedback teal
    pub accent_verify: Color,

    // Feedback mode
    // Teal/green accent for feedback mode
    pub accent_feedback: Color,

    // Remember mode
    // Green accent for # remember mode
    pub accent_remember: Color,

    // Selection
    pub selection_border: Color,
    pub hover_border: Color,
    pub prompt_border: Color,
    pub prompt_border_active: Color,

    // Prompt info
    // Model name in prompt info line
    pub accent_model: Color,

    // Scrollbar
    pub scrollbar_bg: Color,
    pub scrollbar_fg: Color,

    // Diff colors
    pub diff_delete_bg: Color,
    pub diff_delete_fg: Color,
    pub diff_insert_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_equal_fg: Color,
    pub diff_gutter_fg: Color,

    // Visual selection / dropdown selection background
    pub bg_visual: Color,

    // Paste elements (chip + preview overlay)
    pub paste_bg: Color,
    pub paste_fg: Color,
    pub paste_dim: Color,

    // Markdown rendering colors — used by md_style.rs for headings, code
    // blocks, inline code, links, etc.  These default to the corresponding
    // top-level theme colors but can be overridden per-theme to customise
    // markdown appearance independently.
    pub md_heading_h1: Color,
    // H1 extra effects
    pub md_heading_h1_mod: Modifier,
    // H2 headings, task unchecked, tables
    pub md_heading_h2: Color,
    // H2 extra effects
    pub md_heading_h2_mod: Modifier,
    // H3 headings, code language tag
    pub md_heading_h3: Color,
    // H3 extra effects
    pub md_heading_h3_mod: Modifier,
    pub md_heading_h4: Color,
    // H4 extra effects
    pub md_heading_h4_mod: Modifier,
    // H5 headings, link titles
    pub md_heading_h5: Color,
    // H5 extra effects
    pub md_heading_h5_mod: Modifier,
    pub md_heading_h6: Color,
    // H6 extra effects
    pub md_heading_h6_mod: Modifier,
    // Inline code, code block delimiters
    pub md_code: Color,
    pub md_task_checked: Color,
    pub md_task_unchecked: Color,
    // Blockquotes, list items, rules, links
    pub md_muted: Color,
    // Code block background
    pub md_code_bg: Color,
    // Default body text (plain paragraphs, strong, emphasis)
    pub md_text: Color,
    // Clickable link text color
    pub link_fg: Color,
}

impl Theme {
    /// TokyoNight Storm theme.
    pub const fn tokyonight() -> Self {
        Self {
            bg_base: BG_STORM,
            bg_light: BG_HIGHLIGHT,
            bg_dark: BG_HIGHLIGHT,
            bg_highlight: BG_HIGHLIGHT,
            bg_hover: rgb(40, 49, 76),
            bg_terminal: BG,

            accent_user: BLUE,
            accent_assistant: MAGENTA,
            accent_thinking: FG_GUTTER,
            accent_tool: DARK5,
            accent_system: BLUE,
            accent_error: RED,
            accent_success: GREEN,
            accent_running: MAGENTA,
            accent_skill: rgb(100, 180, 170),

            text_primary: FG,
            text_secondary: FG_DARK,

            gray_dim: FG_GUTTER,
            gray: COMMENT,
            gray_bright: DARK5,

            command: YELLOW,
            path: ORANGE,
            running: CYAN,
            warning: YELLOW,

            fuzzy_accent: BLUE,

            // #E6B432 — golden
            accent_plan: rgb(230, 180, 50),

            // #bb9af7 — violet (distinct from plan / feedback)
            accent_verify: MAGENTA,

            // #73daca — warm teal/green
            accent_feedback: GREEN1,

            // #8BC34A — Material Design light green
            accent_remember: Color::Rgb(139, 195, 74),

            // #3A4873 — muted tokyonight blue
            selection_border: rgb(58, 72, 115),
            // #323E64 — dimmer prompt chrome
            prompt_border: rgb(60, 75, 120),
            // #4B5C8C — brighter when focused
            prompt_border_active: rgb(75, 92, 140),
            hover_border: rgb(55, 58, 80),

            accent_model: TEAL,

            scrollbar_bg: BG_STORM_DARK,
            scrollbar_fg: BG_HIGHLIGHT,

            diff_delete_bg: rgb(85, 15, 20),
            diff_delete_fg: RED,
            diff_insert_bg: rgb(15, 65, 20),
            diff_insert_fg: GREEN,
            diff_equal_fg: COMMENT,
            diff_gutter_fg: COMMENT,

            // #283457 — blue-tinted selection bg
            bg_visual: rgb(40, 52, 87),

            paste_bg: BG_STORM_DARK,
            paste_fg: FG_DARK,
            paste_dim: FG_GUTTER,
            // paste_bg: BG_HIGHLIGHT,
            // paste_fg: DARK5,
            // paste_dim: COMMENT,
            md_heading_h1: TEAL,
            md_heading_h1_mod: Modifier::BOLD,
            md_heading_h2: BLUE,
            md_heading_h2_mod: Modifier::BOLD,
            md_heading_h3: ORANGE,
            md_heading_h3_mod: Modifier::BOLD,
            md_heading_h4: RED,
            md_heading_h4_mod: Modifier::BOLD,
            md_heading_h5: GREEN,
            md_heading_h5_mod: Modifier::BOLD,
            md_heading_h6: MAGENTA,
            md_heading_h6_mod: Modifier::BOLD,
            md_code: GREEN1,
            md_task_checked: CYAN,
            md_task_unchecked: BLUE,
            md_muted: COMMENT,
            md_code_bg: BG_HIGHLIGHT,
            md_text: FG,
            link_fg: BLUE,
        }
    }

    /// Get a style with the given foreground color.
    pub const fn fg(&self, color: Color) -> Style {
        Style::new().fg(color)
    }

    /// Get a style with muted text (gray — medium).
    ///
    /// When `gray` is [`Color::Reset`] (terminal-native / minimal palette),
    /// de-emphasize with [`Modifier::DIM`] instead of painting ANSI bright
    /// black — dim scales the terminal's own default fg, so contrast stays
    /// polarity-safe. RGB themes keep an explicit gray foreground.
    pub const fn muted(&self) -> Style {
        match self.gray {
            Color::Reset => Style::new().add_modifier(Modifier::DIM),
            c => Style::new().fg(c),
        }
    }

    /// Style for OSC 8 hyperlink overlay text.
    pub fn link_style(&self) -> Style {
        Style::new()
            .fg(self.link_fg)
            .add_modifier(ratatui::style::Modifier::UNDERLINED)
    }

    /// Get a style with dim text (gray_dim — dimmest).
    ///
    /// Same Reset→DIM rule as [`Self::muted`] for the terminal-native palette.
    pub const fn dim(&self) -> Style {
        match self.gray_dim {
            Color::Reset => Style::new().add_modifier(Modifier::DIM),
            c => Style::new().fg(c),
        }
    }

    /// Get a style for primary text.
    pub const fn primary(&self) -> Style {
        Style::new().fg(self.text_primary)
    }

    /// Get a bold style.
    pub const fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

/// Compute animated brightness for a traveling wave effect.
///
/// Creates a wave that travels along the accent line. Each row has a fixed phase
/// offset so the wave appears to move smoothly regardless of block height.
///
/// # Arguments
/// - `tick`: Frame counter (increments each render tick)
/// - `row`: Current row within the block (0 = top)
/// - `wave_rows`: Rows per full wave cycle (e.g., 32)
/// - `speed`: Wave speed (radians per tick, e.g., 0.15)
///
/// # Returns
/// Brightness value in [0.0, 1.0] for this row at this tick.
pub fn wave_brightness(tick: u64, row: u16, wave_rows: u16, speed: f32) -> f32 {
    use std::f32::consts::PI;

    let rows_per_wave = wave_rows.max(1) as f32;
    let phase = (row as f32 / rows_per_wave) * 2.0 * PI;

    // Time-based oscillation
    let t = tick as f32 * speed;

    // sin²(t + phase) gives smooth 0-1 oscillation
    let sin_val = (t + phase).sin();
    sin_val * sin_val
}

/// Compute a smooth pulsing brightness for a single element (icon, indicator).
///
/// Unlike [`wave_brightness`] which creates a spatial wave across rows,
/// this is a simple temporal pulse: all elements sharing the same tick
/// pulse in unison.
///
/// # Arguments
/// - `tick`: Frame counter (increments each render tick, ~30fps)
/// - `speed`: Pulse speed (radians per tick). The returned value uses
///   `sin²`, which has period π, so the visible bright→dim→bright cycle
///   is `π / (speed * fps)`. At 30fps, `speed = 0.08` ≈ 1.3s per cycle;
///   for a 2.5s cycle pass `speed ≈ 0.042`.
///
/// # Returns
/// Brightness value in [0.0, 1.0].
pub fn pulse_brightness(tick: u64, speed: f32) -> f32 {
    let t = tick as f32 * speed;
    let sin_val = t.sin();
    sin_val * sin_val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokyonight_theme() {
        let theme = Theme::tokyonight();
        assert!(matches!(theme.bg_base, Color::Rgb(36, 40, 59)));
        assert!(matches!(theme.accent_user, Color::Rgb(122, 162, 247)));
    }
}
