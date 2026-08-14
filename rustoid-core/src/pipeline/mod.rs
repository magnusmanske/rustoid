//! Parsing pipeline — orchestrates all three stages.
//!
//! The pipeline coordinates tokenization, preprocessing, tree building,
//! and serialization. It is the primary entry point for library consumers.

pub mod behavior_switch_handler;
pub mod list_handler;
pub mod paragraph_wrapper;
pub mod paragraph_wrapper_v2;
pub mod pre_handler;
pub mod quote_transformer;
pub mod quote_transformer_v2;
pub mod sanitizer_handler;
pub mod stage1;
pub mod stage2;
pub mod stage3;
