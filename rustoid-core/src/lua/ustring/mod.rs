//! Scribunto's `mw.ustring` library.
//!
//! The manual describes this as "a direct reimplementation of the standard
//! String library, except that the methods operate on characters in UTF-8
//! encoded strings rather than bytes". That distinction is the reason this is a
//! module of its own rather than an alias for `string`: the indices are
//! codepoints, and the pattern classes are Unicode properties (see [`classes`]).
//!
//! Built up from the parts that depend only on Rust's Unicode tables, so that
//! each can be checked without the live service. The matcher itself is in
//! [`pattern`], and is a port of `lstrlib.c` rather than a reading of the manual.

pub mod classes;
pub mod pattern;

pub use classes::LuaClass;
pub use pattern::{Capture, MatchError, MatchResult};
