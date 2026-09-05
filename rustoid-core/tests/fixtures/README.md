# Parsoid test fixtures

These files are vendored from the Wikimedia [`mediawiki-services-parsoid`](https://github.com/wikimedia/mediawiki-services-parsoid)
repository's `tests/parser/` directory.

## Pinned source

| File | Upstream path |
|------|---------------|
| `.txt` fixtures | `tests/parser/<name>.txt` |
| `*-knownFailures.json` | `tests/parser/<name>-knownFailures.json` |
| `*-standalone-knownFailures.json` | `tests/parser/<name>-standalone-knownFailures.json` |

The fixtures are **pinned** to Parsoid commit `d79c17f03af7423c7c2dcc73d25a6f63a4b805e2`
(`mediawiki-services-parsoid` `master`, 2026-09-04). To refresh, fetch the
corresponding files from that commit and re-place them here:

```sh
BASE=https://raw.githubusercontent.com/wikimedia/mediawiki-services-parsoid/d79c17f03af7423c7c2dcc73d25a6f63a4b805e2/tests/parser
curl -o media.txt          "$BASE/media.txt"
curl -o media-knownFailures.json               "$BASE/media-knownFailures.json"
curl -o media-standalone-knownFailures.json    "$BASE/media-standalone-knownFailures.json"
# …repeat for each fixture…
```

## Known-failures semantics

Parsoid's test runner records, per test and per mode, the output Parsoid
*actually* produces when it diverges from a fixture's canonical `!! html/parsoid`
(or legacy `!! html`) section:

- `*-knownFailures.json` — integrated/legacy-mode divergences.
- `*-standalone-knownFailures.json` — standalone-mode divergences.

The Rust test harness (`tests/harness/mod.rs`) reads both sidecar files and, when
a test's normalized output matches the recorded value for a mode, accepts it as a
faithful (expected) divergence instead of a failure. See `load_known_failures`.
