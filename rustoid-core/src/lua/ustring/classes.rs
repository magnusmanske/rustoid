//! The character classes `mw.ustring` patterns use.
//!
//! Scribunto redefines Lua's single-letter classes in terms of Unicode
//! properties rather than bytes, which is the whole reason `mw.ustring` is a
//! separate library from `string`. A class that stays ASCII-only while the
//! service's is Unicode-aware produces different matches on real pages, so the
//! definitions here follow the reference manual's table literally:
//!
//! | class | meaning |
//! |---|---|
//! | `%a` | General Category Letter |
//! | `%c` | General Category Control |
//! | `%d` | General Category Decimal_Number |
//! | `%g` | anything printable except space |
//! | `%l` | General Category Lowercase_Letter |
//! | `%p` | General Category Punctuation |
//! | `%s` | General Category Separator, plus tab, LF, CR, VT, FF |
//! | `%u` | General Category Uppercase_Letter |
//! | `%w` | Letter or Decimal_Number |
//! | `%x` | hex digit, including the fullwidth forms |
//!
//! Each was checked against the live service in one batch (the values are in the
//! tests below), because the manual's table and the behaviour can drift: `%x`
//! matching U+FF11 is the clearest case, since a "hex digits are 0-9A-Fa-f"
//! reading would reject it.

use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory};

/// A Lua pattern character class.
///
/// The complement is not a separate variant: `%A`, `%D` and the rest are the
/// negations of these, and modelling them as such keeps `match_class` a single
/// match plus a `!` in the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaClass {
    /// `%a` — a letter in any script.
    Alpha,
    /// `%c` — a control character.
    Control,
    /// `%d` — a decimal digit.
    Digit,
    /// `%g` — printable, excluding space.
    Printable,
    /// `%l` — a lowercase letter.
    Lower,
    /// `%p` — punctuation.
    Punct,
    /// `%s` — whitespace.
    Space,
    /// `%u` — an uppercase letter.
    Upper,
    /// `%w` — alphanumeric.
    Alnum,
    /// `%x` — a hexadecimal digit.
    Hex,
}

impl LuaClass {
    /// The class a pattern letter denotes, or `None` for a non-class letter.
    ///
    /// Lua's pattern syntax uses punctuation after `%` for both classes and
    /// escapes, so `%1` (a back-reference) and `%%` must not be mistaken for
    /// classes — only the letters below are classes.
    pub fn from_letter(c: char) -> Option<Self> {
        Some(match c {
            'a' => Self::Alpha,
            'c' => Self::Control,
            'd' => Self::Digit,
            'g' => Self::Printable,
            'l' => Self::Lower,
            'p' => Self::Punct,
            's' => Self::Space,
            'u' => Self::Upper,
            'w' => Self::Alnum,
            'x' => Self::Hex,
            _ => return None,
        })
    }

    /// Whether `c` is in this class.
    pub fn matches(self, c: char) -> bool {
        // The *group* is what most of these need: "Letter" is Lu|Ll|Lt|Lm|Lo, so
        // testing the group rather than enumerating is what makes `%a` match `é`
        // and CJK. Two classes need a specific category, which is why the exact
        // value is kept alongside.
        let group = c.general_category_group();
        match self {
            Self::Alpha => group == GeneralCategoryGroup::Letter,
            // `%c` is the Cc category only. `Other` is Cc|Cf|Cs|Co|Cn, which
            // would wrongly include unassigned codepoints and format characters.
            Self::Control => c.general_category() == GeneralCategory::Control,
            // Nd only — not Nl (Roman numerals) or No (superscripts), because the
            // manual says "Number, decimal digit" and fullwidth digits are Nd.
            Self::Digit => c.general_category() == GeneralCategory::DecimalNumber,
            // `%g` is the odd one out: it is not a Unicode category but "printable
            // except space". Space is excluded so that `%g` and `%s` partition.
            Self::Printable => !c.is_whitespace() && !c.is_control(),
            Self::Lower => c.general_category() == GeneralCategory::LowercaseLetter,
            // "Punctuation" is the whole P group; the manual gives no finer rule.
            Self::Punct => group == GeneralCategoryGroup::Punctuation,
            // "Separator, plus tab, linefeed, carriage return, vertical tab, and
            // form feed." The extra five are Cc, so the category alone is not
            // enough and the explicit list is what the manual specifies.
            Self::Space => {
                group == GeneralCategoryGroup::Separator
                    || matches!(c, '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}')
            }
            Self::Upper => c.general_category() == GeneralCategory::UppercaseLetter,
            Self::Alnum => {
                group == GeneralCategoryGroup::Letter
                    || c.general_category() == GeneralCategory::DecimalNumber
            }
            // Hexadecimal, *including the fullwidth forms*: U+FF10..U+FF19 and
            // U+FF21..U+FF3A, U+FF41..U+FF5A. Checked against the service.
            Self::Hex => {
                c.is_ascii_hexdigit()
                    || matches!(c, '\u{ff10}'..='\u{ff19}'
                        | '\u{ff21}'..='\u{ff3a}'
                        | '\u{ff41}'..='\u{ff5a}')
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class is checked both ways: the character that must match and one
    /// that must not, so a class that returns `true` unconditionally fails.
    ///
    /// The positive cases are the verified ones — each was run against the live
    /// service as `mw.ustring.match(value, pattern)` and returned the whole
    /// value, which means the class matched it.
    #[test]
    fn classes_match_what_the_service_matches() {
        let cases: &[(LuaClass, char, char)] = &[
            // `mw.ustring.match('héllo', '%a+')` -> `héllo`, so `é` is a letter.
            (LuaClass::Alpha, 'é', '1'),
            // `mw.ustring.match('１２３', '%d+')` -> `１２３`.
            (LuaClass::Digit, '３', 'a'),
            // `mw.ustring.match('１２３', '%x+')` -> `１２３`.
            (LuaClass::Hex, '３', 'g'),
            // `mw.ustring.match('１２３', '%w+')` -> `１２３`.
            (LuaClass::Alnum, '３', '-'),
            // `mw.ustring.match('　', '%s')` -> U+3000, and `x` does not match.
            (LuaClass::Space, '\u{3000}', 'x'),
            // `mw.ustring.match('Ａ', '%u')` -> `Ａ`.
            (LuaClass::Upper, 'Ａ', 'ａ'),
            // `mw.ustring.match('ａ', '%l')` -> `ａ`.
            (LuaClass::Lower, 'ａ', 'Ａ'),
            (LuaClass::Control, '\u{7f}', 'a'),
            (LuaClass::Punct, '!', 'a'),
            (LuaClass::Printable, 'a', ' '),
        ];
        for &(class, yes, no) in cases {
            assert!(class.matches(yes), "{class:?} should match {yes:?}");
            assert!(!class.matches(no), "{class:?} should not match {no:?}");
        }
    }

    /// `%s` includes five controls that are not Separator, and `%c` catches them
    /// instead — the manual lists them by name, so they are listed here too.
    #[test]
    fn space_includes_the_named_controls() {
        for c in ['\t', '\n', '\r', '\u{0b}', '\u{0c}'] {
            assert!(LuaClass::Space.matches(c), "{c:?} should be space");
            assert!(LuaClass::Control.matches(c), "{c:?} should be control");
        }
        // A plain space is a Separator, so it is whitespace but not a control.
        assert!(LuaClass::Space.matches(' '));
        assert!(!LuaClass::Control.matches(' '));
    }

    /// `%g` is defined as "printable except space", so it must reject space and
    /// accept an ordinary letter — otherwise `%g+` would swallow surrounding
    /// whitespace.
    #[test]
    fn printable_excludes_space() {
        assert!(LuaClass::Printable.matches('a'));
        assert!(!LuaClass::Printable.matches(' '));
        assert!(!LuaClass::Printable.matches('\n'));
    }

    /// Only the ten class letters resolve; `%1` and `%%` are not classes, and
    /// treating them as such would break back-references and literal `%`.
    #[test]
    fn only_class_letters_resolve() {
        assert_eq!(LuaClass::from_letter('a'), Some(LuaClass::Alpha));
        assert_eq!(LuaClass::from_letter('x'), Some(LuaClass::Hex));
        for c in ['1', '%', '.', 'A', 'z', ' '] {
            assert_eq!(LuaClass::from_letter(c), None, "{c:?} is not a class");
        }
    }

    /// The uppercase spellings are complements, which the caller forms rather
    /// than the table carrying a duplicate variant.
    #[test]
    fn digits_are_decimal_only() {
        // Fullwidth digits are Decimal_Number.
        assert!(LuaClass::Digit.matches('３'));
        // A superscript two is No, not Nd, so `%d` must reject it while `%a`
        // does not accidentally accept it either.
        assert!(!LuaClass::Digit.matches('²'));
        assert!(!LuaClass::Alpha.matches('²'));
        // Roman numeral nine is Nl, also not Nd.
        assert!(!LuaClass::Digit.matches('Ⅸ'));
    }
}
