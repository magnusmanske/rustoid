p = 'rustoid-core/src/lua/engine.rs'
s = open(p).read()

old = '''    fn format_date_handles_the_raw_prefix() {
        assert_eq!(format_date("U", "2020-01-01"), "1577836800");
        assert_eq!(format_date("xnU", "2020-01-01"), "1577836800");
        assert_eq!(format_date("nU", "2020-01-01"), "11577836800");
        // `x` makes the next code literal, and `n` contributes nothing at all.
        assert_eq!(format_date("xU", "2020-01-01"), "U");
        assert_eq!(format_date("xx", "2020-01-01"), "x");
        assert_eq!(format_date("xn", "2020-01-01"), "");
        // A field may still follow the modifier: `H` is `00` here.
        assert_eq!(format_date("HxnU", "2020-01-01"), "001577836800");
        // A trailing `x` has nothing to make literal and adds nothing.
        assert_eq!(format_date("Ux", "2020-01-01"), "1577836800");
    }'''

new = '''    fn format_date_handles_the_raw_prefix() {
        let at = |f: &str| format_date(f, "2020-01-01").unwrap();
        assert_eq!(at("U"), "1577836800");
        assert_eq!(at("xnU"), "1577836800");
        assert_eq!(at("nU"), "11577836800");
        // `x` makes the next code literal, and `n` contributes nothing at all.
        assert_eq!(at("xU"), "U");
        assert_eq!(at("xx"), "x");
        assert_eq!(at("xn"), "");
        // A field may still follow the modifier: `H` is `00` here.
        assert_eq!(at("HxnU"), "001577836800");
        // A trailing `x` has nothing to make literal and adds nothing.
        assert_eq!(at("Ux"), "1577836800");
    }

    /// The input shapes MediaWiki accepts, each checked against the service.
    #[test]
    fn format_date_parses_the_documented_stamp_shapes() {
        let ts = |s: &str| format_date("U", s).unwrap();
        assert_eq!(ts("2020-01-01"), "1577836800");
        // Single-digit parts.
        assert_eq!(ts("2020-1-1"), "1577836800");
        // Eight digits, same instant.
        assert_eq!(ts("2020-01-01"), ts("20200101"));
        // A time of day is accepted, and the date half still matches.
        assert_eq!(format_date("H:i", "2020-01-01 07:30").unwrap(), "07:30");
        // A month name in either order; the service gives 2020-01-01 for both.
        assert_eq!(ts("January 2020"), "1577836800");
        assert_eq!(ts("2020 January"), "1577836800");
        // A bare year keeps the current month and day, so it is pinned to a real
        // instant inside that year rather than to an exact value. The service
        // returned 1600732800 for `{{#time:U|2020}}` on 2026-09-22.
        let year_only = ts("2020").parse::<i64>().unwrap();
        assert!(
            (1_577_836_800..1_609_459_200).contains(&year_only),
            "a bare year must land inside 2020, got {year_only}"
        );
    }

    /// An unparseable stamp is an *error*, as MediaWiki's is: `pcall` is what
    /// callers use to catch it. Returning a string made `Module:Time ago`'s
    /// `pcall` succeed and fed error markup into arithmetic.
    #[test]
    fn format_date_rejects_a_stamp_it_cannot_parse() {
        assert!(format_date("U", "nonsense").is_err());
        assert!(format_date("U", "not-a-date").is_err());
        // Empty is *now*, not an error, which the manual states explicitly.
        assert!(format_date("U", "").is_ok());
    }'''

assert old in s, "anchor not found"
open(p, 'w').write(s.replace(old, new))
print("tests updated")
