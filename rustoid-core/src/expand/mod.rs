//! Template expansion engine.
//!
//! Handles recursive expansion of `{{TemplateName|arg1=val1|arg2}}` constructs.
//! Also manages parser functions (`{{#if:...}}`, `{{#switch:...}}`, etc.)
//! and template argument substitution (`{{{1}}}`, `{{{name|default}}}`).

pub mod tpl_args;
pub mod transclusion;
