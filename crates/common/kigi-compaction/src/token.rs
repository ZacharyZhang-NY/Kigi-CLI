//! Token-count seam.
//!
//! Budgeting math in the shared engine needs a *trusted* token count, but the
//! two harnesses disagree on how to produce one: Kigi chat has a real tokenizer
//! (`TextTokenizer` / `ImageTokenizer`) and counts whole turns via
//! `KigiTurn::get_num_tokens`, while kigi estimates with `bytes / 4`. Rather
//! than bake either policy into the shared crate, callers supply an
//! [`ItemTokenCounter`].
//!
//! There is intentionally **no** blanket `Arc` forwarding here: each harness
//! implements the counter directly for the item type its algorithms run on
//! (Kigi chat: `ItemTokenCounter<Arc<KigiTurn>>`), so exactly one mechanism
//! is in play.

pub trait ItemTokenCounter<T: ?Sized>: Send + Sync {
    fn count_item_tokens(&self, item: &T) -> u32;
}
