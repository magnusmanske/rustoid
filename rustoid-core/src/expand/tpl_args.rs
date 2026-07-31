//! Template argument substitution.
//!
//! Handles `{{{1}}}`, `{{{name}}}`, and `{{{name|default}}}` argument references.
//! Resolves positional and named arguments from a template invocation context.

/// A parsed template argument reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgReference {
    /// The argument name (e.g. `"1"`, `"title"`).
    pub name: String,
    /// Default value if the argument is not provided, or None.
    pub default: Option<String>,
}

/// A collection of template arguments (both positional and named).
#[derive(Debug, Clone, Default)]
pub struct TemplateArgs {
    /// Positional arguments indexed by 1-based position.
    positional: Vec<String>,
    /// Named arguments.
    named: std::collections::HashMap<String, String>,
}

impl TemplateArgs {
    /// Create empty template arguments.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a positional argument.
    pub fn add_positional(&mut self, value: impl Into<String>) {
        self.positional.push(value.into());
    }

    /// Add a named argument.
    pub fn add_named(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.named.insert(name.into(), value.into());
    }

    /// Get the value of an argument by name (or positional index as string).
    /// Returns `None` if the argument is not provided.
    pub fn get(&self, name: &str) -> Option<&str> {
        // Named arguments first
        if let Some(val) = self.named.get(name) {
            return Some(val.as_str());
        }
        // Try positional (if name is a number)
        if let Ok(idx) = name.parse::<usize>()
            && idx >= 1
            && idx <= self.positional.len()
        {
            return Some(&self.positional[idx - 1]);
        }
        None
    }

    /// Check if any argument with this name exists (including empty string).
    pub fn contains(&self, name: &str) -> bool {
        self.named.contains_key(name)
            || name
                .parse::<usize>()
                .map(|idx| idx >= 1 && idx <= self.positional.len())
                .unwrap_or(false)
    }
}

/// Parse an argument reference like `"1"`, `"name"`, or `"name|default value"`.
pub fn parse_arg_reference(content: &str) -> ArgReference {
    let parts: Vec<&str> = content.splitn(2, '|').collect();
    let name = parts[0].trim().to_string();
    let default = if parts.len() > 1 {
        Some(parts[1].to_string())
    } else {
        None
    };
    ArgReference { name, default }
}

/// Resolve an argument reference against the provided template arguments.
///
/// If the argument is found, returns its value. Otherwise returns the
/// default if provided, or an empty string.
pub fn resolve_arg(arg_ref: &ArgReference, args: &TemplateArgs) -> String {
    if let Some(value) = args.get(&arg_ref.name) {
        value.to_string()
    } else if let Some(ref default) = arg_ref.default {
        default.clone()
    } else {
        // Undefined arguments expand to empty string
        String::new()
    }
}

/// The `{{!}}` magic word substitutes to `|` for use in templates
/// where a pipe character would otherwise be interpreted as an argument separator.
pub const MAGIC_PIPE: &str = "{{!}}";

/// Replace `{{!}}` with `|` in wikitext.
pub fn replace_magic_pipe(text: &str) -> String {
    text.replace("{{!}}", "|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_positional() {
        let arg = parse_arg_reference("1");
        assert_eq!(arg.name, "1");
        assert_eq!(arg.default, None);
    }

    #[test]
    fn test_parse_named() {
        let arg = parse_arg_reference("title");
        assert_eq!(arg.name, "title");
    }

    #[test]
    fn test_parse_with_default() {
        let arg = parse_arg_reference("name|default value");
        assert_eq!(arg.name, "name");
        assert_eq!(arg.default, Some("default value".to_string()));
    }

    #[test]
    fn test_parse_empty_default() {
        let arg = parse_arg_reference("1|");
        assert_eq!(arg.name, "1");
        assert_eq!(arg.default, Some("".to_string()));
    }

    #[test]
    fn test_resolve_positional() {
        let mut args = TemplateArgs::new();
        args.add_positional("hello");
        args.add_positional("world");

        let arg_ref = parse_arg_reference("1");
        assert_eq!(resolve_arg(&arg_ref, &args), "hello");

        let arg_ref = parse_arg_reference("2");
        assert_eq!(resolve_arg(&arg_ref, &args), "world");

        let arg_ref = parse_arg_reference("3");
        assert_eq!(resolve_arg(&arg_ref, &args), "");
    }

    #[test]
    fn test_resolve_named() {
        let mut args = TemplateArgs::new();
        args.add_named("key", "value");

        let arg_ref = parse_arg_reference("key");
        assert_eq!(resolve_arg(&arg_ref, &args), "value");
    }

    #[test]
    fn test_resolve_with_default() {
        let args = TemplateArgs::new();

        let arg_ref = parse_arg_reference("missing|default");
        assert_eq!(resolve_arg(&arg_ref, &args), "default");
    }

    #[test]
    fn test_resolve_named_over_positional() {
        let mut args = TemplateArgs::new();
        args.add_positional("pos1");
        args.add_named("1", "named1"); // Override positional

        let arg_ref = parse_arg_reference("1");
        assert_eq!(resolve_arg(&arg_ref, &args), "named1");
    }

    #[test]
    fn test_magic_pipe() {
        assert_eq!(replace_magic_pipe("a{{!}}b"), "a|b");
        assert_eq!(replace_magic_pipe("{{!}}start"), "|start");
    }
}
