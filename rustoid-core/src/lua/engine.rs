//! Lua/Scribunto engine.
//!
//! Wraps `mlua` to provide a sandboxed Lua runtime for Scribunto modules.
//! Phase 4 will implement the full `mw` global table.

use crate::error::Result;
use mlua::Lua;

/// Configuration for the Lua engine.
#[derive(Debug, Clone)]
pub struct LuaEngineConfig {
    /// Maximum instructions per Lua call (safety timeout).
    pub instruction_limit: u64,
    /// Maximum memory in bytes for the Lua runtime.
    pub memory_limit: usize,
}

impl Default for LuaEngineConfig {
    fn default() -> Self {
        Self {
            instruction_limit: 10_000_000,
            memory_limit: 50 * 1024 * 1024, // 50 MB
        }
    }
}

/// The Lua/Scribunto engine.
#[allow(dead_code)]
pub struct LuaEngine {
    lua: Lua,
    config: LuaEngineConfig,
}

impl LuaEngine {
    /// Create a new Lua engine with the given config.
    pub fn new(config: LuaEngineConfig) -> Result<Self> {
        let lua = Lua::new();
        // Set up sandbox: remove dangerous globals
        lua.globals()
            .set("os", mlua::Value::Nil)
            .map_err(|e| crate::error::RustoidError::Lua(e.to_string()))?;
        lua.globals()
            .set("io", mlua::Value::Nil)
            .map_err(|e| crate::error::RustoidError::Lua(e.to_string()))?;
        lua.globals()
            .set("package", mlua::Value::Nil)
            .map_err(|e| crate::error::RustoidError::Lua(e.to_string()))?;
        lua.globals()
            .set("require", mlua::Value::Nil)
            .map_err(|e| crate::error::RustoidError::Lua(e.to_string()))?;

        // Set memory limit
        lua.set_memory_limit(config.memory_limit)
            .map_err(|e| crate::error::RustoidError::Lua(e.to_string()))?;

        Ok(Self { lua, config })
    }

    /// Execute a Lua module and call a function.
    ///
    /// Placeholder — Phase 4 will implement full Scribunto support.
    pub fn execute(
        &self,
        _module_source: &str,
        _function: &str,
        _args: &[String],
    ) -> Result<String> {
        Err(crate::error::RustoidError::Unsupported(
            "Lua engine not yet implemented".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_engine() {
        let engine = LuaEngine::new(LuaEngineConfig::default());
        assert!(engine.is_ok());
    }

    #[test]
    fn test_basic_lua_execution() -> Result<()> {
        let lua = Lua::new();
        let result: i64 = lua.load("return 1 + 1").eval().unwrap();
        assert_eq!(result, 2);
        Ok(())
    }
}
