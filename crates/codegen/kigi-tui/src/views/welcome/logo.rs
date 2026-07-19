//! Logo component — a procedurally generated braille moon that waxes and
//! wanes through a full lunation, echoing the Kimi CLI's moon-phase spinner.
//!
//! The disc is rasterized into the 2×4 dot grid of braille cells (dots are
//! close to square in a typical terminal cell), so the moon stays round at
//! any size and needs no art assets. The lit region follows the standard
//! phase model: the terminator is the ellipse `x = cos(2πp)·√(1−y²)`, with
//! sunlight arriving from the right while waxing and from the left while
//! waning. The dark limb keeps a faint outline ring so the silhouette never
//! disappears at new moon. A fixed map of lunar maria textures the disc:
//! dark blotches on the sunlit side, faint gray patches on the dark limb.
//!
//! Hidden entirely on legacy Windows consoles: the U+2800 braille block is
//! not covered by the ConHost raster fonts and would render as tofu.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::render::color::blend_color;
use crate::theme::Theme;

/// Full-size moon (hero box and tall stacked layouts), in braille cells.
const FULL_COLS: u16 = 20;
const FULL_ROWS: u16 = 10;
/// Small moon for short windows, in braille cells.
const SMALL_COLS: u16 = 10;
const SMALL_ROWS: u16 = 5;

/// Height at or above which the small moon is shown (below it, no logo).
const SMALL_LOGO_MIN_HEIGHT: u16 = 22;
/// Height at or above which the full moon is shown.
const FULL_LOGO_MIN_HEIGHT: u16 = 26;

/// Seconds for one full lunation (new → full → new). Deliberately slow —
/// the welcome screen should breathe, not blink.
const LUNATION_SECS: f32 = 8.0;

/// Redraw cadence in frames per second. The terminator moves slowly, so a
/// modest rate looks smooth while sparing the long-lived welcome screen
/// from full-rate repaints.
const MOON_FPS: f32 = 12.0;

/// Brightness of lit dots (blend factor from the resting gray toward the
/// bright text color).
const LIT_OPACITY: f32 = 0.85;
/// Extra breathing applied to lit dots.
const PULSE: f32 = 0.10;
/// Breathing period in seconds.
const PULSE_SECS: f32 = 5.0;
/// Squared inner radius (normalized) of the dark-limb outline ring.
const RING_INNER_SQ: f32 = 0.82;

/// Lunar maria as `(cx, cy, radius²)` in normalized disc coordinates
/// (x right, y down), loosely after the near side's real maria. On the
/// sunlit disc a mare dot is drawn in the resting gray (a dark blotch);
/// on the dark limb it is drawn in the same gray, which reads as a faint
/// light patch against the empty limb.
const MARIA: &[(f32, f32, f32)] = &[
    (-0.40, -0.42, 0.018), // Imbrium
    (0.12, -0.50, 0.008),  // Serenitatis
    (0.40, -0.22, 0.012),  // Tranquillitatis
    (0.55, 0.20, 0.005),   // Fecunditatis
    (0.62, -0.42, 0.004),  // Crisium
    (-0.58, 0.05, 0.010),  // Procellarum
    (-0.28, 0.40, 0.006),  // Nubium
    (0.08, 0.12, 0.004),   // Vaporum
];

fn in_mare(dx: f32, dy: f32) -> bool {
    MARIA.iter().any(|&(mx, my, r_sq)| {
        let ex = dx - mx;
        let ey = dy - my;
        ex * ex + ey * ey <= r_sq
    })
}

/// One logo size tier, in braille cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct MoonSize {
    cols: u16,
    rows: u16,
}

const FULL: MoonSize = MoonSize {
    cols: FULL_COLS,
    rows: FULL_ROWS,
};
const SMALL: MoonSize = MoonSize {
    cols: SMALL_COLS,
    rows: SMALL_ROWS,
};

fn pick_logo(window_height: u16) -> Option<MoonSize> {
    pick_logo_for(window_height, logo_hidden())
}

/// Pure tier selection so tests can drive the legacy-console flag directly.
fn pick_logo_for(window_height: u16, hidden: bool) -> Option<MoonSize> {
    if hidden || window_height < SMALL_LOGO_MIN_HEIGHT {
        None
    } else if window_height < FULL_LOGO_MIN_HEIGHT {
        Some(SMALL)
    } else {
        Some(FULL)
    }
}

/// The braille moon has no ASCII stand-in; see the module doc.
fn logo_hidden() -> bool {
    crate::glyphs::is_legacy_windows_console()
}

/// Animation phase in seconds since the first render. Wall-clock based so the
/// lunation speed is independent of the frame rate.
fn anim_phase_secs() -> f32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

/// Quantized animation frame for the current wall-clock phase. The welcome
/// screen redraws only when this advances, throttling the animation to
/// ~[`MOON_FPS`] rather than the full event-loop tick rate. Pinned to 0 when
/// the logo is hidden.
pub fn shimmer_frame() -> u64 {
    if logo_hidden() {
        return 0;
    }
    (anim_phase_secs() * MOON_FPS) as u64
}

/// Lunation phase in `[0, 1)`: 0 = new moon, 0.5 = full moon.
fn phase_now() -> f32 {
    (anim_phase_secs() / LUNATION_SECS).fract()
}

/// Whether the normalized disc point `(dx, dy)` is sunlit at phase `p`.
///
/// `x_edge = √(1−dy²)` is the disc edge at that height; the terminator sits
/// at `k·x_edge` with `k = cos(2πp)`. Waxing light grows from the right,
/// waning light retreats to the left.
fn dot_lit(dx: f32, dy: f32, p: f32) -> bool {
    let k = (std::f32::consts::TAU * p).cos();
    let x_edge = (1.0 - dy * dy).max(0.0).sqrt();
    if p < 0.5 {
        dx >= k * x_edge
    } else {
        dx <= -k * x_edge
    }
}

/// One rendered braille cell of the moon: the glyph plus whether any of its
/// dots are sunlit (drives the cell color).
#[derive(Clone, Copy)]
struct MoonCell {
    ch: char,
    lit: bool,
}

/// Rasterize the moon at `size` and phase `p` into rows of braille cells.
/// `None` cells are fully empty (outside the disc).
fn moon_cells(size: MoonSize, p: f32) -> Vec<Vec<Option<MoonCell>>> {
    // Braille dot bit values by (dot_col, dot_row) within a cell.
    const DOT_BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

    let dots_w = (size.cols * 2) as i32;
    let dots_h = (size.rows * 4) as i32;
    let cx = (dots_w as f32 - 1.0) / 2.0;
    let cy = (dots_h as f32 - 1.0) / 2.0;
    let r = (dots_w.min(dots_h) as f32) / 2.0 - 0.5;

    (0..size.rows as i32)
        .map(|cell_row| {
            (0..size.cols as i32)
                .map(|cell_col| {
                    let mut lit_mask = 0u32;
                    let mut dark_mask = 0u32;
                    for (dot_col, col_bits) in DOT_BITS.iter().enumerate() {
                        for (dot_row, bit) in col_bits.iter().enumerate() {
                            let x = cell_col * 2 + dot_col as i32;
                            let y = cell_row * 4 + dot_row as i32;
                            let dx = (x as f32 - cx) / r;
                            let dy = (y as f32 - cy) / r;
                            let d_sq = dx * dx + dy * dy;
                            if d_sq > 1.0 {
                                continue;
                            }
                            let mare = in_mare(dx, dy);
                            if dot_lit(dx, dy, p) {
                                if mare {
                                    // Dark blotch on the sunlit disc.
                                    dark_mask |= bit;
                                } else {
                                    lit_mask |= bit;
                                }
                            } else if mare || d_sq >= RING_INNER_SQ {
                                // Dark limb: outline ring plus faint maria.
                                dark_mask |= bit;
                            }
                        }
                    }
                    // A braille cell holds a single color, so a cell with any
                    // sunlit dots renders only those (mare dots in it stay
                    // background-dark); otherwise its dark dots render gray.
                    let (mask, lit) = if lit_mask != 0 {
                        (lit_mask, true)
                    } else {
                        (dark_mask, false)
                    };
                    (mask != 0).then(|| MoonCell {
                        ch: char::from_u32(0x2800 + mask).expect("braille block"),
                        lit,
                    })
                })
                .collect()
        })
        .collect()
}

fn render_into(area: Rect, buf: &mut Buffer, theme: &Theme, size: MoonSize) {
    let secs = anim_phase_secs();
    let pulse = PULSE * (0.5 - 0.5 * (std::f32::consts::TAU * secs / PULSE_SECS).cos());
    let lit_opacity = (LIT_OPACITY + pulse).clamp(0.0, 1.0);

    let base = theme.gray;
    let hilite = theme.text_primary;
    let lit_color = blend_color(base, hilite, lit_opacity).unwrap_or(base);

    // Adjacent cells with the same color share one Span to hold down the
    // per-frame allocation.
    let logo_lines: Vec<Line> = moon_cells(size, phase_now())
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;
            for cell in row {
                let (ch, color) = match cell {
                    Some(MoonCell { ch, lit: true }) => (ch, lit_color),
                    Some(MoonCell { ch, lit: false }) => (ch, base),
                    None => ('\u{2800}', base),
                };
                if run_color != Some(color) {
                    if let Some(prev) = run_color {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            Style::default().fg(prev),
                        ));
                    }
                    run_color = Some(color);
                }
                run.push(ch);
            }
            if let Some(prev) = run_color {
                spans.push(Span::styled(run, Style::default().fg(prev)));
            }
            Line::from(spans).alignment(Alignment::Center)
        })
        .collect();
    Paragraph::new(logo_lines).render(area, buf);
}

pub fn logo_line_count(window_height: u16) -> u16 {
    pick_logo(window_height).map_or(0, |s| s.rows)
}

pub fn logo_visual_width(window_height: u16) -> u16 {
    pick_logo(window_height).map_or(24, |s| s.cols)
}

pub fn render_logo(area: Rect, buf: &mut Buffer, theme: &Theme, window_height: u16) {
    if let Some(size) = pick_logo(window_height) {
        render_into(area, buf, theme, size);
    }
}

/// The hero box always shows the full moon: it is laid out beside the menu,
/// so it fits whenever the box does. These report and render that logo
/// directly, independent of the height-based [`pick_logo`] tiers used by the
/// stacked layout. When [`logo_hidden`], they report 0 and render nothing.
pub fn full_logo_line_count() -> u16 {
    full_logo_line_count_for(logo_hidden())
}

fn full_logo_line_count_for(hidden: bool) -> u16 {
    if hidden { 0 } else { FULL.rows }
}

pub fn full_logo_visual_width() -> u16 {
    full_logo_visual_width_for(logo_hidden())
}

fn full_logo_visual_width_for(hidden: bool) -> u16 {
    if hidden { 0 } else { FULL.cols }
}

pub fn render_full_logo(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if !logo_hidden() {
        render_into(area, buf, theme, FULL);
    }
}

/// Line count of the small moon used in minimal's committed welcome card
/// (0 on a legacy Windows console, where braille art is suppressed).
pub fn compact_logo_line_count() -> u16 {
    if logo_hidden() { 0 } else { SMALL.rows }
}

/// Render the small moon (centered) into `area` for minimal's welcome card.
/// No-op when the logo is hidden.
pub fn render_compact_logo(area: Rect, buf: &mut Buffer, theme: &Theme) {
    if !logo_hidden() {
        render_into(area, buf, theme, SMALL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count sunlit dots across the whole disc at phase `p`.
    fn lit_dots(p: f32) -> usize {
        let dots_w = (FULL.cols * 2) as i32;
        let dots_h = (FULL.rows * 4) as i32;
        let cx = (dots_w as f32 - 1.0) / 2.0;
        let cy = (dots_h as f32 - 1.0) / 2.0;
        let r = (dots_w.min(dots_h) as f32) / 2.0 - 0.5;
        let mut count = 0;
        for y in 0..dots_h {
            for x in 0..dots_w {
                let dx = (x as f32 - cx) / r;
                let dy = (y as f32 - cy) / r;
                if dx * dx + dy * dy <= 1.0 && dot_lit(dx, dy, p) {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn logo_sizes_by_height() {
        assert!(pick_logo_for(SMALL_LOGO_MIN_HEIGHT - 1, false).is_none());
        assert_eq!(pick_logo_for(SMALL_LOGO_MIN_HEIGHT, false), Some(SMALL));
        assert_eq!(pick_logo_for(FULL_LOGO_MIN_HEIGHT - 1, false), Some(SMALL));
        assert_eq!(pick_logo_for(FULL_LOGO_MIN_HEIGHT, false), Some(FULL));
    }

    // The braille moon has no legacy-safe stand-in, so every height tier must
    // collapse to no logo when the legacy-console flag is set.
    #[test]
    fn logo_hidden_on_legacy_console_at_every_height() {
        for h in [0, SMALL_LOGO_MIN_HEIGHT, FULL_LOGO_MIN_HEIGHT, u16::MAX] {
            assert!(pick_logo_for(h, true).is_none(), "height {h}");
        }
    }

    #[test]
    fn hero_box_always_uses_full_logo() {
        // The box renders the full moon regardless of height (it's laid out
        // beside the menu), and it's the large variant — never the small one.
        assert_eq!(full_logo_line_count_for(false), FULL.rows);
        assert_eq!(full_logo_visual_width_for(false), FULL.cols);
        assert!(full_logo_line_count_for(false) > SMALL.rows);
        assert!(full_logo_visual_width_for(false) > SMALL.cols);
    }

    #[test]
    fn full_logo_helpers_collapse_when_hidden() {
        assert_eq!(full_logo_line_count_for(true), 0);
        assert_eq!(full_logo_visual_width_for(true), 0);
    }

    #[test]
    fn compact_logo_is_the_small_moon() {
        if !logo_hidden() {
            assert_eq!(compact_logo_line_count(), SMALL.rows);
            assert!(compact_logo_line_count() < FULL.rows);
        } else {
            assert_eq!(compact_logo_line_count(), 0);
        }
    }

    #[test]
    fn full_moon_lights_the_whole_disc_and_new_moon_none() {
        let full = lit_dots(0.5);
        assert!(full > 0, "full moon must light the disc");
        assert_eq!(lit_dots(0.0), 0, "new moon must be fully dark");
        // Quarter phases light about half the disc.
        let quarter = lit_dots(0.25);
        assert!(
            (full / 3..=2 * full / 3).contains(&quarter),
            "first quarter lit {quarter} should be near half of full {full}"
        );
    }

    #[test]
    fn illumination_waxes_then_wanes() {
        // Monotone growth to full, then monotone decay — the terminator
        // sweeps once across the disc per lunation.
        let phases = [0.05, 0.15, 0.25, 0.35, 0.45];
        for w in phases.windows(2) {
            assert!(
                lit_dots(w[0]) < lit_dots(w[1]),
                "waxing must grow: p={} vs p={}",
                w[0],
                w[1]
            );
        }
        let phases = [0.55, 0.65, 0.75, 0.85, 0.95];
        for w in phases.windows(2) {
            assert!(
                lit_dots(w[0]) > lit_dots(w[1]),
                "waning must shrink: p={} vs p={}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn waxing_lights_the_right_limb_first() {
        // Shortly after new moon the crescent must hang on the right side.
        assert!(dot_lit(0.95, 0.0, 0.1), "right limb lit while waxing");
        assert!(!dot_lit(-0.95, 0.0, 0.1), "left limb dark while waxing");
        // And on the left side while waning.
        assert!(dot_lit(-0.95, 0.0, 0.9), "left limb lit while waning");
        assert!(!dot_lit(0.95, 0.0, 0.9), "right limb dark while waning");
    }

    #[test]
    fn new_moon_keeps_a_silhouette_ring() {
        // Even at p=0 the rasterizer must emit outline cells so the moon
        // never vanishes from the welcome screen.
        let cells = moon_cells(FULL, 0.0);
        let drawn = cells.iter().flatten().flatten().count();
        assert!(drawn > 0, "new moon must keep an outline ring");
        assert!(
            cells.iter().flatten().flatten().all(|c| !c.lit),
            "no cell may be lit at new moon"
        );
    }

    #[test]
    fn maria_texture_the_disc_in_both_extremes() {
        // Full moon: mare dots stay dark, so the drawn glyphs must cover
        // fewer dots than the geometric disc (lit_dots ignores maria).
        let full = moon_cells(FULL, 0.5);
        let drawn_dots: u32 = full
            .iter()
            .flatten()
            .flatten()
            .map(|c| (c.ch as u32 - 0x2800).count_ones())
            .sum();
        assert!(
            (drawn_dots as usize) < lit_dots(0.5),
            "full moon must keep dark maria holes"
        );
        // New moon: maria show as drawn (gray) cells well inside the outline
        // ring — Procellarum sits around cell (5, 4) on the full-size grid.
        let new = moon_cells(FULL, 0.0);
        assert!(
            new[5][4].is_some(),
            "new moon must show maria inside the ring"
        );
        assert!(in_mare(-0.58, 0.05), "Procellarum anchors the maria map");
        assert!(!in_mare(0.0, 0.85), "south pole stays mare-free");
    }

    #[test]
    fn moon_raster_is_round_and_fills_the_grid() {
        // The disc must span (nearly) the whole cell grid in both axes at
        // full moon — this is what "make it larger" bought us.
        let cells = moon_cells(FULL, 0.5);
        assert_eq!(cells.len(), FULL.rows as usize);
        assert!(cells.iter().all(|r| r.len() == FULL.cols as usize));
        let first_row_drawn = cells.first().unwrap().iter().flatten().count();
        let mid_row_drawn = cells[FULL.rows as usize / 2].iter().flatten().count();
        assert!(first_row_drawn > 0, "top of the disc must be drawn");
        assert!(
            mid_row_drawn > first_row_drawn,
            "equator must be wider than the pole (round disc)"
        );
        assert_eq!(mid_row_drawn, FULL.cols as usize, "equator spans the grid");
    }
}
