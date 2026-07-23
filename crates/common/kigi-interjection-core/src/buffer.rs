use serde::{Deserialize, Serialize};

use crate::events::EventQueue;
use crate::format::format_interjection;

/// Mid-turn interjection waiting for the next safe drain point.
/// `Attachment` is host-defined; core never inspects it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInterjection<Attachment> {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// Drained entry, framed as a synthetic user message.
#[derive(Debug, Clone, PartialEq)]
pub struct FormattedInterjection<Attachment> {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// Queue of [`PendingInterjection`] values. Drain via [`drain_formatted`].
pub type InterjectionBuffer<Attachment> = EventQueue<PendingInterjection<Attachment>>;

/// Drain `buffer` FIFO, one synthetic user message per entry (never merged).
/// `sanitize_text` runs on raw text first (e.g. strip image placeholders);
/// pass `std::convert::identity` when none is needed.
pub fn drain_formatted<Attachment>(
    buffer: &InterjectionBuffer<Attachment>,
    sanitize_text: impl Fn(String) -> String,
) -> Vec<FormattedInterjection<Attachment>> {
    buffer
        .drain_all()
        .into_iter()
        .map(|entry| FormattedInterjection {
            text: format_interjection(sanitize_text(entry.text)),
            attachments: entry.attachments,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_formatted_sanitizes_wraps_and_preserves_order() {
        let buf: InterjectionBuffer<()> = InterjectionBuffer::new();
        buf.push(PendingInterjection {
            text: "look at [SECRET] one".into(),
            attachments: vec![],
        });
        buf.push(PendingInterjection {
            text: "two".into(),
            attachments: vec![],
        });

        let out = drain_formatted(&buf, |t| t.replace("[SECRET] ", ""));
        assert!(buf.is_empty());
        assert_eq!(out.len(), 2, "one message per entry, never merged");
        assert!(
            out[0]
                .text
                .contains("<user_query>\nlook at one\n</user_query>")
        );
        assert!(out[1].text.contains("<user_query>\ntwo\n</user_query>"));
        assert!(
            out[0]
                .text
                .starts_with("The user sent a message while you were working:")
        );
    }
}
