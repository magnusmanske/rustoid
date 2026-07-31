//! Parsing pipeline — orchestrates all three stages.
//!
//! The pipeline coordinates tokenization, preprocessing, tree building,
//! and serialization. It is the primary entry point for library consumers.

pub mod quote_transformer;
pub mod stage1;
pub mod stage2;
pub mod stage3;
