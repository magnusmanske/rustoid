//! Parsing pipeline — orchestrates all three stages.
//!
//! The pipeline coordinates tokenization, preprocessing, tree building,
//! and serialization. It is the primary entry point for library consumers.

pub mod add_link_attributes;
pub mod add_red_links;
pub mod attribute_expander;
pub mod attribute_transform_manager;
pub mod behavior_switch_handler;
pub mod cleanup;
pub mod compute_dsr;
pub mod extension_handler;
pub mod external_link_handler;
pub mod frame;
pub mod headings;
pub mod list_handler;
pub mod media_options;
pub mod p_wrap;
pub mod paragraph_wrapper_v2;
pub mod parser;
pub mod parser_functions;
pub mod pre_handler;
pub mod quote_transformer_v2;
pub mod sanitizer_handler;
pub mod section_wrapper;
pub mod template_encapsulator;
pub mod template_handler;
pub mod token_handler_pipeline;
pub mod tree_builder_html;
pub mod tree_builder_stage;
pub mod unpack_dom_fragments;
pub mod wiki_link_handler;
pub mod wiki_link_render;
