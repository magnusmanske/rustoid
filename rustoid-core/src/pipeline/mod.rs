//! Parsing pipeline — orchestrates all three stages.
//!
//! The pipeline coordinates tokenization, preprocessing, tree building,
//! and serialization. It is the primary entry point for library consumers.

pub mod attribute_expander;
pub mod attribute_transform_manager;
pub mod behavior_switch_handler;
pub mod external_link_handler;
pub mod frame;
pub mod list_handler;
pub mod media_options;
pub mod paragraph_wrapper;
pub mod paragraph_wrapper_v2;
pub mod parser_functions;
pub mod pre_handler;
pub mod quote_transformer;
pub mod quote_transformer_v2;
pub mod sanitizer_handler;
pub mod stage1;
pub mod stage2;
pub mod stage3;
pub mod template_encapsulator;
pub mod template_handler;
pub mod token_handler_pipeline;
pub mod wiki_link_handler;
pub mod wiki_link_render;
