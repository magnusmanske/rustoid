//! AST visitor trait for serialization backends.
//!
//! Implement this trait to produce output from the AST: HTML, JSON, Typst, etc.
//!
//! The visitor pattern visits nodes in document order. The `visit_*` methods
//! return `Result<()>` so backends can accumulate state (e.g., a string buffer)
//! as they traverse.

use crate::dom::node::Attribute;
use crate::error::Result;

/// Trait for visiting AST nodes to produce output.
pub trait AstVisitor {
    /// Called when a document node is entered.
    fn visit_document(&mut self) -> Result<()> {
        Ok(())
    }
    /// Called when a document node is left.
    fn leave_document(&mut self) -> Result<()> {
        Ok(())
    }

    /// Called when an element node is entered.
    fn visit_element(&mut self, attrs: &[Attribute]) -> Result<()> {
        let _ = attrs;
        Ok(())
    }
    /// Called when an element node is left.
    fn leave_element(&mut self) -> Result<()> {
        Ok(())
    }

    /// Called for text nodes.
    fn visit_text(&mut self, text: &str) -> Result<()> {
        let _ = text;
        Ok(())
    }

    /// Called for comment nodes.
    fn visit_comment(&mut self, content: &str) -> Result<()> {
        let _ = content;
        Ok(())
    }
}

/// Walk an AST in document order, calling the visitor's methods.
pub fn walk_ast(node: &crate::dom::node::Node, visitor: &mut dyn AstVisitor) -> Result<()> {
    match &node.kind {
        crate::dom::node::NodeKind::Document => {
            visitor.visit_document()?;
            for child in &node.children {
                walk_ast(child, visitor)?;
            }
            visitor.leave_document()?;
        }
        crate::dom::node::NodeKind::Element(_) => {
            visitor.visit_element(&node.attrs)?;
            for child in &node.children {
                walk_ast(child, visitor)?;
            }
            visitor.leave_element()?;
        }
        crate::dom::node::NodeKind::Text(text) => {
            visitor.visit_text(text)?;
        }
        crate::dom::node::NodeKind::Comment(content) => {
            visitor.visit_comment(content)?;
        }
    }
    Ok(())
}
