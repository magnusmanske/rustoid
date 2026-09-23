//! Scribunto's `mw.ustring` library.
//!
//! The manual describes this as "a direct reimplementation of the standard
//! String library, except that the methods operate on characters in UTF-8
//! encoded strings rather than bytes". That distinction is the reason this is a
//! module of its own rather than an alias for `string`: the indices are
//! codepoints, and the pattern classes are Unicode properties (see [`classes`]).
//!
//! Built up from the parts that depend only on Rust's Unicode tables, so that
//! each can be checked without the live service. The pattern engine itself is
//! not here yet.

pub mod classes;

pub use classes::LuaClass;
