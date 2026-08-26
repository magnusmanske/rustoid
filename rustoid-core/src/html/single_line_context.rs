//! Single-line context stack for wikitext serialization.
//!
//! Faithful port of PHP Parsoid's `src/Html2Wt/SingleLineContext.php`: a stack
//! of booleans tracking whether the serializer is currently forced into
//! single-line output mode (newlines are collapsed to spaces while enforced).

/// A stack of booleans enforcing single-line output context.
///
/// Mirrors `SingleLineContext`: `enforce()` pushes `true`, `disable()` pushes
/// `false`, `pop()` restores the previous state, and `enforced()` reports
/// whether the top of the stack is `true`.
#[derive(Default)]
pub struct SingleLineContext {
    stack: Vec<bool>,
}

impl SingleLineContext {
    /// Push a "single-line enforced" frame.
    pub fn enforce(&mut self) {
        self.stack.push(true);
    }

    /// Is single-line output currently enforced (top of stack is `true`)?
    pub fn enforced(&self) -> bool {
        matches!(self.stack.last(), Some(true))
    }

    /// Push a "single-line disabled" frame.
    pub fn disable(&mut self) {
        self.stack.push(false);
    }

    /// Pop the most recent frame.
    pub fn pop(&mut self) {
        self.stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enforce_and_pop() {
        let mut ctx = SingleLineContext::default();
        assert!(!ctx.enforced());
        ctx.enforce();
        assert!(ctx.enforced());
        ctx.disable();
        assert!(!ctx.enforced());
        ctx.pop();
        assert!(ctx.enforced());
        ctx.pop();
        assert!(!ctx.enforced());
    }

    #[test]
    fn test_nested() {
        let mut ctx = SingleLineContext::default();
        ctx.enforce();
        ctx.enforce();
        assert!(ctx.enforced());
        ctx.pop();
        assert!(ctx.enforced());
        ctx.pop();
        assert!(!ctx.enforced());
    }

    #[test]
    fn test_pop_empty_is_noop() {
        let mut ctx = SingleLineContext::default();
        ctx.pop();
        assert!(!ctx.enforced());
    }
}
