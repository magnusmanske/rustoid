//! Lua/Scribunto engine.
//!
//! Wraps `mlua` to provide a sandboxed Lua runtime for Scribunto modules.
//! Implements the `mw` global table with MediaWiki API stubs.

use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};

use crate::error::{Result, RustoidError};
use crate::traits::{DataSource, SiteConfig};

/// Configuration for the Lua engine.
#[derive(Debug, Clone)]
pub struct LuaEngineConfig {
    pub instruction_limit: u64,
    pub memory_limit: usize,
}

impl Default for LuaEngineConfig {
    fn default() -> Self {
        Self {
            instruction_limit: 10_000_000,
            memory_limit: 50 * 1024 * 1024,
        }
    }
}

pub struct LuaContext<S: DataSource, C: SiteConfig> {
    pub source: Arc<S>,
    pub config: Arc<C>,
    pub page_title: String,
}

pub struct LuaEngine<S: DataSource, C: SiteConfig> {
    lua: Lua,
    _context: Arc<LuaContext<S, C>>,
}

impl<S: DataSource + 'static, C: SiteConfig + 'static> LuaEngine<S, C> {
    pub fn new(engine_config: LuaEngineConfig, ctx: LuaContext<S, C>) -> Result<Self> {
        let lua = Lua::new();

        for name in &["os", "io", "package", "require", "loadfile", "dofile"] {
            lua.globals()
                .set(*name, Value::Nil)
                .map_err(|e| RustoidError::Lua(e.to_string()))?;
        }

        lua.set_memory_limit(engine_config.memory_limit)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;

        let ctx = Arc::new(ctx);
        let mw = setup_mw_table(&lua, ctx.clone())?;
        lua.globals()
            .set("mw", mw)
            .map_err(|e| RustoidError::Lua(e.to_string()))?;

        Ok(Self { lua, _context: ctx })
    }

    pub fn execute(
        &self,
        module_source: &str,
        function_name: &str,
        args: &[String],
    ) -> Result<String> {
        let frame = create_frame(&self.lua, args)?;

        self.lua
            .load(module_source)
            .set_name("module")
            .exec()
            .map_err(|e| RustoidError::Lua(format!("module load error: {e}")))?;

        let func: Function =
            self.lua.globals().get(function_name).map_err(|e| {
                RustoidError::Lua(format!("function not found: {function_name}: {e}"))
            })?;

        let result: Value = func
            .call::<Value>(frame)
            .map_err(|e| RustoidError::Lua(format!("execution error: {e}")))?;

        Ok(lua_value_to_string(&result))
    }

    pub fn eval(&self, code: &str) -> Result<String> {
        let result: Value = self
            .lua
            .load(code)
            .eval()
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
        Ok(lua_value_to_string(&result))
    }
}

// ---- mw table setup ----

fn setup_mw_table<S: DataSource + 'static, C: SiteConfig + 'static>(
    lua: &Lua,
    ctx: Arc<LuaContext<S, C>>,
) -> Result<Table> {
    let mw = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    // mw.text
    let text = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    text.set("encode", lua.create_function(luafn_text_encode)?)?;
    text.set("decode", lua.create_function(luafn_text_decode)?)?;
    text.set("trim", lua.create_function(luafn_text_trim)?)?;
    text.set("split", lua.create_function(luafn_text_split)?)?;
    text.set("tag", lua.create_function(luafn_text_tag)?)?;
    mw.set("text", text)?;

    // mw.title
    let title = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    let ctx2 = ctx.clone();
    title.set(
        "new",
        lua.create_function(move |lua, (text, ns): (String, Option<i32>)| {
            luafn_title_new(lua, &ctx2, text, ns)
        })?,
    )?;
    let ctx3 = ctx.clone();
    title.set(
        "getCurrentTitle",
        lua.create_function(move |lua, ()| luafn_title_current(lua, &ctx3))?,
    )?;
    mw.set("title", title)?;

    // mw.site
    let site = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    site.set("siteName", "Wikipedia")?;
    site.set("server", ctx.config.server_url().to_string())?;
    site.set("scriptPath", "/w")?;
    site.set("languageCode", ctx.config.language_code().to_string())?;
    mw.set("site", site)?;

    // mw.uri
    let uri = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    uri.set("encode", lua.create_function(luafn_uri_encode)?)?;
    uri.set("decode", lua.create_function(luafn_uri_decode)?)?;
    uri.set(
        "anchorEncode",
        lua.create_function(luafn_uri_anchor_encode)?,
    )?;
    mw.set("uri", uri)?;

    // mw.language
    let lang = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    lang.set("formatNum", lua.create_function(luafn_lang_format_num)?)?;
    lang.set(
        "getCode",
        lua.create_function(|_, ()| Ok("en".to_string()))?,
    )?;
    mw.set("language", lang)?;

    // mw.ustring
    let ustring = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    ustring.set("len", lua.create_function(luafn_ustring_len)?)?;
    ustring.set("sub", lua.create_function(luafn_ustring_sub)?)?;
    ustring.set("upper", lua.create_function(luafn_ustring_upper)?)?;
    ustring.set("lower", lua.create_function(luafn_ustring_lower)?)?;
    mw.set("ustring", ustring)?;

    // mw.message
    let message = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    message.set("new", lua.create_function(luafn_message_new)?)?;
    mw.set("message", message)?;

    // mw.html (simplified)
    let html = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    html.set("create", lua.create_function(luafn_html_create)?)?;
    mw.set("html", html)?;

    Ok(mw)
}

// ---- Standalone Lua functions ----

fn luafn_text_encode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(html_escape(&s))
}

fn luafn_text_decode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(html_unescape(&s))
}

fn luafn_text_trim(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.trim().to_string())
}

fn luafn_text_split(_: &Lua, (s, sep): (String, String)) -> mlua::Result<Vec<String>> {
    Ok(s.split(&sep).map(|p| p.to_string()).collect())
}

fn luafn_text_tag(
    _: &Lua,
    (name, attrs, content): (String, Option<Table>, Option<String>),
) -> mlua::Result<String> {
    let mut result = format!("<{name}");
    if let Some(attr_table) = attrs {
        for (key, val) in attr_table.pairs::<String, String>().flatten() {
            result.push_str(&format!(" {key}=\"{val}\""));
        }
    }
    if let Some(content) = content {
        result.push_str(&format!(">{content}</{name}>"));
    } else {
        result.push_str("/>");
    }
    Ok(result)
}

fn luafn_title_new<S: DataSource, C: SiteConfig>(
    lua: &Lua,
    ctx: &LuaContext<S, C>,
    text: String,
    namespace: Option<i32>,
) -> mlua::Result<Table> {
    let t = crate::title::TitleParser::parse(&text, ctx.config.as_ref());
    let ns_id = namespace.unwrap_or(t.namespace_id);
    let table = lua.create_table()?;
    table.set("text", t.text.clone())?;
    table.set("nsText", ns_name(ns_id))?;
    table.set("namespace", ns_id)?;
    let prefix = ns_name(ns_id);
    let ft = if prefix.is_empty() {
        t.text.clone()
    } else {
        format!("{prefix}:{}", t.text)
    };
    table.set("fullText", ft)?;
    table.set("exists", false)?;
    table.set("isRedirect", false)?;
    table.set("fragment", t.fragment.unwrap_or_default())?;
    table.set(
        "rootText",
        t.text.split('/').next().unwrap_or("").to_string(),
    )?;
    Ok(table)
}

fn luafn_title_current<S: DataSource, C: SiteConfig>(
    lua: &Lua,
    ctx: &LuaContext<S, C>,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("text", ctx.page_title.clone())?;
    table.set("prefixedText", ctx.page_title.clone())?;
    table.set("namespace", 0)?;
    Ok(table)
}

fn luafn_uri_encode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(url_encode(&s))
}

fn luafn_uri_decode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(url_decode(&s))
}

fn luafn_uri_anchor_encode(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.replace(' ', "_").replace('?', "%3F").replace('#', "%23"))
}

fn luafn_lang_format_num(_: &Lua, n: f64) -> mlua::Result<String> {
    Ok(format_number(n))
}

fn luafn_ustring_len(_: &Lua, s: String) -> mlua::Result<usize> {
    Ok(s.chars().count())
}

fn luafn_ustring_sub(
    _: &Lua,
    (s, start, length): (String, i64, Option<i64>),
) -> mlua::Result<String> {
    let chars: Vec<char> = s.chars().collect();
    let start_idx = if start > 0 {
        ((start - 1) as usize).min(chars.len())
    } else {
        chars.len().saturating_sub((-start) as usize)
    };
    let end_idx = if let Some(len) = length {
        if len > 0 {
            (start_idx + len as usize).min(chars.len())
        } else {
            chars.len().saturating_sub((-len) as usize)
        }
    } else {
        chars.len()
    };
    Ok(chars[start_idx..end_idx].iter().collect())
}

fn luafn_ustring_upper(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.to_uppercase())
}

fn luafn_ustring_lower(_: &Lua, s: String) -> mlua::Result<String> {
    Ok(s.to_lowercase())
}

fn luafn_message_new(lua: &Lua, (key, _args): (String, Option<Table>)) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let k = key.clone();
    table.set("key", key)?;
    table.set("plain", lua.create_function(move |_, ()| Ok(k.clone()))?)?;
    Ok(table)
}

fn luafn_html_create(lua: &Lua, (tag_name, _args): (String, Option<Table>)) -> mlua::Result<Table> {
    let builder = lua.create_table()?;
    let t1 = tag_name.clone();
    builder.set(
        "wikitext",
        lua.create_function(move |lua, text: String| {
            let b = lua.create_table()?;
            b.set("_text", text)?;
            b.set("_tag", t1.clone())?;
            Ok(b)
        })?,
    )?;
    let t2 = tag_name.clone();
    builder.set(
        "done",
        lua.create_function(move |_, ()| Ok(format!("<{t2}></{t2}>")))?,
    )?;
    let t3 = tag_name;
    builder.set(
        "allDone",
        lua.create_function(move |_, ()| Ok(format!("<{t3}></{t3}>")))?,
    )?;
    Ok(builder)
}

// ---- Frame ----

fn create_frame(lua: &Lua, args: &[String]) -> Result<Value> {
    let frame = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;

    let args_table = lua
        .create_table()
        .map_err(|e| RustoidError::Lua(e.to_string()))?;
    for (i, arg) in args.iter().enumerate() {
        args_table
            .set(i + 1, arg.clone())
            .map_err(|e| RustoidError::Lua(e.to_string()))?;
    }
    frame.set("args", args_table)?;

    frame.set("getParent", lua.create_function(|_, ()| Ok(Value::Nil))?)?;
    frame.set(
        "preprocess",
        lua.create_function(|_, text: String| Ok(text))?,
    )?;
    frame.set(
        "extensionTag",
        lua.create_function(|_, opts: Table| {
            let name: String = opts.get("name").unwrap_or_default();
            let content: String = opts.get("content").unwrap_or_default();
            Ok(format!("<{name}>{content}</{name}>"))
        })?,
    )?;

    Ok(Value::Table(frame))
}

// ---- Utilities ----

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn ns_name(ns_id: i32) -> &'static str {
    match ns_id {
        0 => "",
        1 => "Talk",
        2 => "User",
        3 => "User talk",
        4 => "Project",
        6 => "File",
        8 => "MediaWiki",
        10 => "Template",
        12 => "Help",
        14 => "Category",
        828 => "Module",
        _ => "",
    }
}

fn format_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        let s = (n as i64).to_string();
        let mut result = String::new();
        for (i, ch) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                result.push(',');
            }
            result.push(ch);
        }
        result.chars().rev().collect()
    } else {
        n.to_string()
    }
}

fn url_encode(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => result.push_str(&format!("%{byte:02X}")),
        }
    }
    result
}

fn url_decode(s: &str) -> String {
    let mut result = String::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                result.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    result.push(hex as char);
                    i += 3;
                } else {
                    result.push('%');
                    i += 1;
                }
            }
            _ => {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    result
}

fn lua_value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Table(_) => String::new(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDataSource, MockSiteConfig};

    fn make_engine() -> LuaEngine<MockDataSource, MockSiteConfig> {
        let ctx = LuaContext {
            source: Arc::new(MockDataSource::new()),
            config: Arc::new(MockSiteConfig::new()),
            page_title: "Test Page".to_string(),
        };
        LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap()
    }

    #[test]
    fn test_create_engine() {
        make_engine();
    }

    #[test]
    fn test_basic_lua_execution() {
        let engine = make_engine();
        assert_eq!(engine.eval("return 1 + 1").unwrap(), "2");
    }

    #[test]
    fn test_mw_text_encode() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.text.encode('<>&\"')").unwrap(),
            "&lt;&gt;&amp;&quot;"
        );
    }

    #[test]
    fn test_mw_text_trim() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.text.trim('  hello  ')").unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_mw_text_split() {
        let engine = make_engine();
        let result = engine
            .eval("return table.concat(mw.text.split('a,b,c', ','), '|')")
            .unwrap();
        assert_eq!(result, "a|b|c");
    }

    #[test]
    fn test_mw_title_new() {
        let engine = make_engine();
        let result = engine
            .eval("return mw.title.new('Template:Foo').fullText")
            .unwrap();
        assert_eq!(result, "Template:Foo");
    }

    #[test]
    fn test_mw_title_current() {
        let engine = make_engine();
        let result = engine
            .eval("return mw.title.getCurrentTitle().text")
            .unwrap();
        assert_eq!(result, "Test Page");
    }

    #[test]
    fn test_mw_site() {
        let engine = make_engine();
        assert_eq!(engine.eval("return mw.site.siteName").unwrap(), "Wikipedia");
    }

    #[test]
    fn test_mw_uri_encode() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.uri.encode('hello world')").unwrap(),
            "hello+world"
        );
    }

    #[test]
    fn test_mw_uri_anchor_encode() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.uri.anchorEncode('Hello World?')")
                .unwrap(),
            "Hello_World%3F"
        );
    }

    #[test]
    fn test_mw_language_format_num() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.language.formatNum(1234567)")
                .unwrap(),
            "1,234,567"
        );
    }

    #[test]
    fn test_mw_ustring_len() {
        let engine = make_engine();
        assert_eq!(engine.eval("return mw.ustring.len('hello')").unwrap(), "5");
    }

    #[test]
    fn test_mw_ustring_sub() {
        let engine = make_engine();
        assert_eq!(
            engine
                .eval("return mw.ustring.sub('hello world', 7, 5)")
                .unwrap(),
            "world"
        );
    }

    #[test]
    fn test_mw_ustring_upper() {
        let engine = make_engine();
        assert_eq!(
            engine.eval("return mw.ustring.upper('hello')").unwrap(),
            "HELLO"
        );
    }

    #[test]
    fn test_frame_args() {
        let ctx = LuaContext {
            source: Arc::new(MockDataSource::new()),
            config: Arc::new(MockSiteConfig::new()),
            page_title: "Test".to_string(),
        };
        let engine = LuaEngine::new(LuaEngineConfig::default(), ctx).unwrap();
        let result = engine
            .execute(
                "function myfn(frame) return frame.args[1] end",
                "myfn",
                &["hello".to_string()],
            )
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_module_execution() {
        let engine = make_engine();
        let source = r#"
            local p = {}
            function p.test(frame)
                return "hello from module"
            end
            return p.test(nil)
        "#;
        let result = engine.eval(source).unwrap();
        assert_eq!(result, "hello from module");
    }
}
