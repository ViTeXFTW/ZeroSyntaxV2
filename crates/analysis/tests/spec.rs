//! Spec-first behavior tests for the language server.
//!
//! Unlike a regenerated snapshot, these specs are **hand-authored** so an
//! outcome can be pinned *before* the feature that produces it exists. Each test
//! is a pair under `tests/spec/`:
//!
//! * `Foo.ini` — real INI plus cursor markers `$1`, `$2`, … placed where a
//!   completion is requested. Markers are stripped before analysis and their
//!   byte offsets recorded.
//! * `Foo.spec.toml` — the expected outcomes:
//!
//! ```toml
//! no_errors = true            # optional: assert zero error-severity diagnostics
//!
//! [[diag]]                    # one expected diagnostic
//! severity = "error"          # error | warning | hint
//! code = "bad-enum"           # stable diagnostic code
//! on = "test"                 # span must cover this *token*...
//! nth = 1                     # ...its nth occurrence (optional, default 1)
//! absent = false              # optional: assert the diagnostic is NOT emitted
//! xfail = false               # optional: currently-expected-to-fail
//!
//! [[complete]]                # one expected completion set
//! at = "$1"
//! includes = ["Armor"]        # labels that must be present
//! excludes = ["Weapon"]       # optional: labels that must be absent
//! equals = ["A", "B"]         # optional: exact label set (instead of includes)
//! xfail = false
//! ```
//!
//! `xfail` keeps `cargo test` green while specifying unbuilt behavior: an
//! `xfail` assertion is *expected to fail today*, and the suite fails if it
//! ever **passes** — a reminder to drop the flag once the feature lands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use zerosyntax_analysis::actions;
use zerosyntax_analysis::completion::complete;
use zerosyntax_analysis::diagnostics::{diagnose, Severity};
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, AssetKind, FileAsset, WorkspaceIndex,
};
use zerosyntax_analysis::{Analyzer, Span};

use serde::Deserialize;

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/spec")
}

#[derive(Deserialize, Default)]
struct Spec {
    #[serde(default)]
    no_errors: bool,
    #[serde(default)]
    diag: Vec<DiagSpec>,
    #[serde(default)]
    complete: Vec<CompleteSpec>,
    #[serde(default)]
    action: Vec<ActionSpec>,
    #[serde(default)]
    audio_assets: Vec<String>,
    #[serde(default)]
    texture_assets: Vec<String>,
    #[serde(default)]
    model_assets: Vec<ModelSpec>,
}

#[derive(Deserialize)]
struct ModelSpec {
    name: String,
    members: Vec<String>,
}

#[derive(Deserialize)]
struct ActionSpec {
    /// Token text whose span is used as the `range` for the code-action request.
    on: String,
    /// Which occurrence of `on` to use (1-based, default 1).
    #[serde(default = "one")]
    nth: usize,
    /// Fix titles that must be offered (substring match).
    #[serde(default)]
    offers: Vec<String>,
    /// Fix titles that must NOT be offered (substring match).
    #[serde(default)]
    not_offers: Vec<String>,
    #[serde(default)]
    xfail: bool,
}

#[derive(Deserialize)]
struct DiagSpec {
    severity: String,
    code: String,
    on: String,
    #[serde(default = "one")]
    nth: usize,
    /// Invert the assertion: the diagnostic must *not* cover the token
    /// (e.g. because a suppression pragma drops it).
    #[serde(default)]
    absent: bool,
    #[serde(default)]
    xfail: bool,
}

#[derive(Deserialize)]
struct CompleteSpec {
    at: String,
    #[serde(default)]
    includes: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    equals: Option<Vec<String>>,
    #[serde(default)]
    xfail: bool,
}

fn one() -> usize {
    1
}

/// Remove `$<digits>` cursor markers, returning the cleaned source and a map of
/// marker (`"$1"`) -> byte offset in the cleaned source.
fn strip_markers(src: &str) -> (String, HashMap<String, u32>) {
    let mut out = String::with_capacity(src.len());
    let mut markers = HashMap::new();
    let mut chars = src.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            let mut id = String::from("$");
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    id.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if id.len() > 1 {
                markers.insert(id, out.len() as u32);
                continue;
            }
            out.push('$');
            continue;
        }
        out.push(ch);
    }
    (out, markers)
}

/// Blank `;`-comment content to spaces (keeping byte offsets intact) so token
/// searches don't match text inside comments.
fn blank_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_comment = false;
    for ch in src.chars() {
        if ch == '\n' {
            in_comment = false;
            out.push(ch);
        } else if in_comment {
            // One space per *byte*, so offsets stay exact even when a
            // comment contains multi-byte characters.
            for _ in 0..ch.len_utf8() {
                out.push(' ');
            }
        } else if ch == ';' {
            in_comment = true;
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Byte offset of the `nth` (1-based) standalone occurrence of `needle` as a
/// whole token in `haystack`. "Standalone" means not flanked by identifier
/// characters, so `"2"` matches the percentage in `ARMOR_PIERCING 2` but not the
/// `2` inside `TestArmor2`.
fn nth_token_offset(haystack: &str, needle: &str, nth: usize) -> Option<u32> {
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    let mut count = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        let at = start + rel;
        let before_ok = at == 0 || !is_ident(haystack[..at].chars().next_back().unwrap());
        let after_idx = at + needle.len();
        let after_ok =
            after_idx >= bytes.len() || !is_ident(haystack[after_idx..].chars().next().unwrap());
        if before_ok && after_ok {
            count += 1;
            if count == nth {
                return Some(at as u32);
            }
        }
        start = at + needle.len().max(1);
    }
    None
}

fn parse_severity(s: &str) -> Severity {
    match s.to_ascii_lowercase().as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "hint" => Severity::Hint,
        other => panic!("unknown severity `{other}` in spec"),
    }
}

/// Evaluate one diagnostic assertion: `Ok(())` if satisfied, `Err(reason)` if not.
fn check_diag(
    spec: &DiagSpec,
    search: &str,
    diags: &[zerosyntax_analysis::Diagnostic],
) -> Result<(), String> {
    let Some(target) = nth_token_offset(search, &spec.on, spec.nth) else {
        return Err(format!(
            "token `{}` (#{}) not found in source",
            spec.on, spec.nth
        ));
    };
    let want = parse_severity(&spec.severity);
    let hit = diags.iter().any(|d| {
        d.code == spec.code && d.severity == want && d.span.start <= target && target < d.span.end
    });
    match (spec.absent, hit) {
        (false, true) | (true, false) => Ok(()),
        (false, false) => Err(format!(
            "expected {} `{}` covering `{}` (#{}) at byte {}; got: {}",
            spec.severity,
            spec.code,
            spec.on,
            spec.nth,
            target,
            render_diags(diags)
        )),
        (true, true) => Err(format!(
            "expected no {} `{}` covering `{}` (#{}), but one is emitted: {}",
            spec.severity,
            spec.code,
            spec.on,
            spec.nth,
            render_diags(diags)
        )),
    }
}

/// Evaluate one completion assertion.
fn check_complete(
    spec: &CompleteSpec,
    markers: &HashMap<String, u32>,
    analyzer: &Analyzer,
    parse: &zerosyntax_syntax::Parse,
    index: &WorkspaceIndex,
) -> Result<(), String> {
    let Some(&offset) = markers.get(&spec.at) else {
        return Err(format!("cursor marker `{}` not present in .ini", spec.at));
    };
    let labels: Vec<String> = complete(analyzer, parse, offset, Some(index), None)
        .into_iter()
        .map(|c| c.label)
        .collect();

    if let Some(want) = &spec.equals {
        let mut got = labels.clone();
        got.sort();
        got.dedup();
        let mut exp = want.clone();
        exp.sort();
        exp.dedup();
        if got != exp {
            return Err(format!(
                "completion at `{}` should equal {:?}; got {:?}",
                spec.at, exp, labels
            ));
        }
        return Ok(());
    }

    let missing: Vec<&String> = spec
        .includes
        .iter()
        .filter(|w| !labels.contains(w))
        .collect();
    let present_excluded: Vec<&String> = spec
        .excludes
        .iter()
        .filter(|w| labels.contains(w))
        .collect();
    if missing.is_empty() && present_excluded.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "completion at `{}` missing {:?}, unexpectedly offered {:?}; got {:?}",
            spec.at, missing, present_excluded, labels
        ))
    }
}

/// Evaluate one action assertion.
fn check_action(
    spec: &ActionSpec,
    search: &str,
    analyzer: &Analyzer,
    parse: &zerosyntax_syntax::Parse,
    src: &str,
    diags: &[zerosyntax_analysis::Diagnostic],
    index: &WorkspaceIndex,
) -> Result<(), String> {
    let Some(off) = nth_token_offset(search, &spec.on, spec.nth) else {
        return Err(format!(
            "token `{}` (#{}) not found in source",
            spec.on, spec.nth
        ));
    };
    let range = Span::new(off, off + spec.on.len() as u32);
    let fixes = actions::fixes(analyzer, parse, src, range, diags, Some(index));
    let titles: Vec<&str> = fixes.iter().map(|f| f.title.as_str()).collect();

    let missing: Vec<&String> = spec
        .offers
        .iter()
        .filter(|want| !titles.iter().any(|t| t.contains(want.as_str())))
        .collect();
    let unexpected: Vec<&String> = spec
        .not_offers
        .iter()
        .filter(|unwanted| titles.iter().any(|t| t.contains(unwanted.as_str())))
        .collect();

    if missing.is_empty() && unexpected.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "action at `{}` (#{}) — missing {:?}, unexpected {:?}; got: {:?}",
            spec.on, spec.nth, missing, unexpected, titles
        ))
    }
}

fn render_diags(diags: &[zerosyntax_analysis::Diagnostic]) -> String {
    if diags.is_empty() {
        return "(none)".into();
    }
    diags
        .iter()
        .map(|d| {
            format!(
                "{:?}/{}@{}..{}",
                d.severity, d.code, d.span.start, d.span.end
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn ini_specs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(spec_dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("ini") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn specs_hold() {
    let analyzer = Analyzer::embedded();
    // Hard failures fail the suite; pending = currently-failing xfail assertions
    // (reported but tolerated).
    let mut failures: Vec<String> = Vec::new();
    let mut pending = 0usize;

    let inis = ini_specs();
    assert!(
        !inis.is_empty(),
        "no specs found under {}",
        spec_dir().display()
    );

    for ini in inis {
        let name = ini.file_name().unwrap().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(&ini).expect("read .ini");
        let (src, markers) = strip_markers(&raw);
        let search = blank_comments(&src);

        let spec_path = ini.with_extension("spec.toml");
        let spec_text = std::fs::read_to_string(&spec_path)
            .unwrap_or_else(|_| panic!("missing spec file {}", spec_path.display()));
        let spec: Spec = toml::from_str(&spec_text)
            .unwrap_or_else(|e| panic!("parse {}: {e}", spec_path.display()));

        let parse = analyzer.parse(&src);
        // A spec is its own single-file workspace, so references resolve against
        // the definitions it declares (and only those).
        let mut index = WorkspaceIndex::new();
        index.set_file(&name, definitions_in(&analyzer, &parse, &name));
        index.set_file_tags(&name, module_tags_in(&analyzer, &parse));
        index.set_file_models(
            "spec-models",
            spec.model_assets
                .iter()
                .map(|model| zerosyntax_analysis::index::ModelAsset {
                    name: model.name.clone(),
                    members: model.members.clone(),
                })
                .collect(),
        );
        index.set_file_assets(
            "spec-assets",
            spec.audio_assets
                .iter()
                .map(|name| FileAsset {
                    kind: AssetKind::Audio,
                    name: name.clone(),
                    uri: format!("file:///{name}"),
                })
                .chain(spec.texture_assets.iter().map(|name| FileAsset {
                    kind: AssetKind::Texture,
                    name: name.clone(),
                    uri: format!("file:///{name}"),
                }))
                .collect(),
        );
        let diags = diagnose(&analyzer, &parse, Some(&index), Some(&name));

        if spec.no_errors {
            let errs: Vec<_> = diags
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if !errs.is_empty() {
                failures.push(format!(
                    "{name}: no_errors violated — {}",
                    render_diags(&diags)
                ));
            }
        }

        for d in &spec.diag {
            let result = check_diag(d, &search, &diags);
            record(d.xfail, result, &name, &mut failures, &mut pending);
        }
        for c in &spec.complete {
            let result = check_complete(c, &markers, &analyzer, &parse, &index);
            record(c.xfail, result, &name, &mut failures, &mut pending);
        }
        for a in &spec.action {
            let result = check_action(a, &search, &analyzer, &parse, &src, &diags, &index);
            record(a.xfail, result, &name, &mut failures, &mut pending);
        }
    }

    if pending > 0 {
        eprintln!("spec: {pending} pending (xfail) assertion(s) — expected to fail until built");
    }

    assert!(
        failures.is_empty(),
        "spec failures:\n  {}",
        failures.join("\n  ")
    );
}

/// Apply xfail inversion and route a single assertion result.
fn record(
    xfail: bool,
    result: Result<(), String>,
    name: &str,
    failures: &mut Vec<String>,
    pending: &mut usize,
) {
    match (xfail, result) {
        (false, Ok(())) => {}
        (false, Err(reason)) => failures.push(format!("{name}: {reason}")),
        (true, Err(_)) => *pending += 1,
        (true, Ok(())) => failures.push(format!(
            "{name}: xfail assertion unexpectedly PASSED — drop `xfail = true`"
        )),
    }
}
