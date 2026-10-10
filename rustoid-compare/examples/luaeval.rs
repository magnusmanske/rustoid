//! Evaluate a Lua snippet in the same environment a module runs in.
//!
//! A diagnostic for pinning down where a module's own logic diverges from the
//! service when the surrounding pipeline looks correct.
//!
//! Usage: `RUSTOID_CACHE_DIR=… cargo run --example luaeval -- '<lua code>' [title]`

use rustoid_compare::cache::WikiCache;
use rustoid_compare::siteconfig::WikiSiteConfig;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code_arg = args.get(1).expect("usage: luaeval <lua|@file> [title]");
    let code = if let Some(path) = code_arg.strip_prefix('@') {
        std::fs::read_to_string(path).expect("read lua file")
    } else {
        code_arg.to_string()
    };
    let title = args.get(2).cloned().unwrap_or_else(|| "Test".to_string());

    let root = std::env::var("RUSTOID_CACHE_DIR").expect("RUSTOID_CACHE_DIR");
    let mut cache = WikiCache::open(&root, "en.wikipedia.org").expect("open cache");
    if cache.is_empty() {
        cache.reindex().expect("reindex");
    }
    let siteinfo = cache
        .get(rustoid_compare::EntryKind::SiteInfo, "siteinfo")
        .expect("read siteinfo")
        .expect("siteinfo present")
        .body;
    let config = WikiSiteConfig::from_siteinfo_json(&siteinfo).expect("parse siteinfo");

    let site = rustoid_core::lua::engine::LuaSite::from_config(&config);
    let ctx = rustoid_core::lua::engine::LuaContext::new(site, title);
    let engine =
        rustoid_core::lua::engine::LuaEngine::new(Default::default(), ctx).expect("engine");
    match engine.eval(&code) {
        Ok(out) => println!("{out}"),
        Err(e) => eprintln!("ERROR: {e}"),
    }
}
