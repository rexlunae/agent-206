//! Discarding a `Result` on purpose, visibly. Adapted from RustyClaw's
//! `rustyclaw-core::ignore` (MIT).
//!
//! Most failures must be propagated or recorded. A few carry nothing anyone
//! can act on: a reply to a receiver that already hung up, killing a child
//! that already exited during cleanup. `let _ = …` hides that decision and
//! is denied workspace-wide (`clippy::let_underscore_must_use`); calling
//! [`Ignore::ignore`] writes it down. If the answer to "what happens when this
//! fails?" is anything other than "nothing anyone can act on", use `?` or
//! record the error with `tracing` instead.

/// Discard a `Result` whose failure needs no handling.
pub trait Ignore {
    /// Drop the result, having decided its error carries nothing to act on.
    fn ignore(self);
}

impl<T, E> Ignore for Result<T, E> {
    #[inline]
    fn ignore(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignoring_drops_the_value() {
        let counted = std::sync::Arc::new(());
        let result: Result<std::sync::Arc<()>, ()> = Ok(counted.clone());
        assert_eq!(std::sync::Arc::strong_count(&counted), 2);
        result.ignore();
        assert_eq!(std::sync::Arc::strong_count(&counted), 1);
    }
}
