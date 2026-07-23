//! Pure-Rust engine: Mermaid source -> SVG via the vendored `mermaid-to-svg`
//! (a dagre layout port), then [`crate::rasterize`] to PNG.

use mermaid_to_svg::{MermaidTheme as EngineTheme, render_mermaid_to_svg};

use crate::{MermaidEngine, MermaidError, MermaidTheme, RenderParams, RenderedDiagram};

/// The default, offline, pure-Rust engine.
#[derive(Debug, Default, Clone, Copy)]
pub struct PureRustEngine;

impl PureRustEngine {
    /// Construct a [`PureRustEngine`].
    pub fn new() -> Self {
        Self
    }
}

impl MermaidEngine for PureRustEngine {
    fn render(&self, source: &str, params: &RenderParams) -> Result<RenderedDiagram, MermaidError> {
        let svg = build_svg(source, params.theme)?;
        crate::rasterize(&svg, params)
    }
}

/// Mermaid source -> SVG (the layout half). A free function, not a method, so
/// the SVG can be asserted on directly in tests.
///
/// Errors here (unparseable source, unsupported diagram type) are not fatal: the
/// caller degrades them to the code-block fallback via [`crate::render_checked`].
fn build_svg(source: &str, theme: MermaidTheme) -> Result<String, MermaidError> {
    let engine_theme = theme_for(theme);
    render_mermaid_to_svg(source, Some(&engine_theme)).map_err(map_engine_error)
}

/// Map the vendored engine's errors onto ours, preserving the
/// parse/layout/unsupported split so observability stays honest.
fn map_engine_error(e: mermaid_to_svg::MermaidError) -> MermaidError {
    use mermaid_to_svg::MermaidError as E;
    match e {
        E::ParseError { .. } | E::InvalidDirection(_) | E::InvalidNodeShape(_) => {
            MermaidError::Parse(e.to_string())
        }
        E::DotGenerationError(_) | E::RenderError(_) => MermaidError::Layout(e.to_string()),
        E::UnsupportedDiagramType(_) => MermaidError::Unsupported(e.to_string()),
    }
}

/// Map [`MermaidTheme`] to a vendored-engine [`EngineTheme`].
///
/// Only the surface is overridden — to the crate's single-source-of-truth color
/// ([`crate::LIGHT_SURFACE`] / [`crate::DARK_SURFACE`]) so the painted SVG
/// background blends with the terminal scrollback the PNG sits on. The rest of
/// each preset's palette is used as-is.
fn theme_for(theme: MermaidTheme) -> EngineTheme {
    match theme {
        MermaidTheme::Light => {
            let mut t = EngineTheme::light();
            t.background = crate::LIGHT_SURFACE.to_hex();
            t
        }
        MermaidTheme::Dark => {
            let mut t = EngineTheme::dark();
            t.background = crate::DARK_SURFACE.to_hex();
            t
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RenderLimits, render_checked};

    #[test]
    fn flowchart_svg_contains_node_labels() {
        let svg = build_svg(
            "flowchart LR\n  A[Start] --> B[Finish]",
            MermaidTheme::Light,
        )
        .expect("flowchart should render to svg");
        assert!(svg.contains("<svg"), "must be an svg document");
        assert!(svg.contains("</svg>"));
        assert!(svg.contains("Start"), "node label 'Start' missing from svg");
        assert!(
            svg.contains("Finish"),
            "node label 'Finish' missing from svg"
        );
    }

    #[test]
    fn sequence_svg_contains_participants() {
        let svg = build_svg(
            "sequenceDiagram\n  Alice->>Bob: Hello\n  Bob-->>Alice: Hi",
            MermaidTheme::Light,
        )
        .expect("sequence should render");
        assert!(svg.contains("Alice"));
        assert!(svg.contains("Bob"));
    }

    #[test]
    fn render_produces_decodable_png_with_matching_dims() {
        let out = PureRustEngine::new()
            .render("flowchart LR\nA-->B-->C", &RenderParams::default())
            .expect("render should succeed");
        assert!(out.width_px > 0 && out.height_px > 0);
        let img = image::load_from_memory(&out.png).expect("output must be a valid png");
        assert_eq!(img.width(), out.width_px);
        assert_eq!(img.height(), out.height_px);
    }

    #[test]
    fn render_is_deterministic_in_process() {
        // The engine measures text with fixed char-width metrics (no system-font
        // dependence), so the same source+params reproduce identical bytes.
        let engine = PureRustEngine::new();
        let p = RenderParams::default();
        let a = engine.render("flowchart LR\nA-->B-->C", &p).expect("a");
        let b = engine.render("flowchart LR\nA-->B-->C", &p).expect("b");
        assert_eq!(
            a.png, b.png,
            "same source+params must yield identical png within a process"
        );
    }

    /// The back-edge (`Attempts -->|No| Enter`) routes back up into the cycle,
    /// the tricky case for flowchart edge routing.
    #[test]
    fn cyclic_login_flow_renders_with_arrowheads() {
        const EDGE_COUNT: usize = 8;
        let source = "flowchart TD\n\
            Start([User visits login page]) --> Enter[Enter username & password]\n\
            Enter --> Submit[Submit credentials]\n\
            Submit --> Validate{Credentials valid?}\n\
            Validate -->|No| Fail[Show error message]\n\
            Fail --> Attempts{Too many failed attempts?}\n\
            Attempts -->|Yes| Lock[Lock account]\n\
            Attempts -->|No| Enter\n\
            Validate -->|Yes| Session[Create session]";
        let svg = build_svg(source, MermaidTheme::Light).expect("cyclic flow renders");
        // Count rather than substring-match: a whole-document "contains an
        // arrowhead" check would still pass with the back-edge arrowhead gone.
        let arrowheads = svg.matches(r#"marker-end="url(#arrowhead)""#).count();
        assert_eq!(
            arrowheads, EDGE_COUNT,
            "every flowchart edge must carry an arrowhead marker",
        );
        for label in [
            "Enter username",
            "Submit credentials",
            "Credentials valid",
            "Too many failed attempts",
            "Lock account",
            "Create session",
        ] {
            assert!(svg.contains(label), "missing node label {label:?}");
        }
    }

    #[test]
    fn light_and_dark_render_to_different_pixels() {
        let engine = PureRustEngine::new();
        let light = engine
            .render(
                "flowchart LR\nA-->B",
                &RenderParams {
                    theme: MermaidTheme::Light,
                    ..Default::default()
                },
            )
            .expect("light");
        let dark = engine
            .render(
                "flowchart LR\nA-->B",
                &RenderParams {
                    theme: MermaidTheme::Dark,
                    ..Default::default()
                },
            )
            .expect("dark");
        assert_eq!(
            (light.width_px, light.height_px),
            (dark.width_px, dark.height_px),
            "same params must yield the same dimensions"
        );
        assert_ne!(
            light.png, dark.png,
            "themes must change the rendered pixels"
        );
    }

    #[test]
    fn theme_for_overrides_surface_per_theme() {
        assert_eq!(
            theme_for(MermaidTheme::Light).background,
            crate::LIGHT_SURFACE.to_hex()
        );
        assert_eq!(
            theme_for(MermaidTheme::Dark).background,
            crate::DARK_SURFACE.to_hex()
        );
        assert_ne!(
            theme_for(MermaidTheme::Light).background,
            theme_for(MermaidTheme::Dark).background,
        );
    }

    /// Only `MermaidError::Panic` is forbidden: garbage input may legitimately
    /// return other errors, which degrade to the code-block fallback.
    #[test]
    fn garbage_input_never_panics() {
        let engine = PureRustEngine::new();
        let limits = RenderLimits::default();
        let params = RenderParams::default();
        for garbage in [
            "",
            "@@@@",
            "%% only a comment",
            "flowchart\n\n\n",
            "????????",
            "\u{0}\u{1}\u{2}\u{3}",
            "flowchart LR\n  A[unterminated --> ",
            "pie\n  : :",
            "erDiagram\n  A ||",
            "sequenceDiagram\n  A->>",
        ] {
            let out = render_checked(&engine, garbage, &params, &limits);
            assert!(
                !matches!(out, Err(MermaidError::Panic(_))),
                "engine panicked on {garbage:?}: {out:?}"
            );
        }
    }

    #[test]
    fn engine_error_taxonomy_maps_every_arm() {
        use mermaid_to_svg::MermaidError as E;
        for parse in [
            E::ParseError {
                line: 1,
                message: "x".into(),
            },
            E::InvalidDirection("x".into()),
            E::InvalidNodeShape("x".into()),
        ] {
            assert!(
                matches!(map_engine_error(parse), MermaidError::Parse(_)),
                "expected Parse mapping",
            );
        }
        for layout in [
            E::DotGenerationError("x".into()),
            E::RenderError("x".into()),
        ] {
            assert!(
                matches!(map_engine_error(layout), MermaidError::Layout(_)),
                "expected Layout mapping",
            );
        }
        assert!(matches!(
            map_engine_error(E::UnsupportedDiagramType("x".into())),
            MermaidError::Unsupported(_)
        ));
    }
}
