//! The `keep_text_selection` user setting (`flash` | `hold` | `word_select`).
//!
//! One setting governs both how long an in-app selection highlight stays on
//! screen and what a double/triple-click does, so the two can never drift out
//! of sync.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TextSelection {
    /// Brief highlight on mouse-up, then clear; double-click toggles fold.
    #[default]
    Flash,
    /// Stay visible until Esc/click/scroll; double-click toggles fold.
    Hold,
    /// Stay visible until dismissed; double/triple-click selects & copies a
    /// word/line (terminal-like). Implies [`TextSelection::holds`].
    WordSelect,
}

impl TextSelection {
    /// Canonical persisted string (matches the settings-registry choices).
    pub const fn as_canonical(self) -> &'static str {
        match self {
            Self::Flash => "flash",
            Self::Hold => "hold",
            Self::WordSelect => "word_select",
        }
    }

    pub fn from_canonical(value: &str) -> Option<Self> {
        match value {
            "flash" => Some(Self::Flash),
            "hold" => Some(Self::Hold),
            "word_select" => Some(Self::WordSelect),
            _ => None,
        }
    }

    /// Never timer-dismiss the highlight (`hold` or `word_select`).
    pub const fn holds(self) -> bool {
        matches!(self, Self::Hold | Self::WordSelect)
    }

    /// Whether double-click selects & copies a word (and triple-click a line),
    /// terminal-style, instead of toggling a fold (`word_select` only).
    pub const fn selects_word(self) -> bool {
        matches!(self, Self::WordSelect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_round_trips() {
        for kind in [
            TextSelection::Flash,
            TextSelection::Hold,
            TextSelection::WordSelect,
        ] {
            assert_eq!(
                TextSelection::from_canonical(kind.as_canonical()),
                Some(kind)
            );
        }
    }

    #[test]
    fn default_is_flash() {
        assert_eq!(TextSelection::default(), TextSelection::Flash);
        assert_eq!(TextSelection::default().as_canonical(), "flash");
    }

    /// `word_select` always implies `holds()`, and is the only mode that turns
    /// on double-click word select.
    #[test]
    fn word_select_implies_hold_and_word_select() {
        assert!(TextSelection::WordSelect.holds());
        assert!(TextSelection::WordSelect.selects_word());
        assert!(TextSelection::Hold.holds());
        assert!(!TextSelection::Hold.selects_word());
        assert!(!TextSelection::Flash.holds());
        assert!(!TextSelection::Flash.selects_word());
    }

    #[test]
    fn unknown_canonical_is_none() {
        assert_eq!(TextSelection::from_canonical("yes"), None);
        assert_eq!(TextSelection::from_canonical(""), None);
        assert_eq!(TextSelection::from_canonical("Flash"), None);
        assert_eq!(TextSelection::from_canonical("true"), None);
    }
}
