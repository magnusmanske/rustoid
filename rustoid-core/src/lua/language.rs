//! `mw.language` — the Language library.
//!
//! Port of Scribunto's `mw.language.lua` plus the parts of `LanguageLibrary.php`
//! the wiki modules actually reach. Scribunto builds every language object from
//! one shared method set over a `code` field, so the same shape is used here:
//! [`build_language_library`] installs the static facade and the per-code
//! constructor, and [`language_object`] builds an instance.
//!
//! Two deliberate limits, both visible in the output rather than silent:
//!
//! - Only the content language is *known* to rustoid. Any other code gets an
//!   object whose direction and casing follow a static table; a module asking
//!   about a language rustoid has no data for gets the English behaviour rather
//!   than a fabricated one.
//! - `fetchLanguageNames` returns a static ISO 639 table rather than CLDR. The
//!   corpus uses it to invert tag→name, and a smaller table only narrows which
//!   `|language=` values render as friendly names.

use mlua::{Lua, Table, Value};

use crate::error::{Result, RustoidError};

/// Language codes written right-to-left, for `isRTL`.
///
/// MediaWiki reads this from its own language data; the list below covers the
/// codes that appear on en.wiki. A code that is absent is treated as LTR, which
/// is MediaWiki's default too (only the codes in its `$rtlLanguages` list are
/// RTL).
const RTL_CODES: &[&str] = &[
    "ar", "arc", "arz", "azb", "bcc", "bgn", "bqi", "ckb", "dv", "fa", "glk", "ha", "he", "khw",
    "ks", "ku", "mzn", "nqo", "pnb", "ps", "sd", "ug", "ur", "yi",
];

/// The fallback chain MediaWiki applies for a language code.
///
/// Real MediaWiki reads this from its language data files; the common cases are
/// reproduced here, and every language falls back to English, which is what
/// `FALLBACK_MESSAGES` (the default mode) guarantees.
fn fallbacks_for(code: &str) -> Vec<String> {
    // The base language is always the first fallback: `en-gb` → `en`.
    let base = code.split('-').next().unwrap_or(code).to_string();
    let mut out: Vec<String> = Vec::new();
    if base != code {
        out.push(base.clone());
    }
    // A handful of documented chains. Anything else goes straight to English.
    let extra: &[&str] = match base.as_str() {
        "als" => &["gsw"],
        "arz" => &["ar"],
        "cbk-zam" => &["es"],
        "de-formal" => &["de"],
        "dsb" => &["de", "en"],
        "en-gb" | "en-ca" | "en-au" | "en-nz" => &[],
        "gsw" => &["als", "de"],
        "hsb" => &["dsb", "de", "en"],
        "ko-kr" => &["ko"],
        "nds-nl" => &["nds", "nl"],
        "nl-informal" => &["nl"],
        "pt-br" => &["pt"],
        "sr-ec" | "sr-el" => &["sr"],
        "zh-hans" | "zh-hant" | "zh-cn" | "zh-tw" | "zh-hk" | "zh-sg" | "zh-my" => &["zh"],
        _ => &[],
    };
    for code in extra {
        if !out.iter().any(|c| c == code) {
            out.push(code.to_string());
        }
    }
    if base != "en" && !out.iter().any(|c| c == "en") {
        out.push("en".to_string());
    }
    out
}

/// A static tag → English name table, enough to invert for `|language=`.
///
/// Scribunto's own `fetchLanguageNames` asks MediaWiki for its full CLDR-backed
/// list. rustoid has no such dataset, and inventing one silently would be worse
/// than a small honest table: `Module:Citation/CS1` uses the *inversion* of this
/// table only as a last-resort lookup behind its own remap tables, so a narrow
/// table costs a friendlier name for uncommon languages and nothing else.
const LANGUAGE_NAMES: &[(&str, &str)] = &[
    ("aa", "Afar"),
    ("ab", "Abkhazian"),
    ("af", "Afrikaans"),
    ("als", "Alemannisch"),
    ("am", "Amharic"),
    ("an", "Aragonese"),
    ("ang", "Old English"),
    ("ar", "Arabic"),
    ("arc", "Aramaic"),
    ("arz", "Egyptian Arabic"),
    ("as", "Assamese"),
    ("ast", "Asturian"),
    ("av", "Avaric"),
    ("ay", "Aymara"),
    ("az", "Azerbaijani"),
    ("ba", "Bashkir"),
    ("bar", "Bavarian"),
    ("be", "Belarusian"),
    ("be-tarask", "Belarusian (Taraškievica)"),
    ("bg", "Bulgarian"),
    ("bh", "Bihari"),
    ("bi", "Bislama"),
    ("bn", "Bengali"),
    ("bo", "Tibetan"),
    ("bpy", "Bishnupriya"),
    ("br", "Breton"),
    ("bs", "Bosnian"),
    ("ca", "Catalan"),
    ("ce", "Chechen"),
    ("ceb", "Cebuano"),
    ("ch", "Chamorro"),
    ("chr", "Cherokee"),
    ("co", "Corsican"),
    ("cr", "Cree"),
    ("cs", "Czech"),
    ("csb", "Kashubian"),
    ("cu", "Church Slavonic"),
    ("cv", "Chuvash"),
    ("cy", "Welsh"),
    ("da", "Danish"),
    ("de", "German"),
    ("de-at", "Austrian German"),
    ("de-ch", "Swiss German"),
    ("dv", "Divehi"),
    ("dz", "Dzongkha"),
    ("ee", "Ewe"),
    ("el", "Greek"),
    ("en", "English"),
    ("en-au", "Australian English"),
    ("en-ca", "Canadian English"),
    ("en-gb", "British English"),
    ("en-us", "American English"),
    ("eo", "Esperanto"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("eu", "Basque"),
    ("fa", "Persian"),
    ("ff", "Fula"),
    ("fi", "Finnish"),
    ("fj", "Fijian"),
    ("fo", "Faroese"),
    ("fr", "French"),
    ("fy", "West Frisian"),
    ("ga", "Irish"),
    ("gd", "Scottish Gaelic"),
    ("gl", "Galician"),
    ("gn", "Guarani"),
    ("gu", "Gujarati"),
    ("gv", "Manx"),
    ("ha", "Hausa"),
    ("haw", "Hawaiian"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hsb", "Upper Sorbian"),
    ("ht", "Haitian Creole"),
    ("hu", "Hungarian"),
    ("hy", "Armenian"),
    ("ia", "Interlingua"),
    ("id", "Indonesian"),
    ("ie", "Interlingue"),
    ("io", "Ido"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("iu", "Inuktitut"),
    ("ja", "Japanese"),
    ("jv", "Javanese"),
    ("ka", "Georgian"),
    ("kk", "Kazakh"),
    ("kl", "Kalaallisut"),
    ("km", "Khmer"),
    ("kn", "Kannada"),
    ("ko", "Korean"),
    ("ks", "Kashmiri"),
    ("ku", "Kurdish"),
    ("kw", "Cornish"),
    ("ky", "Kyrgyz"),
    ("la", "Latin"),
    ("lb", "Luxembourgish"),
    ("lg", "Luganda"),
    ("li", "Limburgish"),
    ("ln", "Lingala"),
    ("lo", "Lao"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("mg", "Malagasy"),
    ("mh", "Marshallese"),
    ("mi", "Māori"),
    ("mk", "Macedonian"),
    ("ml", "Malayalam"),
    ("mn", "Mongolian"),
    ("mr", "Marathi"),
    ("ms", "Malay"),
    ("mt", "Maltese"),
    ("my", "Burmese"),
    ("na", "Nauru"),
    ("nds", "Low German"),
    ("ne", "Nepali"),
    ("nl", "Dutch"),
    ("nn", "Norwegian Nynorsk"),
    ("no", "Norwegian"),
    ("nv", "Navajo"),
    ("ny", "Chichewa"),
    ("oc", "Occitan"),
    ("om", "Oromo"),
    ("or", "Odia"),
    ("os", "Ossetian"),
    ("pa", "Punjabi"),
    ("pl", "Polish"),
    ("ps", "Pashto"),
    ("pt", "Portuguese"),
    ("qu", "Quechua"),
    ("rm", "Romansh"),
    ("rn", "Kirundi"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("rw", "Kinyarwanda"),
    ("sa", "Sanskrit"),
    ("sc", "Sardinian"),
    ("scn", "Sicilian"),
    ("sco", "Scots"),
    ("sd", "Sindhi"),
    ("se", "Northern Sami"),
    ("sg", "Sango"),
    ("sh", "Serbo-Croatian"),
    ("si", "Sinhala"),
    ("simple", "Simple English"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sm", "Samoan"),
    ("sn", "Shona"),
    ("so", "Somali"),
    ("sq", "Albanian"),
    ("sr", "Serbian"),
    ("ss", "Swati"),
    ("st", "Sotho"),
    ("su", "Sundanese"),
    ("sv", "Swedish"),
    ("sw", "Swahili"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("tg", "Tajik"),
    ("th", "Thai"),
    ("ti", "Tigrinya"),
    ("tk", "Turkmen"),
    ("tl", "Tagalog"),
    ("tn", "Tswana"),
    ("to", "Tongan"),
    ("tpi", "Tok Pisin"),
    ("tr", "Turkish"),
    ("ts", "Tsonga"),
    ("tt", "Tatar"),
    ("tw", "Twi"),
    ("ty", "Tahitian"),
    ("ug", "Uyghur"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("uz", "Uzbek"),
    ("ve", "Venda"),
    ("vec", "Venetian"),
    ("vi", "Vietnamese"),
    ("vo", "Volapük"),
    ("wa", "Walloon"),
    ("war", "Waray"),
    ("wo", "Wolof"),
    ("xh", "Xhosa"),
    ("yi", "Yiddish"),
    ("yo", "Yoruba"),
    ("za", "Zhuang"),
    ("zh", "Chinese"),
    ("zh-classical", "Literary Chinese"),
    ("zh-hans", "Simplified Chinese"),
    ("zh-hant", "Traditional Chinese"),
    ("zu", "Zulu"),
];

/// Whether a code is one MediaWiki knows about.
fn is_known_code(code: &str) -> bool {
    let lower = code.to_lowercase();
    LANGUAGE_NAMES.iter().any(|(c, _)| *c == lower)
}

/// MediaWiki's `Language::isValidCode`: no unsafe characters, and title-legal.
///
/// `isValidCode` is deliberately permissive — it accepts codes for languages
/// that do not exist, because those are used for `MediaWiki:` customisation.
pub(crate) fn is_valid_code(code: &str) -> bool {
    !code.is_empty()
        && !code.contains([':', '\'', '"', '/', '\\', '<', '>', '&', '\0'])
        // A leading colon or a space would not be a page title.
        && !code.starts_with([':', ' '])
}

/// `isValidBuiltInCode`: a valid code, ASCII letters/numbers/hyphens, 2+ chars.
pub(crate) fn is_valid_builtin_code(code: &str) -> bool {
    is_valid_code(code)
        && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && code.len() >= 2
}

/// The direction of a language, as MediaWiki reports it.
pub(crate) fn is_rtl(code: &str) -> bool {
    let base = code.split('-').next().unwrap_or(code).to_lowercase();
    RTL_CODES.contains(&base.as_str())
}

/// The English name of a language code, or `None`.
fn language_name(code: &str) -> Option<&'static str> {
    let lower = code.to_lowercase();
    LANGUAGE_NAMES
        .iter()
        .find(|(c, _)| *c == lower)
        .map(|(_, n)| *n)
}

/// Build `mw.language`, plus the `mw.getLanguage`/`mw.getContentLanguage`
/// shorthands, and return the content-language object.
pub(super) fn build_language_library(lua: &Lua, language_code: &str) -> Result<(Table, Table)> {
    let err = |e: mlua::Error| RustoidError::Lua(e.to_string());
    let lang = lua.create_table().map_err(err)?;

    // Constants `mw.language.lua` installs from the PHP side. `FALLBACK_STRICT`
    // suppresses the implicit final fallback to English.
    lang.set("FALLBACK_MESSAGES", 1).map_err(err)?;
    lang.set("FALLBACK_STRICT", 2).map_err(err)?;

    // --- static queries, all over a code string ---
    lang.set(
        "isSupportedLanguage",
        lua.create_function(|_, code: Value| {
            let code = super::engine::coerce_string(&code, "isSupportedLanguage")?;
            // Supported requires a known code *and* no uppercase, matching
            // MediaWiki's "has a message file in this version" notion.
            Ok(is_known_code(&code) && code == code.to_lowercase())
        })
        .map_err(err)?,
    )
    .map_err(err)?;
    lang.set(
        "isKnownLanguageTag",
        lua.create_function(|_, code: Value| {
            let code = super::engine::coerce_string(&code, "isKnownLanguageTag")?;
            Ok(is_known_code(&code))
        })
        .map_err(err)?,
    )
    .map_err(err)?;
    lang.set(
        "isValidCode",
        lua.create_function(|_, code: Value| {
            let code = super::engine::coerce_string(&code, "isValidCode")?;
            Ok(is_valid_code(&code))
        })
        .map_err(err)?,
    )
    .map_err(err)?;
    lang.set(
        "isValidBuiltInCode",
        lua.create_function(|_, code: Value| {
            let code = super::engine::coerce_string(&code, "isValidBuiltInCode")?;
            Ok(is_valid_builtin_code(&code))
        })
        .map_err(err)?,
    )
    .map_err(err)?;
    lang.set(
        "fetchLanguageName",
        lua.create_function(|_, (code, _in_language): (Value, Option<Value>)| {
            let code = super::engine::coerce_string(&code, "fetchLanguageName")?;
            Ok(language_name(&code).unwrap_or("").to_string())
        })
        .map_err(err)?,
    )
    .map_err(err)?;
    lang.set(
        "fetchLanguageNames",
        lua.create_function(
            |lua, (_in_language, _include): (Option<Value>, Option<Value>)| {
                // A fresh table per call, as MediaWiki returns: modules mutate theirs
                // (CS1 inverts it in place), so sharing one would leak between calls.
                let names = lua.create_table()?;
                for (code, name) in LANGUAGE_NAMES {
                    names.set(*code, *name)?;
                }
                Ok(names)
            },
        )
        .map_err(err)?,
    )
    .map_err(err)?;
    lang.set(
        "getFallbacksFor",
        lua.create_function(|lua, (code, mode): (Value, Option<i64>)| {
            let code = super::engine::coerce_string(&code, "getFallbacksFor")?;
            let mut chain = fallbacks_for(&code);
            // `FALLBACK_STRICT` drops the implicit final English fallback, but
            // an explicit English fallback in the chain stays.
            if mode == Some(2) {
                let explicit = fallbacks_for(&code);
                let has_explicit_en = explicit.iter().any(|c| c == "en") && code != "en";
                if !has_explicit_en {
                    chain.retain(|c| c != "en");
                }
            }
            let out = lua.create_table()?;
            for (i, code) in chain.iter().enumerate() {
                out.set(i + 1, code.clone())?;
            }
            Ok(out)
        })
        .map_err(err)?,
    )
    .map_err(err)?;

    // `language.new(code)` — the constructor every language object comes from.
    let factory = lua
        .create_function(move |lua, opts: Value| language_object(lua, &code_of(&opts)?))
        .map_err(err)?;
    lang.set("new", factory.clone()).map_err(err)?;

    let content_language = language_object(lua, language_code)?;
    let content_for_getter = content_language.clone();
    lang.set(
        "getContentLanguage",
        lua.create_function(move |_, ()| Ok(content_for_getter.clone()))
            .map_err(err)?,
    )
    .map_err(err)?;

    Ok((lang, content_language))
}

/// The code argument of `mw.language.new`, which requires a string.
fn code_of(opts: &Value) -> mlua::Result<String> {
    if matches!(opts, Value::Nil) {
        // `mw.language.new()` with no argument is an error in Scribunto, but the
        // message matters more than the failure mode; report it as such.
        return Err(mlua::Error::runtime(
            "too few arguments to mw.language.new()",
        ));
    }
    super::engine::coerce_string(opts, "language.new")
}

/// Build one language object for `code`.
///
/// A table carrying `code` plus the shared method set, exactly as
/// `mw.language.lua` does — modules read `.code` as a *property* and call the
/// rest as methods.
fn language_object(lua: &Lua, code: &str) -> mlua::Result<Table> {
    let obj = lua.create_table()?;
    obj.set("code", code)?;
    let rtl = is_rtl(code);
    let code_owned = code.to_string();

    // `getDir` is the one that mattered most: `Module:Lang/data` calls it at
    // module scope, so its absence failed the whole module — and through it
    // `Module:Lang`, `Module:lang` and every page that transcludes them.
    obj.set(
        "getDir",
        lua.create_function(move |_, _self: Option<Value>| {
            Ok(if rtl { "rtl" } else { "ltr" }.to_string())
        })?,
    )?;
    obj.set(
        "isRTL",
        lua.create_function(move |_, _self: Option<Value>| Ok(rtl))?,
    )?;
    obj.set(
        "getDirMark",
        lua.create_function(
            move |_, (_self, opposite): (Option<Value>, Option<Value>)| {
                let opposite = matches!(opposite, Some(Value::Boolean(true)));
                let rtl = if opposite { !rtl } else { rtl };
                Ok(if rtl { "\u{200f}" } else { "\u{200e}" }.to_string())
            },
        )?,
    )?;
    obj.set(
        "getDirMarkEntity",
        lua.create_function(
            move |_, (_self, opposite): (Option<Value>, Option<Value>)| {
                let opposite = matches!(opposite, Some(Value::Boolean(true)));
                let rtl = if opposite { !rtl } else { rtl };
                Ok(if rtl { "&rlm;" } else { "&lrm;" }.to_string())
            },
        )?,
    )?;
    obj.set(
        "getArrow",
        lua.create_function(
            move |_, (_self, direction): (Option<Value>, Option<String>)| {
                let direction = direction.unwrap_or_else(|| "forwards".to_string());
                Ok(match direction.as_str() {
                    "forwards" => {
                        if rtl {
                            "\u{2190}"
                        } else {
                            "\u{2192}"
                        }
                    }
                    "backwards" => {
                        if rtl {
                            "\u{2192}"
                        } else {
                            "\u{2190}"
                        }
                    }
                    "left" => "\u{2190}",
                    "right" => "\u{2192}",
                    "up" => "\u{2191}",
                    "down" => "\u{2193}",
                    _ => "",
                }
                .to_string())
            },
        )?,
    )?;

    let code_for_getter = code_owned.clone();
    obj.set(
        "getCode",
        lua.create_function(move |_, _self: Option<Value>| Ok(code_for_getter.clone()))?,
    )?;
    let code_for_fb = code_owned.clone();
    obj.set(
        "getFallbackLanguages",
        lua.create_function(move |lua, (_self, mode): (Option<Value>, Option<i64>)| {
            let chain = fallbacks_for(&code_for_fb);
            let mut chain = chain;
            if mode == Some(2) && code_for_fb != "en" {
                chain.retain(|c| c != "en");
            }
            let out = lua.create_table()?;
            for (i, c) in chain.iter().enumerate() {
                out.set(i + 1, c.clone())?;
            }
            Ok(out)
        })?,
    )?;

    // Case conversion. `ucfirst`/`lcfirst` touch only the first character, which
    // is what distinguishes them from `uc`/`lc`.
    for (name, first_upper) in [
        ("uc", false),
        ("lc", false),
        ("ucfirst", true),
        ("lcfirst", true),
    ] {
        let first_only = first_upper;
        let upper = name.starts_with("uc");
        obj.set(
            name,
            lua.create_function(move |_, (_self, s): (Option<Value>, Value)| {
                let s = super::engine::coerce_string(&s, "language case conversion")?;
                Ok(convert_case(&s, first_only, upper))
            })?,
        )?;
    }
    obj.set(
        "caseFold",
        lua.create_function(|_, (_self, s): (Option<Value>, Value)| {
            let s = super::engine::coerce_string(&s, "caseFold")?;
            Ok(s.to_lowercase())
        })?,
    )?;

    obj.set(
        "formatNum",
        lua.create_function(
            |_, (_self, n, _opts): (Option<Value>, Value, Option<Table>)| {
                let n = match n {
                    Value::Integer(i) => i as f64,
                    Value::Number(f) => f,
                    other => {
                        let s = super::engine::coerce_string(&other, "formatNum")?;
                        s.trim().parse::<f64>().unwrap_or(f64::NAN)
                    }
                };
                Ok(super::engine::format_number(n))
            },
        )?,
    )?;
    obj.set(
        "parseFormattedNumber",
        lua.create_function(|_, (_self, n): (Option<Value>, Value)| {
            // The inverse of `formatNum`: strip grouping separators and read.
            let s = super::engine::coerce_string(&n, "parseFormattedNumber")?;
            let cleaned = s.replace(',', "");
            Ok(cleaned.trim().parse::<f64>().ok())
        })?,
    )?;
    obj.set(
        "formatDate",
        lua.create_function(
            |_, (_self, fmt, ts): (Option<Value>, Value, Option<Value>)| {
                let fmt = super::engine::coerce_string(&fmt, "formatDate")?;
                let stamp = match ts {
                    Some(v) if !v.is_nil() => super::engine::coerce_string(&v, "formatDate")?,
                    _ => return Ok(String::new()),
                };
                Ok(super::engine::format_date(&fmt, &stamp))
            },
        )?,
    )?;

    // `getDurationIntervals` — deconstruct a duration into named units. The
    // values must be *integers*: `Module:Listen` feeds them to `%02d`.
    obj.set(
        "getDurationIntervals",
        lua.create_function(
            |lua, (_self, seconds, intervals): (Option<Value>, Value, Option<Table>)| {
                let seconds = match seconds {
                    Value::Integer(i) => i as f64,
                    Value::Number(f) => f,
                    other => super::engine::coerce_string(&other, "getDurationIntervals")?
                        .trim()
                        .parse::<f64>()
                        .unwrap_or(0.0),
                };
                let wanted: Vec<String> = match intervals {
                    Some(t) => t
                        .sequence_values::<String>()
                        .filter_map(std::result::Result::ok)
                        .collect(),
                    // The documented default set.
                    None => vec![
                        "years".to_string(),
                        "weeks".to_string(),
                        "days".to_string(),
                        "hours".to_string(),
                        "minutes".to_string(),
                        "seconds".to_string(),
                    ],
                };
                let table = lua.create_table()?;
                let mut left = seconds.trunc() as i64;
                for unit in &wanted {
                    let size = match unit.as_str() {
                        "millennia" => 31_557_600_000,
                        "centuries" => 3_155_760_000,
                        "decades" => 315_576_000,
                        "years" => 31_557_600,
                        "weeks" => 604_800,
                        "days" => 86_400,
                        "hours" => 3_600,
                        "minutes" => 60,
                        "seconds" => 1,
                        _ => continue,
                    };
                    let value = left / size;
                    left -= value * size;
                    table.set(unit.clone(), value)?;
                }
                Ok(table)
            },
        )?,
    )?;

    // `convertPlural`/`plural`: English has two forms, singular for 1.
    let plural = lua.create_function(|_, (_self, n, forms): (Option<Value>, Value, Value)| {
        let count = match n {
            Value::Integer(i) => i,
            Value::Number(f) => f.trunc() as i64,
            ref other => super::engine::coerce_string(other, "plural")?
                .trim()
                .parse::<i64>()
                .unwrap_or(0),
        };
        let picked = match forms {
            Value::Table(t) => {
                let index = if count == 1 { 1 } else { 2 };
                t.get::<Value>(index)?
            }
            other => other,
        };
        Ok(super::engine::lua_value_to_string(&picked))
    })?;
    obj.set("convertPlural", plural.clone())?;
    obj.set("plural", plural)?;

    // `convertGrammar`/`grammar`: no English inflections, so the word is
    // returned unchanged — which is what MediaWiki does for English.
    let grammar =
        lua.create_function(|_, (_self, word, _case): (Option<Value>, Value, Value)| {
            super::engine::coerce_string(&word, "grammar")
        })?;
    obj.set("convertGrammar", grammar.clone())?;
    obj.set("grammar", grammar)?;

    // `gender` — English has no gendered forms; the neutral option is used.
    obj.set(
        "gender",
        lua.create_function(
            |_, (_self, _what, forms): (Option<Value>, Value, Value)| match forms {
                Value::Table(t) => Ok(super::engine::lua_value_to_string(&t.get::<Value>(3)?)),
                other => Ok(super::engine::lua_value_to_string(&other)),
            },
        )?,
    )?;

    // `formatDuration` — the human-readable counterpart of the above.
    obj.set(
        "formatDuration",
        lua.create_function(
            |_, (_self, seconds, intervals): (Option<Value>, Value, Option<Table>)| {
                let seconds = match seconds {
                    Value::Integer(i) => i as f64,
                    Value::Number(f) => f,
                    other => super::engine::coerce_string(&other, "formatDuration")?
                        .trim()
                        .parse::<f64>()
                        .unwrap_or(0.0),
                };
                let wanted: Vec<String> = match intervals {
                    Some(t) => t
                        .sequence_values::<String>()
                        .filter_map(std::result::Result::ok)
                        .collect(),
                    None => vec![
                        "years".to_string(),
                        "weeks".to_string(),
                        "days".to_string(),
                        "hours".to_string(),
                        "minutes".to_string(),
                        "seconds".to_string(),
                    ],
                };
                Ok(format_duration(seconds, &wanted))
            },
        )?,
    )?;

    obj.set(
        "toBcp47Code",
        lua.create_function(move |_, _self: Option<Value>| Ok(code_owned.clone()))?,
    )?;

    Ok(obj)
}

/// Case conversion, optionally of the first character only.
fn convert_case(s: &str, first_only: bool, upper: bool) -> String {
    if !first_only {
        return if upper {
            s.to_uppercase()
        } else {
            s.to_lowercase()
        };
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => {
            let head = if upper {
                c.to_uppercase().collect::<String>()
            } else {
                c.to_lowercase().collect::<String>()
            };
            head + chars.as_str()
        }
        None => String::new(),
    }
}

/// `formatDuration` in English: `"3 hours, 25 minutes and 45 seconds"`.
///
/// The separators come from MediaWiki's interface messages
/// (`comma-separator`, `and`, `word-separator`), whose English values are used
/// here.
fn format_duration(seconds: f64, wanted: &[String]) -> String {
    let mut left = seconds.trunc() as i64;
    let mut parts: Vec<String> = Vec::new();
    for unit in wanted {
        let (size, one, many) = match unit.as_str() {
            "millennia" => (31_557_600_000i64, "millennium", "millennia"),
            "centuries" => (3_155_760_000, "century", "centuries"),
            "decades" => (315_576_000, "decade", "decades"),
            "years" => (31_557_600, "year", "years"),
            "weeks" => (604_800, "week", "weeks"),
            "days" => (86_400, "day", "days"),
            "hours" => (3_600, "hour", "hours"),
            "minutes" => (60, "minute", "minutes"),
            "seconds" => (1, "second", "seconds"),
            _ => continue,
        };
        let value = left / size;
        left -= value * size;
        if value != 0 {
            parts.push(format!("{value} {}", if value == 1 { one } else { many }));
        }
    }
    match parts.len() {
        0 => String::new(),
        1 => parts.remove(0),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} and {last}", parts.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtl_codes_are_recognised_by_base_language() {
        assert!(is_rtl("ar"));
        assert!(is_rtl("he"));
        assert!(is_rtl("fa"));
        // A regional variant follows its base language.
        assert!(is_rtl("ar-eg"));
        assert!(!is_rtl("en"));
        assert!(!is_rtl("en-gb"));
        assert!(!is_rtl("de"));
    }

    #[test]
    fn fallbacks_reach_english() {
        assert_eq!(fallbacks_for("de"), vec!["en"]);
        // A regional variant falls back to its base, then English.
        assert_eq!(fallbacks_for("en-gb"), vec!["en"]);
        assert_eq!(fallbacks_for("zh-hant"), vec!["zh", "en"]);
        // English itself has no fallback, which is what stops `en` → `en`.
        assert!(fallbacks_for("en").is_empty());
    }

    #[test]
    fn validity_follows_mediawiki() {
        assert!(is_valid_code("en"));
        assert!(is_valid_code("zh-classical"));
        // Unsafe characters are rejected.
        assert!(!is_valid_code("en<"));
        assert!(!is_valid_code("en&x"));
        assert!(!is_valid_code(""));
        // A built-in code additionally has to be ASCII and at least 2 chars.
        assert!(is_valid_builtin_code("en"));
        assert!(!is_valid_builtin_code("e"));
        assert!(!is_valid_builtin_code("en us"));
    }

    #[test]
    fn case_conversion_distinguishes_first_only() {
        assert_eq!(convert_case("hello world", false, true), "HELLO WORLD");
        assert_eq!(convert_case("hello world", true, true), "Hello world");
        assert_eq!(convert_case("HELLO", true, false), "hELLO");
        assert_eq!(convert_case("", true, true), "");
    }

    #[test]
    fn duration_reads_like_english() {
        let units = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            format_duration(12_345.0, &units(&["hours", "minutes", "seconds"])),
            "3 hours, 25 minutes and 45 seconds"
        );
        // A single unit takes no conjunction.
        assert_eq!(format_duration(3_600.0, &units(&["hours"])), "1 hour");
        // Zero is the empty string, not "0 seconds".
        assert_eq!(format_duration(0.0, &units(&["hours"])), "");
    }

    #[test]
    fn the_name_table_is_invertible() {
        // CS1 inverts tag → name; the table has to be non-empty and consistent.
        assert_eq!(language_name("en"), Some("English"));
        assert_eq!(language_name("EN"), Some("English"));
        assert_eq!(language_name("xx"), None);
        assert!(is_known_code("de"));
        assert!(!is_known_code("zz"));
    }
}
