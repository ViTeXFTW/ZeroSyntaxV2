//! Diagnostics: validates a parsed document against the schema (and, when
//! available, the cross-file index).
//!
//! Two layers, per the project's "stricter / helpful" stance:
//! * engine-faithful errors — unknown block, unknown field, bad value type,
//!   bad enum/bitflag member, unterminated block;
//! * stricter warnings/hints — unknown module, unresolved cross-file reference.
//!
//! A file can opt out of specific codes with a file-scope pragma comment
//! (see [`apply_suppressions`]):
//!
//! ```ini
//! ; zerosyntax-disable: unresolved-reference, unreachable-set
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use zerosyntax_schema::{AudioExtension, Field as SchemaField, RefKind, ValueType};
use zerosyntax_syntax::ast::{Block, Field, Module};
use zerosyntax_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::index::{AssetKind, ModelMemberStrictness};
use crate::model::{
    is_model_asset_type, is_model_member_type, model_member_matches, model_member_mode,
    models_for_source, module_fits_slot, scope_schema, ScopeSchema,
};
use crate::{Analyzer, Span, WorkspaceIndex};

/// Severity of a diagnostic, mapped to LSP severities by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

/// A single diagnostic over a byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    /// Stable machine code (e.g. `unknown-field`) for client filtering.
    pub code: &'static str,
    pub message: String,
}

/// Run all diagnostics over `parse`. `index` enables cross-file reference checks
/// (pass `None` to skip them, e.g. for single-file analysis). `file` is the
/// document's name as it appears in the index (the server passes the URI);
/// with both present, redefinitions of names defined elsewhere are recognized
/// (map.ini override hints, duplicate-definition warnings, and override-aware
/// whole-object checks).
pub fn diagnose(
    analyzer: &Analyzer,
    parse: &Parse,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
) -> Vec<Diagnostic> {
    let mut out = parser_errors(parse);
    for node in parse.syntax().children() {
        out.extend(diagnose_root_child(analyzer, index, file, &node));
    }
    out.extend(map_layer_diagnostics(analyzer, parse, index, file));
    apply_suppressions(parse, &mut out);
    out
}

/// Every stable diagnostic code this module can emit. The suppression pragma
/// validates against this list so a typo gets an `unknown-suppression` hint
/// instead of silently suppressing nothing. (`push` debug-asserts membership,
/// so a new code that forgets to register here fails the test suite.)
pub const KNOWN_CODES: &[&str] = &[
    "syntax",
    "stray-field",
    "unknown-block",
    "overrides",
    "duplicate-definition",
    "map-forward-reference",
    "map-projectile-object",
    "unreachable-set",
    "unknown-field",
    "missing-module-tag",
    "unknown-module",
    "unknown-module-tag",
    "missing-condition",
    "missing-value",
    "bad-bool",
    "non-positive",
    "bad-percent",
    "bad-color",
    "bad-coord",
    "bad-number",
    "bad-enum",
    "bad-flag",
    "bad-prefixed",
    "unresolved-reference",
    "unknown-model",
    "unknown-model-member",
    "unknown-audio-file",
    "unknown-texture",
    "unknown-suppression",
    "module-wrong-slot",
    "duplicate-module-tag",
    "editor-default-module",
    "default-modules-not-removed",
];

/// The head word of the in-file suppression pragma comment.
const PRAGMAS: &[&str] = &["zerosyntax-disable", "zerosyntax-disable"];

/// If `tok` is a file-scope pragma comment (`; zerosyntax-disable[: ...]`),
/// return `(base, rest)` where `base` is the absolute byte offset of `rest`
/// inside the file and `rest` is the code-list portion (after the pragma
/// keyword and optional `:`). Returns `None` if the token is not a pragma.
pub(crate) fn pragma_rest(tok: &SyntaxToken) -> Option<(u32, &str)> {
    if tok.kind() != SyntaxKind::COMMENT {
        return None;
    }
    let text = tok.text();
    let body = text.trim_start_matches(';').trim_start();
    let rest = PRAGMAS
        .iter()
        .find_map(|pragma| body.strip_prefix(pragma))?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let base = u32::from(tok.text_range().start()) + (text.len() - rest.len()) as u32;
    Some((base, rest))
}

/// Iterate the word positions `(start, end)` within a pragma `rest` string,
/// skipping separators (space, tab, comma, carriage-return). The returned
/// offsets are byte-relative to `rest`.
pub(crate) fn pragma_words(rest: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let is_sep = |b: u8| matches!(b, b' ' | b'\t' | b',' | b'\r');
    let bytes = rest.as_bytes();
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i < bytes.len() && is_sep(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let start = i;
        while i < bytes.len() && !is_sep(bytes[i]) {
            i += 1;
        }
        Some((start, i))
    })
}

/// Honor file-scope suppression pragmas: a comment line outside any block of
/// the form `; zerosyntax-disable: code, code …` (colon optional; codes
/// separated by commas and/or whitespace; multiple pragma comments
/// accumulate) drops every diagnostic with a listed code from the file's
/// output. Unrecognized codes produce an `unknown-suppression` hint spanning
/// the offending word.
///
/// Called as the final step of both [`diagnose`] and [`diagnose_with_cache`],
/// *after* cache assembly: cached per-block entries stay unfiltered, so
/// editing the pragma takes effect file-wide even while sibling blocks reuse
/// their cached diagnostics.
fn apply_suppressions(parse: &Parse, out: &mut Vec<Diagnostic>) {
    let mut suppressed: Vec<&'static str> = Vec::new();
    let mut hints: Vec<Diagnostic> = Vec::new();
    // Comment-only lines at file scope are direct ROOT tokens; comments
    // inside blocks live in the block's subtree and are deliberately not
    // scanned (the pragma is a whole-file switch, not a local one).
    for el in parse.syntax().children_with_tokens() {
        let Some(tok) = el.as_token() else { continue };
        let Some((base, rest)) = pragma_rest(tok) else {
            continue;
        };
        for (start, end) in pragma_words(rest) {
            let word = &rest[start..end];
            if let Some(code) = KNOWN_CODES.iter().find(|c| **c == word) {
                suppressed.push(code);
            } else {
                hints.push(Diagnostic {
                    span: Span::new(base + start as u32, base + end as u32),
                    severity: Severity::Hint,
                    code: "unknown-suppression",
                    message: format!("`{word}` is not a known diagnostic code"),
                });
            }
        }
    }
    if suppressed.is_empty() && hints.is_empty() {
        return;
    }
    out.extend(hints);
    out.retain(|d| !suppressed.contains(&d.code));
}

/// Structural errors from the parser (unterminated blocks, stray `End`).
fn parser_errors(parse: &Parse) -> Vec<Diagnostic> {
    parse
        .errors
        .iter()
        .map(|err| Diagnostic {
            span: Span::new(err.start as u32, err.end as u32),
            severity: Severity::Error,
            code: "syntax",
            message: err.message.clone(),
        })
        .collect()
}

/// All schema diagnostics for one direct child of ROOT, with absolute spans.
/// Depends only on the child's own subtree, the schema, and the index — never
/// on siblings — which is what makes per-block caching sound.
fn diagnose_root_child(
    analyzer: &Analyzer,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
    node: &SyntaxNode,
) -> Vec<Diagnostic> {
    let mut ctx = Ctx {
        analyzer,
        index,
        file,
        out: Vec::new(),
        tag_seen: HashSet::new(),
    };
    match node.kind() {
        SyntaxKind::BLOCK => ctx.block(node),
        SyntaxKind::FIELD => {
            // A field at file scope is meaningless to the engine — unless
            // it is a single-line directive block (`BenchProfile`,
            // `ReallyLowMHz`), which the parser deliberately emits as a
            // field.
            if let Some(key) = Field(node.clone()).key() {
                let is_inline_block = ctx
                    .analyzer
                    .block(key.text())
                    .is_some_and(|b| !b.terminated);
                if !is_inline_block {
                    ctx.error(
                        &key,
                        "stray-field",
                        format!("`{}` is not inside a block", key.text()),
                    );
                }
            }
        }
        _ => {}
    }
    ctx.out
}

/// A per-document cache of block-level diagnostics, keyed on green-node
/// identity. After an incremental reparse, unchanged top-level blocks keep
/// pointer-identical green nodes, so their diagnostics are reused (spans are
/// stored block-relative and rebased to the block's new offset).
///
/// Reference diagnostics depend on the [`WorkspaceIndex`], so the cache is
/// cleared whenever the index [`generation`](WorkspaceIndex::generation)
/// changes.
#[derive(Default)]
pub struct DiagnosticsCache {
    index_generation: Option<u64>,
    map: HashMap<usize, CacheEntry>,
    hits: u64,
    misses: u64,
}

struct CacheEntry {
    /// Keeps the green allocation alive so the pointer key can't be reused by
    /// a new allocation while this entry exists.
    _green: rowan::GreenNode,
    /// Diagnostics with spans relative to the block's start offset.
    diags: Arc<Vec<Diagnostic>>,
}

impl DiagnosticsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cumulative (hits, misses) over the cache's lifetime.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

/// Like [`diagnose`], but reuses per-block results from `cache` for top-level
/// children whose green nodes are unchanged. Always returns exactly what
/// [`diagnose`] would (equivalence is tested); the cache only saves work.
pub fn diagnose_with_cache(
    analyzer: &Analyzer,
    parse: &Parse,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
    cache: &mut DiagnosticsCache,
) -> Vec<Diagnostic> {
    let generation = index.map(|i| i.generation());
    if cache.index_generation != generation {
        cache.map.clear();
        cache.index_generation = generation;
    }

    let mut out = parser_errors(parse);
    let mut next: HashMap<usize, CacheEntry> = HashMap::with_capacity(cache.map.len() + 8);
    for node in parse.syntax().children() {
        let offset = u32::from(node.text_range().start());
        let green = node.green().into_owned();
        let key = &*green as *const rowan::GreenNodeData as usize;

        let diags = if let Some(entry) = cache.map.get(&key) {
            cache.hits += 1;
            entry.diags.clone()
        } else {
            cache.misses += 1;
            let absolute = diagnose_root_child(analyzer, index, file, &node);
            Arc::new(
                absolute
                    .into_iter()
                    .map(|d| Diagnostic {
                        span: Span::new(d.span.start - offset, d.span.end - offset),
                        ..d
                    })
                    .collect::<Vec<_>>(),
            )
        };
        out.extend(diags.iter().map(|d| Diagnostic {
            span: Span::new(d.span.start + offset, d.span.end + offset),
            ..d.clone()
        }));
        next.insert(
            key,
            CacheEntry {
                _green: green,
                diags,
            },
        );
    }
    // Entries for children no longer in the tree are dropped here.
    cache.map = next;
    out.extend(map_layer_diagnostics(analyzer, parse, index, file));
    apply_suppressions(parse, &mut out);
    out
}

struct Ctx<'a> {
    analyzer: &'a Analyzer,
    index: Option<&'a WorkspaceIndex>,
    /// The document's name as keyed in `index` (None for single-file analysis).
    file: Option<&'a str>,
    out: Vec<Diagnostic>,
    /// Module tags seen within the current top-level block walk.
    /// Used to detect duplicates; only the tag text is tracked.
    /// (ThingTemplate.cpp: tags "must be unique across all modules".)
    tag_seen: HashSet<String>,
}

/// Files the engine loads in `INI_LOAD_CREATE_OVERRIDES` mode: map-shipped
/// `map.ini`/`solo.ini`. A block there redefining an existing name is merged
/// over a *copy* of the existing template (ThingFactory.cpp `newOverride`),
/// inheriting its modules and fields.
fn is_override_layer(file: &str) -> bool {
    file.rsplit(['/', '\\']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("map.ini") || name.eq_ignore_ascii_case("solo.ini")
    })
}

/// The trailing path segment, for readable diagnostics (`file` may be a URI).
fn short_file(file: &str) -> &str {
    file.rsplit(['/', '\\']).next().unwrap_or(file)
}

#[derive(Clone)]
struct MapDef {
    order: usize,
    header: String,
}

type MapDefs = HashMap<(RefKind, String), Vec<MapDef>>;

fn map_layer_diagnostics(
    analyzer: &Analyzer,
    parse: &Parse,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
) -> Vec<Diagnostic> {
    let Some(file) = file.filter(|file| is_override_layer(file)) else {
        return Vec::new();
    };

    let defs = map_top_level_defs(analyzer, parse);
    let mut out = Vec::new();
    for (order, node) in parse
        .syntax()
        .children()
        .filter(|n| n.kind() == SyntaxKind::BLOCK)
        .enumerate()
    {
        let consumer = block_header(&Block(node.clone()));
        collect_map_reference_diags(
            analyzer,
            &node,
            order,
            &defs,
            index,
            &consumer,
            short_file(file),
            &mut out,
        );
        collect_map_reskin_diag(
            &node,
            order,
            &defs,
            index,
            &consumer,
            short_file(file),
            &mut out,
        );
        collect_map_projectile_diags(&node, &defs, &mut out);
    }
    out
}

fn map_top_level_defs(analyzer: &Analyzer, parse: &Parse) -> MapDefs {
    let mut out: MapDefs = HashMap::new();
    for (order, node) in parse
        .syntax()
        .children()
        .filter(|n| n.kind() == SyntaxKind::BLOCK)
        .enumerate()
    {
        let block = Block(node);
        let Some(keyword) = block.keyword() else {
            continue;
        };
        let Some(kind) = analyzer.block(keyword.text()).and_then(|b| b.defines) else {
            continue;
        };
        let Some(name) = block.name() else { continue };
        out.entry((kind, name.text().to_ascii_lowercase()))
            .or_default()
            .push(MapDef {
                order,
                header: block_header(&block),
            });
    }
    out
}

fn block_header(block: &Block) -> String {
    [block.keyword(), block.name(), block.parent_name()]
        .into_iter()
        .flatten()
        .map(|token| token.text().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_base_definition(index: Option<&WorkspaceIndex>, kind: RefKind, name: &str) -> bool {
    index.is_some_and(|idx| {
        idx.locations(kind, name)
            .iter()
            .any(|loc| !is_override_layer(&loc.file))
    })
}

fn later_map_definition<'a>(
    defs: &'a MapDefs,
    kind: RefKind,
    name: &str,
    order: usize,
) -> Option<&'a MapDef> {
    defs.get(&(kind, name.to_ascii_lowercase()))?
        .iter()
        .find(|site| site.order > order)
}

#[allow(clippy::too_many_arguments)]
fn collect_map_reference_diags(
    analyzer: &Analyzer,
    node: &SyntaxNode,
    order: usize,
    defs: &MapDefs,
    index: Option<&WorkspaceIndex>,
    consumer: &str,
    file: &str,
    out: &mut Vec<Diagnostic>,
) {
    let scope = scope_schema(analyzer, node);
    for child in node.children() {
        match child.kind() {
            SyntaxKind::FIELD => {
                let field = Field(child);
                let Some(key) = field.key() else { continue };
                let Some(schema_field) = scope.field(key.text()) else {
                    continue;
                };
                for reference in reference_tokens(&field, &schema_field.value_type) {
                    if !eager_map_reference(&scope, schema_field, reference.kind) {
                        continue;
                    }
                    let name = reference.name;
                    if name.eq_ignore_ascii_case("None") {
                        continue;
                    }
                    if has_base_definition(index, reference.kind, &name) {
                        continue;
                    }
                    if let Some(site) = later_map_definition(defs, reference.kind, &name, order) {
                        let field_kind = if key.text().chars().all(|c| c.is_ascii_digit()) {
                            "slot"
                        } else {
                            "field"
                        };
                        out.push(Diagnostic {
                            span: reference.token.text_range().into(),
                            severity: Severity::Warning,
                            code: "map-forward-reference",
                            message: format!(
                                "`{}` is declared after `{consumer}`, but {field_kind} `{}` is \
                                 resolved immediately while `{file}` loads. Move `{}` above \
                                 `{consumer}`.",
                                site.header,
                                key.text(),
                                site.header,
                            ),
                        });
                    }
                }
            }
            SyntaxKind::MODULE | SyntaxKind::BLOCK => {
                collect_map_reference_diags(
                    analyzer, &child, order, defs, index, consumer, file, out,
                );
            }
            _ => {}
        }
    }
}

fn eager_map_reference(scope: &ScopeSchema<'_>, field: &SchemaField, kind: RefKind) -> bool {
    if matches!(scope, ScopeSchema::Block(block) if block.name == "Object")
        && matches!(field.name.as_str(), "SelectPortrait" | "ButtonImage")
    {
        return true;
    }

    if field.parse_fn == "parseFactionObjectCreationList" {
        return kind == RefKind::ObjectCreationList;
    }

    matches!(
        field.parse_fn.as_str(),
        "AI::parseScience"
            | "AIUpdateModuleData::parseLocomotorSet"
            | "ArmorStore::parseArmorTemplate"
            | "BoneFXUpdateModuleData::parseFXList"
            | "BoneFXUpdateModuleData::parseObjectCreationList"
            | "BoneFXUpdateModuleData::parseParticleSystem"
            | "CommandSet::parseCommandButton"
            | "DamageFX::parseMajorFXList"
            | "DamageFX::parseMinorFXList"
            | "DamageFXStore::parseDamageFX"
            | "INI::parseFXList"
            | "INI::parseMappedImage"
            | "INI::parseObjectCreationList"
            | "INI::parseParticleSystemTemplate"
            | "INI::parseScience"
            | "INI::parseScienceVector"
            | "INI::parseSpecialPowerTemplate"
            | "INI::parseThingTemplate"
            | "INI::parseUpgradeTemplate"
            | "INI::parseWeaponTemplate"
            | "ProductionPrerequisite::parsePrerequisiteScience"
            | "ProductionPrerequisite::parsePrerequisiteUnit"
            | "TransitionDamageFXModuleData::parseFXList"
            | "TransitionDamageFXModuleData::parseObjectCreationList"
            | "TransitionDamageFXModuleData::parseParticleSystem"
            | "WeaponTemplateSet::parseWeapon"
            | "parseAllVetLevelsFXList"
            | "parseAllVetLevelsPSys"
            | "parseAngleFX"
            | "parseBountyUpgradePair"
            | "parseCashHackUpgradePair"
            | "parseFX"
            | "parseOCL"
            | "parseOCLUpgradePair"
            | "parseParticleSysBone"
            | "parsePerVetLevelFXList"
            | "parsePerVetLevelPSys"
            | "parseWeapon"
    )
}

struct MapReference {
    kind: RefKind,
    token: SyntaxToken,
    name: String,
}

fn reference_tokens(field: &Field, ty: &ValueType) -> Vec<MapReference> {
    let tokens = field.value_tokens();
    match ty {
        ValueType::OneOf { .. } => ty
            .variant_for_first_token(tokens.first().map(|token| unquote(token.text())))
            .map(|variant| reference_tokens(field, variant))
            .unwrap_or_default(),
        ValueType::Reference { ref_kind } => tokens
            .first()
            .map(|tok| vec![map_reference(*ref_kind, tok, unquote(tok.text()))])
            .unwrap_or_default(),
        ValueType::ReferenceList { ref_kind } => tokens
            .iter()
            .map(|tok| map_reference(*ref_kind, tok, unquote(tok.text())))
            .collect::<Vec<_>>(),
        ValueType::Prefixed { .. } => {
            let mut out = Vec::new();
            reference_value_tokens(ty, &tokens, 0, &mut out);
            out
        }
        ValueType::TokenList { tokens: specs } => {
            let mut out = Vec::new();
            let mut raw = 0;
            for (i, spec) in specs.iter().enumerate() {
                if tokens.get(raw).is_none() {
                    break;
                }
                if i + 1 == specs.len() {
                    if let ValueType::ReferenceList { ref_kind } = spec {
                        out.extend(
                            tokens[raw..]
                                .iter()
                                .map(|tok| map_reference(*ref_kind, tok, unquote(tok.text()))),
                        );
                        break;
                    }
                }
                raw += reference_value_tokens(spec, &tokens, raw, &mut out);
            }
            out
        }
        _ => Vec::new(),
    }
}

fn reference_value_tokens(
    ty: &ValueType,
    tokens: &[SyntaxToken],
    index: usize,
    out: &mut Vec<MapReference>,
) -> usize {
    let Some(tok) = tokens.get(index) else {
        return 0;
    };
    if let Some(value_type) = ty.split_prefix_value_type(unquote(tok.text())) {
        if let Some(value) = tokens.get(index + 1) {
            reference_token(value_type, value, out);
            return 2;
        }
    }
    reference_token(ty, tok, out);
    1
}

fn map_reference(kind: RefKind, token: &SyntaxToken, name: &str) -> MapReference {
    MapReference {
        kind,
        token: token.clone(),
        name: name.to_string(),
    }
}

fn reference_token(ty: &ValueType, tok: &SyntaxToken, out: &mut Vec<MapReference>) {
    match ty {
        ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
            out.push(map_reference(*ref_kind, tok, unquote(tok.text())));
        }
        ValueType::Prefixed { prefix, value_type } => {
            let raw = unquote(tok.text());
            let Some((actual, name)) = raw.split_once(':') else {
                return;
            };
            if !actual.eq_ignore_ascii_case(prefix) {
                return;
            }
            if let ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } =
                value_type.as_ref()
            {
                out.push(map_reference(*ref_kind, tok, name));
            }
        }
        ValueType::OneOf { .. } => {
            if let Some(variant) = ty.variant_for_first_token(Some(unquote(tok.text()))) {
                reference_token(variant, tok, out);
            }
        }
        _ => {}
    }
}

fn collect_map_reskin_diag(
    node: &SyntaxNode,
    order: usize,
    defs: &MapDefs,
    index: Option<&WorkspaceIndex>,
    consumer: &str,
    file: &str,
    out: &mut Vec<Diagnostic>,
) {
    let block = Block(node.clone());
    if !block
        .keyword()
        .is_some_and(|keyword| keyword.text().eq_ignore_ascii_case("ObjectReskin"))
    {
        return;
    }
    let Some(parent) = block.parent_name() else {
        return;
    };
    let name = unquote(parent.text());
    if has_base_definition(index, RefKind::Object, name) {
        return;
    }
    let Some(site) = later_map_definition(defs, RefKind::Object, name, order) else {
        return;
    };
    out.push(Diagnostic {
        span: parent.text_range().into(),
        severity: Severity::Warning,
        code: "map-forward-reference",
        message: format!(
            "`{}` is declared after `{consumer}`; `ObjectReskin` requires its parent to exist \
             when parsed while `{file}` loads. Move `{}` above `{consumer}`.",
            site.header, site.header,
        ),
    });
}

fn collect_map_projectile_diags(node: &SyntaxNode, defs: &MapDefs, out: &mut Vec<Diagnostic>) {
    let block = Block(node.clone());
    if !block
        .keyword()
        .is_some_and(|kw| kw.text().eq_ignore_ascii_case("Weapon"))
    {
        return;
    }
    for field in block.fields() {
        if !field
            .key()
            .is_some_and(|key| key.text().eq_ignore_ascii_case("ProjectileObject"))
        {
            continue;
        }
        let Some(tok) = field.value_tokens().first().cloned() else {
            continue;
        };
        let name = unquote(tok.text());
        if name.eq_ignore_ascii_case("None")
            || !defs.contains_key(&(RefKind::Object, name.to_ascii_lowercase()))
        {
            continue;
        }
        out.push(Diagnostic {
            span: tok.text_range().into(),
            severity: Severity::Warning,
            code: "map-projectile-object",
            message: format!(
                "`ProjectileObject` uses map-defined object `{name}`; weapon projectile \
                 templates are cached before map.ini/solo.ini loads, so new map projectile \
                 objects do not resolve reliably in game"
            ),
        });
    }
}

impl<'a> Ctx<'a> {
    fn block(&mut self, node: &SyntaxNode) {
        let block = Block(node.clone());
        let schema = scope_schema(self.analyzer, node);
        if let Some(keyword) = block.keyword() {
            if self.analyzer.block(keyword.text()).is_none() {
                self.error(
                    &keyword,
                    "unknown-block",
                    format!("unknown block type `{}`", keyword.text()),
                );
            }
            let is_override_redefinition = self.check_redefinition(node);
            if keyword.text().eq_ignore_ascii_case("Object") {
                self.check_default_module_removals(node);
            }
            // Only plain `Object`s: an ObjectReskin inherits its parent's
            // modules and sets, so neither side of the pairing is visible —
            // and a map.ini override redefinition inherits the base object's
            // modules the same way.
            if keyword.text() == "Object" && !is_override_redefinition {
                self.check_set_reachability(node);
            }
        }
        self.walk(node, &schema);
    }

    fn check_default_module_removals(&mut self, node: &SyntaxNode) {
        let (Some(index), Some(file), Some(name)) =
            (self.index, self.file, Block(node.clone()).name())
        else {
            return;
        };
        if name.text().eq_ignore_ascii_case("DefaultThingTemplate")
            || !index.is_new_override_object(name.text(), file)
        {
            return;
        }
        let removed = Block(node.clone())
            .fields()
            .filter(|field| {
                field
                    .key()
                    .is_some_and(|key| key.text().eq_ignore_ascii_case("RemoveModule"))
            })
            .filter_map(|field| field.value_tokens().first().cloned())
            .map(|tag| unquote(tag.text()).to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        let remaining = index
            .module_tags_for_object("DefaultThingTemplate")
            .filter(|tag| !removed.contains(&tag.to_ascii_lowercase()))
            .filter(|tag| seen.insert(tag.to_ascii_lowercase()))
            .collect::<Vec<_>>();
        if !remaining.is_empty() {
            self.hint(
                &name,
                "default-modules-not-removed",
                format!(
                    "new map object inherits default modules {}; remove them with `RemoveModule <tag>`",
                    remaining.join(", ")
                ),
            );
        }
    }

    /// Cross-file redefinition handling for named definition blocks, driven
    /// purely by the index's name table (so it stays sound under the
    /// per-block cache: the generation bumps whenever any file's definition
    /// names change). Returns true when this block is a map.ini/solo.ini
    /// *override* of a name defined elsewhere — the engine merges it over a
    /// copy of the existing template (`INI_LOAD_CREATE_OVERRIDES`,
    /// ThingFactory.cpp `newOverride`), so whole-object checks must not
    /// assume the block is self-contained.
    fn check_redefinition(&mut self, node: &SyntaxNode) -> bool {
        let (Some(index), Some(file)) = (self.index, self.file) else {
            return false;
        };
        let block = Block(node.clone());
        let Some(kind) = block
            .keyword()
            .and_then(|k| self.analyzer.block(k.text()))
            .and_then(|b| b.defines)
        else {
            return false;
        };
        let Some(name) = block.name() else {
            return false;
        };
        let my_span: Span = name.text_range().into();
        // Definition sites other than this block (the index records the name
        // token's span, so a same-file site at another span is a duplicate).
        let others: Vec<&crate::index::Location> = index
            .locations(kind, name.text())
            .iter()
            .filter(|l| l.file != file || l.span != my_span)
            .collect();
        if others.is_empty() {
            return false;
        }
        if is_override_layer(file) {
            // Name the base-game site when one exists.
            let site = others
                .iter()
                .find(|l| !is_override_layer(&l.file))
                .unwrap_or(&others[0]);
            self.hint(
                &name,
                "overrides",
                format!(
                    "overrides `{}` defined in {} (map override: merged over the existing definition)",
                    name.text(),
                    short_file(&site.file)
                ),
            );
            return true;
        }
        // Outside override mode the engine rejects duplicate object templates
        // outright (ThingFactory.cpp "Duplicate factionunit"); other stores
        // have laxer last-wins semantics, so only objects are flagged.
        if kind == RefKind::Object {
            if let Some(site) = others.iter().find(|l| !is_override_layer(&l.file)) {
                self.warning(
                    &name,
                    "duplicate-definition",
                    format!(
                        "`{}` is already defined in {} — the engine rejects duplicate object definitions outside map overrides",
                        name.text(),
                        short_file(&site.file)
                    ),
                );
            }
        }
        false
    }

    /// Block-local dead-code check for conditional weapon, armor, and locomotor
    /// sets. Both the set and its selecting module live in the same top-level
    /// block, so the check is sound under the per-block diagnostics cache.
    /// Skipped for map.ini-style override blocks
    /// (`AddModule`/`RemoveModule`/`ReplaceModule` present): those are
    /// partial definitions.
    fn check_set_reachability(&mut self, node: &SyntaxNode) {
        /// Modules that can set WEAPONSET_PLAYER_UPGRADE on their object
        /// (WeaponSetUpgrade.cpp; TransportContain.cpp `onContaining` sets it
        /// on the transport when a rider has a viable weapon — inherited by
        /// the whole TransportContain family).
        const WEAPON_TRIGGERS: [&str; 7] = [
            "WeaponSetUpgrade",
            "TransportContain",
            "HelixContain",
            "InternetHackContain",
            "OverlordContain",
            "RailedTransportContain",
            "RiderChangeContain",
        ];
        /// ArmorUpgrade.cpp is the only setter of ARMORSET_PLAYER_UPGRADE.
        const ARMOR_TRIGGERS: [&str; 1] = ["ArmorUpgrade"];

        let mut module_names: Vec<String> = Vec::new();
        let mut weapon_set_upgrade_modules: Vec<SyntaxToken> = Vec::new();
        // (set keyword, the PLAYER_UPGRADE condition token) per set sub-block.
        let mut player_upgrade_sets: Vec<(&'static str, SyntaxToken)> = Vec::new();
        let mut is_override_patch = has_direct_field(node, "RemoveModule");

        for child in node.children().filter(|n| n.kind() == SyntaxKind::MODULE) {
            let module = Module(child.clone());
            let Some(slot) = module.slot() else { continue };
            match slot.text() {
                "AddModule" | "ReplaceModule" => is_override_patch = true,
                kw @ ("WeaponSet" | "ArmorSet") => {
                    if let Some(tok) = direct_condition_token(&child, "PLAYER_UPGRADE") {
                        let kw = if kw == "WeaponSet" {
                            "WeaponSet"
                        } else {
                            "ArmorSet"
                        };
                        player_upgrade_sets.push((kw, tok));
                    }
                }
                _ => {
                    if let Some(name) = module.module_name() {
                        if name.text() == "WeaponSetUpgrade" {
                            weapon_set_upgrade_modules.push(name.clone());
                        }
                        module_names.push(name.text().to_string());
                    }
                }
            }
        }
        if is_override_patch {
            return;
        }
        if !module_names.iter().any(|m| m == "LocomotorSetUpgrade") {
            for tok in Block(node.clone())
                .fields()
                .filter(|f| f.key().is_some_and(|k| k.text() == "Locomotor"))
                .filter_map(|f| f.value_tokens().first().cloned())
                .filter(|t| t.text().eq_ignore_ascii_case("SET_NORMAL_UPGRADED"))
            {
                self.warning(
                    &tok,
                    "unreachable-set",
                    "this locomotor set requires `LocomotorSetUpgrade` — the set can never be selected".to_string(),
                );
            }
        }
        let has_player_upgrade_weapon_set =
            player_upgrade_sets.iter().any(|(kw, _)| *kw == "WeaponSet");
        if !has_player_upgrade_weapon_set {
            for tok in weapon_set_upgrade_modules {
                self.warning(
                    &tok,
                    "unreachable-set",
                    "this `WeaponSetUpgrade` module sets `PLAYER_UPGRADE`, but this object has no `WeaponSet` with that condition".to_string(),
                );
            }
        }
        for (kw, tok) in player_upgrade_sets {
            let triggers: &[&str] = if kw == "WeaponSet" {
                &WEAPON_TRIGGERS
            } else {
                &ARMOR_TRIGGERS
            };
            if !module_names.iter().any(|m| triggers.iter().any(|t| t == m)) {
                self.warning(
                    &tok,
                    "unreachable-set",
                    format!(
                        "this `{kw}` requires the `PLAYER_UPGRADE` condition, but no module \
                         on this object can set it (e.g. `{}`) — the set can never be selected",
                        triggers[0]
                    ),
                );
            }
        }
    }

    /// Validate every field / nested scope directly inside `node`, given the
    /// resolved schema of `node` itself.
    fn walk(&mut self, node: &SyntaxNode, scope: &ScopeSchema) {
        for child in node.children() {
            match child.kind() {
                SyntaxKind::FIELD => self.field(&child, scope, node),
                SyntaxKind::MODULE => self.module(&child, scope),
                SyntaxKind::BLOCK => {
                    // Blocks nested in blocks are unusual but handled for safety.
                    let inner = scope_schema(self.analyzer, &child);
                    self.walk(&child, &inner);
                }
                _ => {}
            }
        }
    }

    fn field(&mut self, node: &SyntaxNode, scope: &ScopeSchema, scope_node: &SyntaxNode) {
        let field = Field(node.clone());
        let Some(key) = field.key() else { return };
        let name = key.text();

        if let Some(schema_field) = scope.field(name) {
            self.validate_value(&field, &schema_field.value_type);
            if name.eq_ignore_ascii_case("RemoveModule") {
                self.validate_remove_module(&field, scope_node);
            }
            self.validate_model_asset(&field, schema_field, scope_node);
            self.validate_raw_asset(&field, &schema_field.value_type);
        } else if scope.has_field_schema()
            && !scope.module_slots().iter().any(|s| s.keyword == name)
        {
            self.warning(
                &key,
                "unknown-field",
                format!("unknown field `{name}` in {}", scope.label()),
            );
        }
    }

    fn validate_remove_module(&mut self, field: &Field, scope_node: &SyntaxNode) {
        if !self.file.is_some_and(is_override_layer) {
            return;
        }
        let (Some(index), Some(tag), Some(object)) = (
            self.index,
            field.value_tokens().first().cloned(),
            Block(scope_node.clone()).name(),
        ) else {
            return;
        };
        let tag_name = unquote(tag.text());
        if !index
            .effective_module_tags_for_object(
                object.text(),
                self.file,
                Some(tag.text_range().start().into()),
            )
            .iter()
            .any(|known| known.name.eq_ignore_ascii_case(tag_name))
        {
            self.error(
                &tag,
                "unknown-module-tag",
                format!(
                    "`{tag_name}` is not a known module tag on `{}`",
                    object.text()
                ),
            );
        }
    }

    fn validate_model_asset(
        &mut self,
        field: &Field,
        schema_field: &zerosyntax_schema::Field,
        scope_node: &SyntaxNode,
    ) {
        let Some(index) = self.index else { return };
        if !index.has_model_assets() {
            return;
        }
        let tokens = field.value_tokens();
        let input = tokens.iter().map(|t| unquote(t.text())).collect::<Vec<_>>();
        let mode = model_member_mode(schema_field.model_member_mode, Some(field));
        for (i, tok) in tokens.iter().enumerate() {
            let ty = if matches!(schema_field.value_type, ValueType::W3dModelList) {
                Some(&schema_field.value_type)
            } else {
                schema_field.value_type.token_type_at_input(&input, i)
            };
            if let Some(ty) = ty {
                self.validate_model_asset_token(
                    ty,
                    tok,
                    scope_node,
                    schema_field.model_source.as_ref(),
                    mode,
                );
            }
        }
    }

    fn validate_raw_asset(&mut self, field: &Field, ty: &ValueType) {
        let Some(index) = self.index else { return };
        let tokens = field.value_tokens();
        match ty {
            ValueType::AudioFile { extension } if index.has_assets(AssetKind::Audio) => {
                if let Some(token) = tokens.first() {
                    let name = unquote(token.text());
                    let allowed = match extension {
                        AudioExtension::Any => {
                            has_extension(name, "wav") || has_extension(name, "mp3")
                        }
                        AudioExtension::Wav => has_extension(name, "wav"),
                        AudioExtension::Mp3 => has_extension(name, "mp3"),
                    };
                    if !name.eq_ignore_ascii_case("None")
                        && (!allowed || !index.is_asset(AssetKind::Audio, name))
                    {
                        self.warning(
                            token,
                            "unknown-audio-file",
                            format!("`{name}` is not a known audio file"),
                        );
                    }
                }
            }
            ValueType::AudioStemList if index.has_assets(AssetKind::Audio) => {
                for token in tokens {
                    let name = unquote(token.text());
                    if !name.eq_ignore_ascii_case("None")
                        && !index.is_asset(AssetKind::Audio, &format!("{name}.wav"))
                    {
                        self.warning(
                            &token,
                            "unknown-audio-file",
                            format!("`{name}` is not a known WAV sound stem"),
                        );
                    }
                }
            }
            ValueType::TextureFile | ValueType::TextureStem | ValueType::TextureSequenceStem
                if index.has_assets(AssetKind::Texture) =>
            {
                if let Some(token) = tokens.first() {
                    let name = unquote(token.text());
                    if !name.eq_ignore_ascii_case("None") && !texture_exists(index, ty, name) {
                        self.warning(
                            token,
                            "unknown-texture",
                            format!("`{name}` is not a known texture"),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn validate_model_asset_token(
        &mut self,
        ty: &ValueType,
        tok: &SyntaxToken,
        scope_node: &SyntaxNode,
        source: Option<&zerosyntax_schema::ModelSource>,
        mode: Option<zerosyntax_schema::ModelMemberMode>,
    ) {
        let Some(index) = self.index else { return };
        let raw = unquote(tok.text());
        let (ty, value) = match ty {
            ValueType::Prefixed { prefix, value_type } => {
                let Some((actual, value)) = raw.split_once(':') else {
                    return;
                };
                if !actual.eq_ignore_ascii_case(prefix) {
                    return;
                }
                (value_type.as_ref(), value)
            }
            _ => (ty, raw),
        };
        if value.is_empty() || value.eq_ignore_ascii_case("None") {
            return;
        }
        if is_model_asset_type(ty) {
            if !index.is_model_asset(value) {
                self.warning(
                    tok,
                    "unknown-model",
                    format!("`{value}` is not a known W3D model asset"),
                );
            }
            return;
        }
        if !is_model_member_type(ty) {
            return;
        }
        if index.model_member_strictness() == ModelMemberStrictness::Off {
            return;
        }
        let models = models_for_source(self.analyzer, scope_node, source, index);
        if models.is_empty() {
            return;
        }
        let mut checked_any_model = false;
        let mut missing = Vec::new();
        for model in models {
            if !index.is_model_asset(&model) {
                continue;
            }
            checked_any_model = true;
            if index
                .model_members(&model)
                .any(|member| model_member_matches(member, value, mode))
            {
                if index.model_member_strictness() == ModelMemberStrictness::Compatible {
                    return;
                }
            } else {
                missing.push(model);
            }
        }
        if checked_any_model && !missing.is_empty() {
            self.warning(
                tok,
                "unknown-model-member",
                format!(
                    "`{value}` is not a known W3D model bone or subobject{}",
                    if index.model_member_strictness() == ModelMemberStrictness::Strict {
                        format!(" in {}", missing.join(", "))
                    } else {
                        String::new()
                    }
                ),
            );
        }
    }

    fn module(&mut self, node: &SyntaxNode, parent: &ScopeSchema) {
        let module = Module(node.clone());
        // A MODULE node is a *real* module only when its slot keyword is one of
        // the parent block's declared module slots; otherwise it is an
        // anonymous sub-block (e.g. `ConditionState = DAMAGED`) and the token
        // after `=` is an argument, not a module name.
        let is_real_module = module
            .slot()
            .map(|s| {
                parent
                    .module_slots()
                    .iter()
                    .any(|ms| ms.keyword == s.text())
            })
            .unwrap_or(false);

        let inner = if is_real_module {
            if let Some(name) = module.module_name() {
                match self.analyzer.module(name.text()) {
                    Some(module_type) => {
                        // Check that the module implements an interface accepted
                        // by this slot. The engine crashes on a mismatch
                        // (ThingTemplate::parseModuleName, ThingTemplate.cpp).
                        if let Some(slot_token) = module.slot() {
                            if let Some(slot) = parent
                                .module_slots()
                                .iter()
                                .find(|ms| ms.keyword == slot_token.text())
                            {
                                if !module_fits_slot(module_type, slot) {
                                    self.error(
                                        &name,
                                        "module-wrong-slot",
                                        format!(
                                            "module `{}` cannot be placed in a `{}` slot; \
                                             it implements {:?} but the slot accepts {:?}",
                                            name.text(),
                                            slot.keyword,
                                            module_type.interfaces,
                                            slot.accepts,
                                        ),
                                    );
                                }
                            }
                        }
                        // ThingTemplate.cpp: "there must be a module tag
                        // present, and it must be unique across all modules".
                        if let Some(tag_tok) = module.tag() {
                            let tag_text = tag_tok.text().to_string();
                            if self.tag_seen.contains(&tag_text) {
                                self.warning(
                                    &tag_tok,
                                    "duplicate-module-tag",
                                    format!(
                                        "module tag `{tag_text}` is used more than once; \
                                         tags must be unique within an object (the engine \
                                         cannot remove a module by tag if duplicates exist)",
                                    ),
                                );
                            } else {
                                self.tag_seen.insert(tag_text.clone());
                            }
                            // World Builder inserts these three module tags into
                            // every new Object it creates. In map/solo.ini they
                            // should almost always be removed.
                            const EDITOR_DEFAULTS: [&str; 3] = [
                                "ModuleTag_DefaultDestroyDie",
                                "ModuleTag_DefaultInactiveBody",
                                "ModuleTag_DefaultW3DDefaultDraw",
                            ];
                            if self.file.is_some_and(is_override_layer)
                                && EDITOR_DEFAULTS
                                    .iter()
                                    .any(|d| d.eq_ignore_ascii_case(&tag_text))
                            {
                                self.hint(
                                    &tag_tok,
                                    "editor-default-module",
                                    format!(
                                        "World Builder adds `{tag_text}` to every new object; \
                                         consider removing it with `RemoveModule {tag_text}`"
                                    ),
                                );
                            }
                        } else {
                            self.warning(
                                &name,
                                "missing-module-tag",
                                format!(
                                    "module `{}` has no module tag (e.g. `ModuleTag_01`)",
                                    name.text()
                                ),
                            );
                        }
                        scope_schema(self.analyzer, node)
                    }
                    None => {
                        self.warning(
                            &name,
                            "unknown-module",
                            format!("unknown module `{}`", name.text()),
                        );
                        ScopeSchema::Unknown
                    }
                }
            } else {
                ScopeSchema::Unknown
            }
        } else {
            // Sub-block declared by the enclosing scope (WeaponSet, FX
            // nuggets, ...) — resolves to its field schema when modeled.
            scope_schema(self.analyzer, node)
        };

        // A WeaponSet without `Conditions` silently becomes the *default* set
        // (empty flags). Real game data spells `Conditions = None` out in all
        // but 5 of 862 cases, so the omission is almost always an accident.
        // (Not checked for ArmorSet: omitting it there is common practice.)
        if let ScopeSchema::SubBlock(sb) = &inner {
            if let Some(argument_type) = &sb.argument_type {
                self.validate_sub_block_argument(&module, argument_type);
            }
            if sb.keyword == "WeaponSet" && !has_direct_field(node, "Conditions") {
                if let Some(slot) = module.slot() {
                    self.warning(
                        &slot,
                        "missing-condition",
                        "`WeaponSet` has no `Conditions` field, making it the default set; \
                         use `Conditions = None` if that is intended"
                            .into(),
                    );
                }
            }
        }

        self.walk(node, &inner);
    }

    fn validate_sub_block_argument(&mut self, module: &Module, ty: &ValueType) {
        let tokens = module.argument_tokens();
        if tokens.is_empty() {
            return;
        }
        let ty = ty
            .variant_for_first_token(tokens.first().map(|token| unquote(token.text())))
            .unwrap_or(ty);
        match ty {
            ValueType::BitFlags { .. } | ValueType::ReferenceList { .. } => {
                for token in &tokens {
                    self.check_token(token, ty);
                }
            }
            ValueType::TokenList { tokens: specs } => {
                let mut index = 0;
                for spec in specs {
                    if tokens.get(index).is_none() {
                        break;
                    }
                    index += self.check_value_tokens(&tokens[index..], spec);
                }
            }
            single => {
                self.check_value_tokens(&tokens, single);
            }
        }
    }

    /// Validate a field's value tokens against its declared type.
    fn validate_value(&mut self, field: &Field, ty: &ValueType) {
        let tokens = field.value_tokens();
        if tokens.is_empty() {
            if let Some(key) = field.key() {
                // Most fields require at least one value; lenient types don't.
                if !matches!(ty, ValueType::Unknown { .. } | ValueType::AsciiStringList) {
                    self.warning(
                        &key,
                        "missing-value",
                        format!("`{}` expects a value", key.text()),
                    );
                }
            }
            return;
        }
        match ty {
            ValueType::OneOf { .. } => {
                if let Some(variant) =
                    ty.variant_for_first_token(tokens.first().map(|t| unquote(t.text())))
                {
                    self.validate_value(field, variant);
                }
            }
            ValueType::BitFlags { value_set } => {
                for tok in &tokens {
                    let raw = tok.text().trim_start_matches(['+', '-']);
                    if raw.eq_ignore_ascii_case("NONE") || raw.eq_ignore_ascii_case("ALL") {
                        continue;
                    }
                    self.check_bitflag_member(value_set, tok, raw);
                }
            }
            // Every token names a definition of the same kind.
            ValueType::ReferenceList { ref_kind } => {
                for tok in &tokens {
                    self.check_reference(*ref_kind, tok);
                }
            }
            ValueType::RandomVariable { value_set } => {
                for tok in tokens.iter().take(2) {
                    self.check_number(tok, NumKind::Real);
                }
                if tokens.len() < 2 {
                    if let Some(key) = field.key() {
                        self.warning(
                            &key,
                            "missing-value",
                            format!("`{}` expects at least 2 values", key.text()),
                        );
                    }
                }
                if let Some(distribution) = tokens.get(2) {
                    self.check_enum_member(value_set, distribution);
                }
            }
            ValueType::RandomKeyframe => {
                for tok in tokens.iter().take(2) {
                    self.check_number(tok, NumKind::Real);
                }
                if let Some(frame) = tokens.get(2) {
                    self.check_number(frame, NumKind::UInt);
                } else if let Some(key) = field.key() {
                    self.warning(
                        &key,
                        "missing-value",
                        format!("`{}` expects 3 values", key.text()),
                    );
                }
            }
            ValueType::ColorKeyframe => {
                if tokens.len() >= 4 {
                    let (color, frame) = tokens.split_at(tokens.len() - 1);
                    self.check_axes(field, color, &["R", "G", "B"], None, true);
                    self.check_number(&frame[0], NumKind::UInt);
                } else if let Some(key) = field.key() {
                    self.warning(
                        &key,
                        "missing-value",
                        format!("`{}` expects a color and frame", key.text()),
                    );
                }
            }
            // A fixed sequence of typed tokens; each listed token is required
            // (the engine's parse function calls getNextToken for each).
            ValueType::TokenList { tokens: specs } => {
                let mut i = 0;
                for spec in specs {
                    match tokens.get(i) {
                        Some(_) => i += self.check_value_tokens(&tokens[i..], spec),
                        None => {
                            if let Some(key) = field.key() {
                                self.warning(
                                    &key,
                                    "missing-value",
                                    format!(
                                        "`{}` expects {} values, found {}",
                                        key.text(),
                                        specs.len(),
                                        tokens.len()
                                    ),
                                );
                            }
                            break;
                        }
                    }
                }
                if let Some(spec) = specs
                    .last()
                    .filter(|spec| matches!(spec, ValueType::BitFlags { .. }))
                {
                    for tok in tokens.iter().skip(i) {
                        self.check_token(tok, spec);
                    }
                }
            }
            // `R:0 G:0 B:0 [A:255]` — components are ints in 0..=255, the `A`
            // component is optional (INI.cpp parseRGBColor / parseColorInt).
            ValueType::Color => self.check_axes(field, &tokens, &["R", "G", "B"], Some("A"), true),
            // `X:0 Y:0 [Z:0]` — reals (INI.cpp parseCoord2D / parseCoord3D).
            ValueType::Coord2D => self.check_axes(field, &tokens, &["X", "Y"], None, false),
            ValueType::Coord3D => self.check_axes(field, &tokens, &["X", "Y", "Z"], None, false),
            single => {
                self.check_value_tokens(&tokens, single);
            }
        }
    }

    /// Validate one logical value, accepting the engine's optional whitespace
    /// after a prefix colon (`Loc:X:0` and `Loc: X:0`).
    fn check_value_tokens(&mut self, tokens: &[SyntaxToken], ty: &ValueType) -> usize {
        let tok = &tokens[0];
        if let Some((value_type, value)) = split_prefixed_token(tokens, ty) {
            if ty
                .first_prefix()
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Loc"))
            {
                self.check_loc_value(value, unquote(value.text()));
            } else {
                self.check_token(value, value_type);
            }
            return 2;
        }
        self.check_token(tok, ty);
        1
    }

    /// Validate one value token against a single-token type.
    fn check_token(&mut self, tok: &SyntaxToken, ty: &ValueType) {
        match ty {
            ValueType::Bool => {
                let v = unquote(tok.text()).to_ascii_lowercase();
                if v != "yes" && v != "no" {
                    self.error(
                        tok,
                        "bad-bool",
                        format!("expected `Yes` or `No`, found `{}`", tok.text()),
                    );
                }
            }
            ValueType::Int => self.check_number(tok, NumKind::Int),
            ValueType::UInt => self.check_number(tok, NumKind::UInt),
            ValueType::Real
            | ValueType::AngleReal
            | ValueType::Velocity
            | ValueType::Acceleration
            | ValueType::Duration => self.check_number(tok, NumKind::Real),
            ValueType::PositiveReal => {
                self.check_number(tok, NumKind::Real);
                if let Ok(n) = tok.text().parse::<f64>() {
                    if n <= 0.0 {
                        self.warning(
                            tok,
                            "non-positive",
                            format!("`{}` should be greater than 0", tok.text()),
                        );
                    }
                }
            }
            ValueType::Percent => {
                let value = tok.text().strip_suffix('%');
                let ok = value
                    .or_else(|| self.analyzer.allow_bare_percentages().then_some(tok.text()))
                    .is_some_and(|n| n.parse::<f64>().is_ok());
                if !ok {
                    self.error(
                        tok,
                        "bad-percent",
                        format!("expected a percentage like `100%`, found `{}`", tok.text()),
                    );
                }
            }
            ValueType::Enum { value_set } => self.check_enum_member(value_set, tok),
            ValueType::BitFlags { value_set } => {
                let raw = tok.text().trim_start_matches(['+', '-']);
                if !raw.eq_ignore_ascii_case("NONE") && !raw.eq_ignore_ascii_case("ALL") {
                    self.check_bitflag_member(value_set, tok, raw);
                }
            }
            ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
                self.check_reference(*ref_kind, tok)
            }
            ValueType::Prefixed { prefix, value_type } => {
                self.check_prefixed_token(tok, prefix, value_type)
            }
            // No single-token validation for these.
            ValueType::AsciiString
            | ValueType::QuotedString
            | ValueType::AsciiStringList
            | ValueType::W3dModel
            | ValueType::W3dModelList
            | ValueType::W3dModelMember
            | ValueType::AudioFile { .. }
            | ValueType::AudioStemList
            | ValueType::TextureFile
            | ValueType::TextureStem
            | ValueType::TextureSequenceStem
            | ValueType::Color
            | ValueType::Coord2D
            | ValueType::Coord3D
            | ValueType::RandomVariable { .. }
            | ValueType::RandomKeyframe
            | ValueType::ColorKeyframe
            | ValueType::TokenList { .. }
            | ValueType::OneOf { .. }
            | ValueType::Unknown { .. } => {}
        }
    }

    fn check_prefixed_token(&mut self, tok: &SyntaxToken, prefix: &str, ty: &ValueType) {
        let text = unquote(tok.text());
        let Some((actual, value)) = text.split_once(':') else {
            self.error(
                tok,
                "bad-prefixed",
                format!("expected `{prefix}:...`, found `{text}`"),
            );
            return;
        };
        if !actual.eq_ignore_ascii_case(prefix) {
            self.error(
                tok,
                "bad-prefixed",
                format!("expected `{prefix}:...`, found `{text}`"),
            );
            return;
        }
        if prefix.eq_ignore_ascii_case("Loc") {
            self.check_loc_value(tok, value);
            return;
        }
        match ty {
            ValueType::Bool => {
                let v = value.to_ascii_lowercase();
                if v != "yes" && v != "no" {
                    self.error(
                        tok,
                        "bad-bool",
                        format!("expected `Yes` or `No`, found `{value}`"),
                    );
                }
            }
            ValueType::Int => self.check_prefixed_number(tok, prefix, value, NumKind::Int),
            ValueType::UInt => self.check_prefixed_number(tok, prefix, value, NumKind::UInt),
            ValueType::Real
            | ValueType::PositiveReal
            | ValueType::AngleReal
            | ValueType::Velocity
            | ValueType::Acceleration
            | ValueType::Duration => self.check_prefixed_number(tok, prefix, value, NumKind::Real),
            ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
                self.check_reference_name(*ref_kind, tok, value)
            }
            ValueType::Enum { value_set } => self.check_enum_member_name(value_set, tok, value),
            ValueType::BitFlags { value_set } => {
                let raw = value.trim_start_matches(['+', '-']);
                if !raw.eq_ignore_ascii_case("NONE") && !raw.eq_ignore_ascii_case("ALL") {
                    self.check_bitflag_member_name(value_set, tok, raw);
                }
            }
            ValueType::AsciiString | ValueType::AsciiStringList | ValueType::QuotedString => {}
            _ => {}
        }
    }

    fn check_loc_value(&mut self, tok: &SyntaxToken, value: &str) {
        let Some((axis, n)) = value.split_once(':') else {
            self.error(
                tok,
                "bad-prefixed",
                format!("expected `X:<n>` after `Loc:`, found `{value}`"),
            );
            return;
        };
        if !axis.eq_ignore_ascii_case("X") {
            self.error(
                tok,
                "bad-prefixed",
                format!("expected `X:<n>` after `Loc:`, found `{value}`"),
            );
            return;
        }
        if n.parse::<f64>().is_err() {
            self.error(
                tok,
                "bad-number",
                format!("expected a number for `Loc:X:`, found `{n}`"),
            );
        }
    }

    fn check_prefixed_number(
        &mut self,
        tok: &SyntaxToken,
        prefix: &str,
        value: &str,
        kind: NumKind,
    ) {
        let ok = match kind {
            NumKind::Int => value.parse::<i64>().is_ok(),
            NumKind::UInt => value.parse::<u64>().is_ok(),
            NumKind::Real => value.parse::<f64>().is_ok(),
        };
        if !ok {
            self.error(
                tok,
                "bad-number",
                format!("expected a number for `{prefix}:`, found `{value}`"),
            );
        }
    }

    /// Validate a tagged-axis value (`R:255 G:0 B:0`, `X:1 Y:2 Z:3`). The
    /// engine tokenizes with `:` as a separator, so `R:255`, `R: 255` and
    /// `R : 255` are all accepted; tags match case-insensitively and in order.
    /// `int_0_255` selects the color rule (ints 0..=255) over reals.
    fn check_axes(
        &mut self,
        field: &Field,
        tokens: &[SyntaxToken],
        axes: &[&str],
        optional: Option<&str>,
        int_0_255: bool,
    ) {
        let code = if int_0_255 { "bad-color" } else { "bad-coord" };
        let expected = || {
            axes.iter()
                .map(|a| format!("{a}:<n>"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        // Flatten to (subtoken, source token), splitting each WORD on `:` the
        // way the engine's separator set does.
        let mut parts: Vec<(&str, &SyntaxToken)> = Vec::new();
        for tok in tokens {
            for part in tok.text().split(':') {
                if !part.is_empty() {
                    parts.push((part, tok));
                }
            }
        }
        let mut i = 0;
        let mut take = |this: &mut Self, axis: &str, required: bool| -> bool {
            let Some(&(tag, tag_tok)) = parts.get(i) else {
                if required {
                    if let Some(key) = field.key() {
                        this.error(
                            &key,
                            code,
                            format!("`{}` expects `{}`", key.text(), expected()),
                        );
                    }
                }
                return false;
            };
            if !tag.eq_ignore_ascii_case(axis) {
                if required {
                    this.error(
                        tag_tok,
                        code,
                        format!(
                            "expected `{axis}:`, found `{tag}` (format: `{}`)",
                            expected()
                        ),
                    );
                }
                return false;
            }
            let Some(&(value, value_tok)) = parts.get(i + 1) else {
                this.error(tag_tok, code, format!("`{axis}:` is missing its value"));
                return false;
            };
            if int_0_255 {
                match value.parse::<i64>() {
                    Ok(n) if (0..=255).contains(&n) => {}
                    Ok(n) => this.error(
                        value_tok,
                        code,
                        format!("`{axis}:{n}` is out of range (0-255)"),
                    ),
                    Err(_) => this.error(
                        value_tok,
                        code,
                        format!("expected an integer for `{axis}:`, found `{value}`"),
                    ),
                }
            } else if value.parse::<f64>().is_err() {
                this.error(
                    value_tok,
                    code,
                    format!("expected a number for `{axis}:`, found `{value}`"),
                );
            }
            i += 2;
            true
        };
        for axis in axes {
            if !take(self, axis, true) {
                return;
            }
        }
        if let Some(axis) = optional {
            take(self, axis, false);
        }
    }

    fn check_number(&mut self, tok: &SyntaxToken, kind: NumKind) {
        let text = tok.text();
        let ok = match kind {
            NumKind::Int => text.parse::<i64>().is_ok(),
            NumKind::UInt => text.parse::<u64>().is_ok(),
            NumKind::Real => text.parse::<f64>().is_ok(),
        };
        if !ok {
            let what = match kind {
                NumKind::Int => "an integer",
                NumKind::UInt => "a non-negative integer",
                NumKind::Real => "a number",
            };
            self.error(
                tok,
                "bad-number",
                format!("expected {what}, found `{text}`"),
            );
        }
    }

    fn check_enum_member(&mut self, value_set: &str, tok: &SyntaxToken) {
        self.check_enum_member_name(value_set, tok, tok.text());
    }

    fn check_enum_member_name(&mut self, value_set: &str, tok: &SyntaxToken, v: &str) {
        let Some(set) = self.analyzer.value_set(value_set) else {
            return;
        };
        if set.members.is_empty() {
            return; // value set we couldn't populate; don't flag
        }
        if !set.members.iter().any(|m| m.name.eq_ignore_ascii_case(v)) {
            self.error(
                tok,
                "bad-enum",
                format!("`{v}` is not a valid value (expected one of {value_set})"),
            );
        }
    }

    fn check_bitflag_member(&mut self, value_set: &str, tok: &SyntaxToken, raw: &str) {
        self.check_bitflag_member_name(value_set, tok, raw);
    }

    fn check_bitflag_member_name(&mut self, value_set: &str, tok: &SyntaxToken, raw: &str) {
        let Some(set) = self.analyzer.value_set(value_set) else {
            return;
        };
        if set.members.is_empty() {
            return;
        }
        if !set.members.iter().any(|m| m.name.eq_ignore_ascii_case(raw)) {
            self.error(
                tok,
                "bad-flag",
                format!("`{raw}` is not a valid {value_set} flag"),
            );
        }
    }

    fn check_reference(&mut self, kind: RefKind, tok: &SyntaxToken) {
        self.check_reference_name(kind, tok, unquote(tok.text()));
    }

    fn check_reference_name(&mut self, kind: RefKind, tok: &SyntaxToken, name: &str) {
        let Some(index) = self.index else { return };
        // `None` is the universal null reference; `NoSound` is the audio one;
        // builtins (e.g. `Upgrade_Veterancy_*`) exist in no file by design.
        if name.is_empty()
            || name.eq_ignore_ascii_case("None")
            || (kind == RefKind::AudioEvent && name.eq_ignore_ascii_case("NoSound"))
            || self.analyzer.is_builtin(kind, name)
        {
            return;
        }
        if !index.is_defined(kind, name) {
            self.warning(
                tok,
                "unresolved-reference",
                format!("`{name}` is not defined anywhere in the workspace"),
            );
        }
    }

    fn error(&mut self, tok: &SyntaxToken, code: &'static str, message: String) {
        self.push(tok, Severity::Error, code, message);
    }

    fn warning(&mut self, tok: &SyntaxToken, code: &'static str, message: String) {
        self.push(tok, Severity::Warning, code, message);
    }

    fn hint(&mut self, tok: &SyntaxToken, code: &'static str, message: String) {
        self.push(tok, Severity::Hint, code, message);
    }

    fn push(&mut self, tok: &SyntaxToken, severity: Severity, code: &'static str, message: String) {
        debug_assert!(
            KNOWN_CODES.contains(&code),
            "`{code}` is not registered in KNOWN_CODES (suppression pragma)"
        );
        self.out.push(Diagnostic {
            span: tok.text_range().into(),
            severity,
            code,
            message,
        });
    }
}

fn has_extension(name: &str, extension: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, actual)| actual.eq_ignore_ascii_case(extension))
}

fn texture_exists(index: &WorkspaceIndex, ty: &ValueType, name: &str) -> bool {
    let exact = |candidate: &str| index.is_asset(AssetKind::Texture, candidate);
    match ty {
        ValueType::TextureFile if has_extension(name, "dds") => exact(name),
        ValueType::TextureFile if has_extension(name, "tga") => {
            exact(name) || exact(&format!("{}.dds", &name[..name.len() - 4]))
        }
        ValueType::TextureFile => false,
        ValueType::TextureStem => exact(&format!("{name}.tga")) || exact(&format!("{name}.dds")),
        ValueType::TextureSequenceStem => {
            exact(&format!("{name}.tga"))
                || exact(&format!("{name}.dds"))
                || exact(&format!("{name}0000.tga"))
                || exact(&format!("{name}0000.dds"))
        }
        _ => false,
    }
}

enum NumKind {
    Int,
    UInt,
    Real,
}

/// Does `node` directly contain a FIELD line whose key is `name`?
fn has_direct_field(node: &SyntaxNode, name: &str) -> bool {
    node.children()
        .filter(|n| n.kind() == SyntaxKind::FIELD)
        .filter_map(|n| Field(n).key())
        .any(|k| k.text() == name)
}

/// The value token of `node`'s direct `Conditions` field that equals `cond`
/// (case-insensitive, ignoring a `+` prefix), if any.
fn direct_condition_token(node: &SyntaxNode, cond: &str) -> Option<SyntaxToken> {
    node.children()
        .filter(|n| n.kind() == SyntaxKind::FIELD)
        .map(Field)
        .filter(|f| f.key().is_some_and(|k| k.text() == "Conditions"))
        .flat_map(|f| f.value_tokens())
        .find(|t| t.text().trim_start_matches('+').eq_ignore_ascii_case(cond))
}

/// Strip surrounding double quotes from a token's text, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .map(|s| s.strip_suffix('"').unwrap_or(s))
        .unwrap_or(s)
}

fn split_prefixed_token<'t, 'v>(
    tokens: &'t [SyntaxToken],
    ty: &'v ValueType,
) -> Option<(&'v ValueType, &'t SyntaxToken)> {
    let value = tokens.get(1)?;
    ty.split_prefix_value_type(unquote(tokens.first()?.text()))
        .map(|value_type| (value_type, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let a = Analyzer::embedded();
        diagnose(&a, &a.parse(src), None, None)
    }

    fn codes(src: &str) -> Vec<&'static str> {
        diags(src).into_iter().map(|d| d.code).collect()
    }

    #[test]
    fn clean_weapon_has_no_diagnostics() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\n  ClipSize = 30\nEnd\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn bare_percentages_are_opt_in() {
        let src = "Armor A\n  Armor = ARMOR_PIERCING 2.5\nEnd\n";
        assert!(codes(src).contains(&"bad-percent"));

        let mut analyzer = Analyzer::embedded();
        analyzer.set_allow_bare_percentages(true);
        let parse = analyzer.parse(src);
        assert!(!diagnose(&analyzer, &parse, None, None)
            .iter()
            .any(|d| d.code == "bad-percent"));

        let malformed = analyzer.parse("Armor A\n  Armor = ARMOR_PIERCING nope\nEnd\n");
        assert!(diagnose(&analyzer, &malformed, None, None)
            .iter()
            .any(|d| d.code == "bad-percent"));
    }

    #[test]
    fn unknown_block_is_error() {
        assert!(codes("Wepon AK47\nEnd\n").contains(&"unknown-block"));
    }

    #[test]
    fn set_reachability_flags_only_untriggered_player_upgrade_sets() {
        let upgrade_set =
            "  WeaponSet\n    Conditions = PLAYER_UPGRADE\n    Weapon = PRIMARY G\n  End\n";
        let trigger =
            "  Behavior = WeaponSetUpgrade ModuleTag_01\n    TriggeredBy = Upgrade_X\n  End\n";

        let dead = format!("Object T\n{upgrade_set}End\n");
        assert_eq!(
            codes(&dead)
                .iter()
                .filter(|c| **c == "unreachable-set")
                .count(),
            1
        );

        let alive = format!("Object T\n{upgrade_set}{trigger}End\n");
        assert!(
            !codes(&alive).contains(&"unreachable-set"),
            "module triggers the set"
        );

        // The TransportContain family can set the flag without an upgrade.
        let transport = format!(
            "Object T\n{upgrade_set}  Behavior = OverlordContain ModuleTag_02\n  End\nEnd\n"
        );
        assert!(!codes(&transport).contains(&"unreachable-set"));

        // Override patches (map.ini AddModule/RemoveModule) are partial: silent.
        let patched = format!("Object T\n  RemoveModule ModuleTag_99\n{upgrade_set}End\n");
        assert!(!codes(&patched).contains(&"unreachable-set"));

        // ObjectReskin inherits the parent's modules: silent.
        let reskin = format!("ObjectReskin T P\n{upgrade_set}End\n");
        assert!(!codes(&reskin).contains(&"unreachable-set"));

        // Other conditions (VETERAN etc.) are set externally: silent.
        let vet =
            "Object T\n  WeaponSet\n    Conditions = VETERAN\n    Weapon = PRIMARY G\n  End\nEnd\n";
        assert!(!codes(vet).contains(&"unreachable-set"));
    }

    #[test]
    fn weapon_set_upgrade_without_player_upgrade_weapon_set_warns() {
        let src = "\
Object T
  WeaponSet
    Conditions = NONE
    Weapon = PRIMARY G
  End
  Behavior = WeaponSetUpgrade ModuleTag_01
    TriggeredBy = Upgrade_X
  End
End
";
        let d = diags(src);
        assert!(d.iter().any(|d| {
            d.code == "unreachable-set"
                && d.severity == Severity::Warning
                && &src[d.span.start as usize..d.span.end as usize] == "WeaponSetUpgrade"
        }));
    }

    #[test]
    fn unknown_field_is_warning() {
        let src = "Weapon AK47\n  PrimaryDamg = 50.0\nEnd\n";
        let d = diags(src);
        assert!(d
            .iter()
            .any(|d| d.code == "unknown-field" && d.severity == Severity::Warning));
    }

    #[test]
    fn bad_bool_and_number_are_errors() {
        // ScaleWeaponSpeed is a Bool field; ClipSize is an Int field.
        let src = "Weapon AK47\n  ScaleWeaponSpeed = Maybe\n  ClipSize = lots\nEnd\n";
        let c = codes(src);
        assert!(c.contains(&"bad-bool"), "{c:?}");
        assert!(c.contains(&"bad-number"), "{c:?}");
    }

    #[test]
    fn bad_enum_member_is_error() {
        // DeathType is an Enum over TheDeathNames; BURNED is valid, NONSENSE is not.
        let ok = "Weapon AK47\n  DeathType = BURNED\nEnd\n";
        assert!(!codes(ok).contains(&"bad-enum"), "{:?}", diags(ok));
        let bad = "Weapon AK47\n  DeathType = NONSENSE\nEnd\n";
        assert!(codes(bad).contains(&"bad-enum"));
    }

    #[test]
    fn unterminated_block_is_syntax_error() {
        assert!(codes("Weapon AK47\n  ClipSize = 30\n").contains(&"syntax"));
    }

    #[test]
    fn object_module_fields_are_validated() {
        // ActiveBody.MaxHealth is Real; a bad value should be flagged, and an
        // unknown module field should warn.
        let src = "\
Object Tank
  Body = ActiveBody Tag01
    MaxHealth = lots
    Bogus = 1
  End
End
";
        let c = codes(src);
        assert!(c.contains(&"bad-number"), "{c:?}");
        assert!(c.contains(&"unknown-field"), "{c:?}");
    }

    #[test]
    fn transition_damage_particle_tokens_are_validated() {
        let ok = "\
Object Tank
  Behavior = TransitionDamageFX ModuleTag_01
    DamagedParticleSystem1 = Bone:NONE RandomBone:No PSys:StructureTransitionMediumSmoke
  End
End
";
        let ok_codes = codes(ok);
        assert!(!ok_codes.contains(&"bad-prefixed"), "{ok_codes:?}");
        assert!(!ok_codes.contains(&"bad-bool"), "{ok_codes:?}");

        let bad = "\
Object Tank
  Behavior = TransitionDamageFX ModuleTag_01
    DamagedParticleSystem1 = Bone:NONE RandomBone:Maybe ParticleSystem:StructureTransitionMediumSmoke
  End
End
";
        let bad_codes = codes(bad);
        assert!(bad_codes.contains(&"bad-bool"), "{bad_codes:?}");
        assert!(bad_codes.contains(&"bad-prefixed"), "{bad_codes:?}");
    }

    #[test]
    fn transition_damage_loc_tokens_are_validated() {
        let ok = "\
Object Tank
  Behavior = TransitionDamageFX ModuleTag_01
    DamagedFXList1 = Loc:X:0.0 Y:0.0 Z:0.0 FXList:FX_TankDamageTransition
  End
End
";
        let ok_codes = codes(ok);
        assert!(!ok_codes.contains(&"bad-prefixed"), "{ok_codes:?}");
        assert!(!ok_codes.contains(&"bad-number"), "{ok_codes:?}");

        let bad = "\
Object Tank
  Behavior = TransitionDamageFX ModuleTag_01
    DamagedFXList1 = Loc:X:0.0 Y:nope Z:0.0 FXList:FX_TankDamageTransition
  End
End
";
        assert!(codes(bad).contains(&"bad-number"));
    }

    #[test]
    fn unknown_module_warns() {
        let src = "Object Tank\n  Body = NotARealModule Tag01\n  End\nEnd\n";
        assert!(codes(src).contains(&"unknown-module"));
    }

    #[test]
    fn pragma_suppresses_listed_codes_file_wide() {
        let src = "; zerosyntax-disable: bad-bool, unknown-field\nWeapon AK47\n  ScaleWeaponSpeed = Maybe\n  ClipSize = lots\n  PrimaryDamg = 1\nEnd\n";
        let c = codes(src);
        assert!(!c.contains(&"bad-bool"), "{c:?}");
        assert!(!c.contains(&"unknown-field"), "{c:?}");
        assert!(c.contains(&"bad-number"), "unlisted codes survive: {c:?}");
    }

    #[test]
    fn pragma_typo_hints_and_suppresses_nothing() {
        let src = "; zerosyntax-disable: bad-bol\nWeapon AK47\n  ScaleWeaponSpeed = Maybe\nEnd\n";
        let d = diags(src);
        assert!(d.iter().any(|d| d.code == "bad-bool"), "{d:?}");
        let hint = d
            .iter()
            .find(|d| d.code == "unknown-suppression" && d.severity == Severity::Hint)
            .unwrap_or_else(|| panic!("expected unknown-suppression hint: {d:?}"));
        assert_eq!(
            &src[hint.span.start as usize..hint.span.end as usize],
            "bad-bol"
        );
    }

    #[test]
    fn pragma_inside_a_block_is_ignored() {
        let src =
            "Weapon AK47\n  ; zerosyntax-disable: bad-bool\n  ScaleWeaponSpeed = Maybe\nEnd\n";
        assert!(codes(src).contains(&"bad-bool"));
    }

    #[test]
    fn pragma_filters_over_cached_blocks() {
        // Warm the cache without a pragma, then insert one at the top via an
        // incremental reparse: the untouched second block keeps its cached
        // (unfiltered) diagnostics, and the filter must still apply to them.
        let a = Analyzer::embedded();
        let src = "Weapon A\n  ScaleWeaponSpeed = Maybe\nEnd\nWeapon B\n  ClipSize = lots\nEnd\n";
        let parse = a.parse(src);
        let mut cache = DiagnosticsCache::new();
        let first = diagnose_with_cache(&a, &parse, None, None, &mut cache);
        assert!(first.iter().any(|d| d.code == "bad-bool"));
        assert!(first.iter().any(|d| d.code == "bad-number"));

        let pragma = "; zerosyntax-disable: bad-number\n";
        let edited = format!("{pragma}{src}");
        let (inc, _strategy) = a.reparse(
            &parse,
            src,
            &edited,
            zerosyntax_syntax::Edit {
                start: 0,
                old_end: 0,
                new_len: pragma.len(),
            },
        );
        let second = diagnose_with_cache(&a, &inc, None, None, &mut cache);
        assert!(!second.iter().any(|d| d.code == "bad-number"), "{second:?}");
        assert!(second.iter().any(|d| d.code == "bad-bool"));
        assert_eq!(second, diagnose(&a, &inc, None, None));
    }

    #[test]
    fn cached_diagnose_matches_and_reuses_blocks() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\n  ClipSize = lots\nEnd\nWeapon B\nEnd\nWeapon C\nEnd\nWeapon D\n  PrimaryDamg = 1\nEnd\n";
        let parse = a.parse(src);
        let mut cache = DiagnosticsCache::new();
        assert_eq!(
            diagnose_with_cache(&a, &parse, None, None, &mut cache),
            diagnose(&a, &parse, None, None)
        );
        assert_eq!(cache.stats().0, 0, "first run is all misses");

        // Edit inside the *last* block; the splice widens one sibling, so the
        // first two blocks keep pointer-identical green nodes and their
        // diagnostics must come from the cache.
        let edited = src.replace("PrimaryDamg = 1", "PrimaryDamg = 2");
        let at = src.find('1').unwrap();
        let (inc, strategy) = a.reparse(
            &parse,
            src,
            &edited,
            zerosyntax_syntax::Edit {
                start: at,
                old_end: at + 1,
                new_len: 1,
            },
        );
        assert_eq!(strategy, zerosyntax_syntax::Strategy::Spliced);
        assert_eq!(
            diagnose_with_cache(&a, &inc, None, None, &mut cache),
            diagnose(&a, &inc, None, None)
        );
        assert!(cache.stats().0 >= 1, "unchanged block must hit the cache");
    }

    #[test]
    fn cache_invalidates_when_index_generation_changes() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        // Weapon block referencing a projectile-like name via an Object field
        // isn't trivial to stage; instead verify the mechanism: same parse,
        // index gains a definition between runs, output must follow suit.
        let src = "Weapon AK47\n  ClipSize = 30\nEnd\n";
        let parse = a.parse(src);
        let mut cache = DiagnosticsCache::new();
        let g0 = index.generation();
        let first = diagnose_with_cache(&a, &parse, Some(&index), None, &mut cache);
        index.set_file(
            "other.ini",
            crate::index::definitions_in(&a, &a.parse("Object X\nEnd\n"), "other.ini"),
        );
        assert_ne!(index.generation(), g0);
        let second = diagnose_with_cache(&a, &parse, Some(&index), None, &mut cache);
        assert_eq!(first, second); // no reference fields here; same result
        let (hits, _) = cache.stats();
        assert_eq!(hits, 0, "generation bump must clear the cache");
    }

    #[test]
    fn map_override_redefinition_hints_and_suppresses_reachability() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let base = "Object AmericaInfantryRanger\nEnd\n";
        index.set_file(
            "data/AmericaInfantry.ini",
            crate::index::definitions_in(&a, &a.parse(base), "data/AmericaInfantry.ini"),
        );
        // A map.ini redefinition with a PLAYER_UPGRADE WeaponSet and no
        // trigger module in sight: the base object's modules are inherited
        // (INI_LOAD_CREATE_OVERRIDES), so unreachable-set must stay silent.
        let src = "Object AmericaInfantryRanger\n  WeaponSet\n    Conditions = PLAYER_UPGRADE\n    Weapon = PRIMARY DefaultRangerCombatRifle\n  End\nEnd\n";
        let parse = a.parse(src);
        index.set_file(
            "maps/Map.ini",
            crate::index::definitions_in(&a, &parse, "maps/Map.ini"),
        );

        let diags = diagnose(&a, &parse, Some(&index), Some("maps/Map.ini"));
        assert!(
            diags
                .iter()
                .any(|d| d.code == "overrides" && d.severity == Severity::Hint),
            "expected `overrides` hint: {diags:?}"
        );
        assert!(
            !diags.iter().any(|d| d.code == "unreachable-set"),
            "override redefinition must skip reachability: {diags:?}"
        );

        // The base-game side is not an override and not a duplicate (the
        // other site lives in an override layer): no diagnostics there.
        let base_parse = a.parse(base);
        let base_diags = diagnose(
            &a,
            &base_parse,
            Some(&index),
            Some("data/AmericaInfantry.ini"),
        );
        assert!(
            !base_diags
                .iter()
                .any(|d| d.code == "overrides" || d.code == "duplicate-definition"),
            "base definition must stay clean: {base_diags:?}"
        );
    }

    #[test]
    fn solo_remove_module_includes_default_object_tags() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let base = "Object DefaultThingTemplate\n  Behavior = DestroyDie ModuleTag_DefaultDestroyDie\n  End\nEnd\n";
        let base_parse = a.parse(base);
        index.set_file_tags(
            "data/INI/Default/Object.ini",
            crate::index::module_tags_in(&a, &base_parse),
        );
        assert_eq!(
            index.effective_module_tag_locations(
                "NewMapObject",
                "ModuleTag_DefaultDestroyDie",
                Some("maps/solo.ini"),
                None,
            )[0]
            .file,
            "data/INI/Default/Object.ini"
        );

        let src = "Object NewMapObject\n  RemoveModule ModuleTag_DefaultDestroyDie\n  RemoveModule ModuleTag_Missing\nEnd\n";
        let parse = a.parse(src);
        let diags = diagnose(&a, &parse, Some(&index), Some("maps/solo.ini"));
        let unknown: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "unknown-module-tag")
            .collect();
        assert_eq!(unknown.len(), 1, "{diags:?}");
        assert_eq!(
            &src[unknown[0].span.start as usize..unknown[0].span.end as usize],
            "ModuleTag_Missing"
        );
        assert!(
            !diags
                .iter()
                .any(|diag| diag.code == "default-modules-not-removed"),
            "{diags:?}"
        );

        let unremoved = a.parse("Object AnotherMapObject\nEnd\n");
        let diags = diagnose(&a, &unremoved, Some(&index), Some("maps/solo.ini"));
        assert!(
            diags
                .iter()
                .any(|diag| diag.code == "default-modules-not-removed"),
            "{diags:?}"
        );
    }

    #[test]
    fn remove_module_rejects_tags_declared_later_in_the_same_map() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  RemoveModule ModuleTag_Later\n  Behavior = DestroyDie ModuleTag_Later\n  End\nEnd\n";
        let parse = a.parse(src);
        let mut index = WorkspaceIndex::new();
        index.set_file_tags("maps/map.ini", crate::index::module_tags_in(&a, &parse));
        let diags = diagnose(&a, &parse, Some(&index), Some("maps/map.ini"));
        assert!(
            diags.iter().any(|diag| diag.code == "unknown-module-tag"),
            "{diags:?}"
        );
    }

    #[test]
    fn map_forward_reference_allows_base_game_definition() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let base = "CommandButton Command_ConstructTank\n  Command = UNIT_BUILD\nEnd\n";
        index.set_file(
            "Data/INI/CommandButton.ini",
            crate::index::definitions_in(&a, &a.parse(base), "Data/INI/CommandButton.ini"),
        );

        let src = "\
CommandSet MapSet
  1 = Command_ConstructTank
End

CommandButton Command_ConstructTank
  Command = UNIT_BUILD
End
";
        let parse = a.parse(src);
        index.set_file(
            "maps/map.ini",
            crate::index::definitions_in(&a, &parse, "maps/map.ini"),
        );

        let diags = diagnose(&a, &parse, Some(&index), Some("maps/map.ini"));
        assert!(
            !diags.iter().any(|d| d.code == "map-forward-reference"),
            "base-game definitions are already loaded before map.ini: {diags:?}"
        );
    }

    #[test]
    fn map_ordering_classifies_only_source_proven_eager_references() {
        let eager = [
            "AI::parseScience",
            "AIUpdateModuleData::parseLocomotorSet",
            "ArmorStore::parseArmorTemplate",
            "BoneFXUpdateModuleData::parseFXList",
            "BoneFXUpdateModuleData::parseObjectCreationList",
            "BoneFXUpdateModuleData::parseParticleSystem",
            "CommandSet::parseCommandButton",
            "DamageFX::parseMajorFXList",
            "DamageFX::parseMinorFXList",
            "DamageFXStore::parseDamageFX",
            "INI::parseFXList",
            "INI::parseMappedImage",
            "INI::parseObjectCreationList",
            "INI::parseParticleSystemTemplate",
            "INI::parseScience",
            "INI::parseScienceVector",
            "INI::parseSpecialPowerTemplate",
            "INI::parseThingTemplate",
            "INI::parseUpgradeTemplate",
            "INI::parseWeaponTemplate",
            "ProductionPrerequisite::parsePrerequisiteScience",
            "ProductionPrerequisite::parsePrerequisiteUnit",
            "TransitionDamageFXModuleData::parseFXList",
            "TransitionDamageFXModuleData::parseObjectCreationList",
            "TransitionDamageFXModuleData::parseParticleSystem",
            "WeaponTemplateSet::parseWeapon",
            "parseAllVetLevelsFXList",
            "parseAllVetLevelsPSys",
            "parseAngleFX",
            "parseBountyUpgradePair",
            "parseCashHackUpgradePair",
            "parseFX",
            "parseOCL",
            "parseOCLUpgradePair",
            "parseParticleSysBone",
            "parsePerVetLevelFXList",
            "parsePerVetLevelPSys",
            "parseWeapon",
        ];
        let mut field = SchemaField {
            name: "Test".into(),
            value_type: ValueType::Reference {
                ref_kind: RefKind::Object,
            },
            parse_fn: String::new(),
            doc: None,
            model_source: None,
            model_member_mode: None,
        };
        for parser in eager {
            field.parse_fn = parser.into();
            assert!(
                eager_map_reference(&ScopeSchema::Unknown, &field, RefKind::Object),
                "{parser}"
            );
        }
        field.parse_fn = "INI::parseAsciiString".into();
        assert!(!eager_map_reference(
            &ScopeSchema::Unknown,
            &field,
            RefKind::Object
        ));

        let a = Analyzer::embedded();
        let ocl_update = a.module("OCLUpdate").unwrap();
        let faction_ocl = ocl_update
            .fields
            .iter()
            .find(|field| field.name == "FactionOCL")
            .unwrap();
        assert!(eager_map_reference(
            &ScopeSchema::Module(ocl_update),
            faction_ocl,
            RefKind::ObjectCreationList
        ));
        assert!(!eager_map_reference(
            &ScopeSchema::Module(ocl_update),
            faction_ocl,
            RefKind::PlayerTemplate
        ));

        let object = a.block("Object").unwrap();
        let object_image = object
            .fields
            .iter()
            .find(|field| field.name == "ButtonImage")
            .unwrap();
        assert!(eager_map_reference(
            &ScopeSchema::Block(object),
            object_image,
            RefKind::MappedImage
        ));
        for block_name in ["CommandButton", "Upgrade"] {
            let block = a.block(block_name).unwrap();
            let image = block
                .fields
                .iter()
                .find(|field| field.name == "ButtonImage")
                .unwrap();
            assert!(!eager_map_reference(
                &ScopeSchema::Block(block),
                image,
                RefKind::MappedImage
            ));
        }
    }

    #[test]
    fn map_reference_extraction_handles_prefixed_and_trailing_lists() {
        let a = Analyzer::embedded();
        let parse =
            a.parse("Object X\n  Test = Faction:LateFaction OCL:LateOCL ExtraA ExtraB\nEnd\n");
        let field = Block(parse.syntax().children().next().unwrap())
            .fields()
            .next()
            .unwrap();
        let prefixed = ValueType::TokenList {
            tokens: vec![
                ValueType::Prefixed {
                    prefix: "Faction".into(),
                    value_type: Box::new(ValueType::Reference {
                        ref_kind: RefKind::PlayerTemplate,
                    }),
                },
                ValueType::Prefixed {
                    prefix: "OCL".into(),
                    value_type: Box::new(ValueType::Reference {
                        ref_kind: RefKind::ObjectCreationList,
                    }),
                },
            ],
        };
        let refs = reference_tokens(&field, &prefixed);
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["LateFaction", "LateOCL"]
        );

        let split_parse = a.parse("Object X\n  Test = Faction: LateFaction OCL: LateOCL\nEnd\n");
        let split_field = Block(split_parse.syntax().children().next().unwrap())
            .fields()
            .next()
            .unwrap();
        let refs = reference_tokens(&split_field, &prefixed);
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["LateFaction", "LateOCL"]
        );

        let trailing = ValueType::TokenList {
            tokens: vec![
                ValueType::AsciiString,
                ValueType::ReferenceList {
                    ref_kind: RefKind::FxList,
                },
            ],
        };
        let refs = reference_tokens(&field, &trailing);
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["OCL:LateOCL", "ExtraA", "ExtraB"]
        );
    }

    #[test]
    fn map_forward_reference_message_is_actionable_and_suppressible() {
        let a = Analyzer::embedded();
        let body = "CommandSet MapSet\n  1 = LateButton\nEnd\n\nCommandButton LateButton\n  Command = UNIT_BUILD\nEnd\n";
        let parse = a.parse(body);
        let diag = diagnose(&a, &parse, None, Some("maps/map.ini"))
            .into_iter()
            .find(|diag| diag.code == "map-forward-reference")
            .unwrap();
        for expected in [
            "CommandButton LateButton",
            "CommandSet MapSet",
            "slot `1`",
            "`map.ini`",
            "Move `CommandButton LateButton` above `CommandSet MapSet`",
        ] {
            assert!(diag.message.contains(expected), "{}", diag.message);
        }

        let suppressed = a.parse(&format!(
            "; zerosyntax-disable: map-forward-reference\n{body}"
        ));
        assert!(!diagnose(&a, &suppressed, None, Some("maps/map.ini"))
            .iter()
            .any(|diag| diag.code == "map-forward-reference"));

        let reskin = a.parse("ObjectReskin Child Parent\nEnd\n\nObject Parent\nEnd\n");
        let message = diagnose(&a, &reskin, None, Some("maps/map.ini"))
            .into_iter()
            .find(|diag| diag.code == "map-forward-reference")
            .unwrap()
            .message;
        assert!(message.contains("`ObjectReskin` requires its parent"));
        assert!(message.contains("Move `Object Parent` above `ObjectReskin Child Parent`"));
    }

    #[test]
    fn map_reskin_parent_allows_base_game_definition() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let base = a.parse("Object Parent\nEnd\n");
        index.set_file(
            "Data/INI/Object.ini",
            crate::index::definitions_in(&a, &base, "Data/INI/Object.ini"),
        );
        let map = a.parse("ObjectReskin Child Parent\nEnd\n\nObject Parent\nEnd\n");
        index.set_file(
            "maps/map.ini",
            crate::index::definitions_in(&a, &map, "maps/map.ini"),
        );
        assert!(!diagnose(&a, &map, Some(&index), Some("maps/map.ini"))
            .iter()
            .any(|diag| diag.code == "map-forward-reference"));
    }

    #[test]
    fn duplicate_object_definition_outside_overrides_warns() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let src = "Object Tank\nEnd\n";
        index.set_file(
            "a.ini",
            crate::index::definitions_in(&a, &a.parse(src), "a.ini"),
        );
        let parse = a.parse(src);
        index.set_file("b.ini", crate::index::definitions_in(&a, &parse, "b.ini"));
        // The engine DEBUG_CRASHes on duplicate object templates outside
        // override mode (ThingFactory.cpp "Duplicate factionunit").
        let diags = diagnose(&a, &parse, Some(&index), Some("b.ini"));
        assert!(
            diags
                .iter()
                .any(|d| d.code == "duplicate-definition" && d.severity == Severity::Warning),
            "expected duplicate-definition: {diags:?}"
        );
    }

    #[test]
    fn reference_resolution_with_index() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        // Define nothing; a Weapon reference on an Object should be unresolved.
        // Object.weapon references go through WeaponSet (custom parser), so use a
        // field we know maps to a reference: PrimaryDamageRadius is Real, so
        // instead assert the plumbing via a synthetic check below.
        index.set_file(
            "a.ini",
            crate::index::definitions_in(&a, &a.parse("Weapon AK47\nEnd\n"), "a.ini"),
        );
        assert!(index.is_defined(RefKind::Weapon, "AK47"));
    }

    #[test]
    fn model_assets_validate_models_and_members_when_indexed() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "Good".into(),
                members: vec!["Tire01".into(), "Cargo01".into(), "Muzzle01".into()],
            }],
        );
        let src = "\
Object Tank
  Draw = W3DTruckDraw ModuleTag_01
    DefaultConditionState
      Model = MissingModel
      HideSubObject = MissingBone
      WeaponFireFXBone = PRIMARY Muzzle
      WeaponLaunchBone = BAD_SLOT Muzzle
      WeaponRecoilBone = PRIMARY MissingMuzzle
    End
  End
  Behavior = BoneFXUpdate ModuleTag_02
    PristineFXList1 = Bone: SplitMissing OnlyOnce: No 0 0 FXList: None
  End
End
";
        let parse = a.parse(src);
        let diags = diagnose(&a, &parse, Some(&index), Some("test.ini"));
        assert!(diags.iter().any(|d| d.code == "unknown-model"), "{diags:?}");
        assert!(
            !diags.iter().any(|d| d.code == "unknown-model-member"),
            "unknown model should not cascade into member warnings: {diags:?}"
        );

        let src = src.replace("MissingModel", "Good");
        let parse = a.parse(&src);
        let diags = diagnose(&a, &parse, Some(&index), Some("test.ini"));
        assert!(
            diags.iter().any(|d| d.code == "unknown-model-member"),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| {
                d.code == "bad-enum"
                    && &src[d.span.start as usize..d.span.end as usize] == "BAD_SLOT"
            }),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| {
                d.code == "unknown-model-member"
                    && &src[d.span.start as usize..d.span.end as usize] == "MissingMuzzle"
            }),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| {
                d.code == "unknown-model-member"
                    && &src[d.span.start as usize..d.span.end as usize] == "SplitMissing"
            }),
            "{diags:?}"
        );
        assert!(!diags.iter().any(|d| {
            d.code == "unknown-model-member"
                && &src[d.span.start as usize..d.span.end as usize] == "Muzzle"
        }));
    }

    #[test]
    fn condition_state_inherits_model_from_sibling_default_state() {
        // Real game data: a ConditionState that sets no Model inherits the
        // DefaultConditionState's model, so its bones validate against it.
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "Good".into(),
                members: vec!["Turret01".into()],
            }],
        );
        let src = "\
Object Tank
  Draw = W3DTankDraw ModuleTag_01
    DefaultConditionState
      Model = Good
    End
    ConditionState = DAMAGED
      HideSubObject = Turret01
      ShowSubObject = MissingBone
    End
  End
End
";
        let parse = a.parse(src);
        let diags = diagnose(&a, &parse, Some(&index), Some("test.ini"));
        assert!(
            !diags.iter().any(|d| {
                d.code == "unknown-model-member"
                    && &src[d.span.start as usize..d.span.end as usize] == "Turret01"
            }),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| {
                d.code == "unknown-model-member"
                    && &src[d.span.start as usize..d.span.end as usize] == "MissingBone"
            }),
            "{diags:?}"
        );
    }

    #[test]
    fn ocl_members_resolve_through_transport_object() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/A10.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "A10".into(),
                members: vec!["WeaponA01".into(), "Missile01".into()],
            }],
        );
        let object = "Object AmericaJetA10Thunderbolt\n  Draw = W3DModelDraw ModuleTag_Draw\n    DefaultConditionState\n      Model = A10\n    End\n  End\nEnd\n";
        let object_parse = a.parse(object);
        index.set_file(
            "objects.ini",
            crate::index::definitions_in(&a, &object_parse, "objects.ini"),
        );
        index.set_file_object_models(
            "objects.ini",
            crate::index::object_models_in(&a, &object_parse),
        );

        let src = "ObjectCreationList Strike\n  DeliverPayload\n    Transport = AmericaJetA10Thunderbolt\n    VisibleDropBoneBaseName = WeaponA\n    VisibleSubObjectBaseName = Missing\n  End\nEnd\n";
        let parse = a.parse(src);
        let diags = diagnose(&a, &parse, Some(&index), Some("ocl.ini"));
        assert!(
            !diags.iter().any(|d| d.code == "unknown-model-member"
                && &src[d.span.start as usize..d.span.end as usize] == "WeaponA"),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.code == "unknown-model-member"
                && &src[d.span.start as usize..d.span.end as usize] == "Missing"),
            "{diags:?}"
        );

        let missing = src.replace("AmericaJetA10Thunderbolt", "MissingTransport");
        let diags = diagnose(&a, &a.parse(&missing), Some(&index), Some("ocl.ini"));
        assert!(
            diags.iter().any(|d| d.code == "unresolved-reference"),
            "{diags:?}"
        );
        assert!(
            !diags.iter().any(|d| d.code == "unknown-model-member"),
            "{diags:?}"
        );
    }

    #[test]
    fn ocl_object_and_model_lists_validate_every_reference() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let objects = a.parse("Object KnownObject\nEnd\n");
        index.set_file(
            "objects.ini",
            crate::index::definitions_in(&a, &objects, "objects.ini"),
        );
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "Good".into(),
                members: vec![],
            }],
        );
        let src = "ObjectCreationList Test\n  CreateObject\n    ObjectNames = KnownObject MissingObject\n  End\n  CreateDebris\n    ModelNames = Good MissingModel\n  End\nEnd\n";
        let diags = diagnose(&a, &a.parse(src), Some(&index), Some("ocl.ini"));
        assert!(diags.iter().any(|d| d.code == "unresolved-reference"
            && &src[d.span.start as usize..d.span.end as usize] == "MissingObject"));
        assert!(diags.iter().any(|d| d.code == "unknown-model"
            && &src[d.span.start as usize..d.span.end as usize] == "MissingModel"));
    }

    #[test]
    fn model_member_strictness_supports_off_compatible_and_strict() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "a.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "A".into(),
                members: vec!["Bone01".into()],
            }],
        );
        index.set_file_models(
            "b.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "B".into(),
                members: vec![],
            }],
        );
        let src = "Object Tank\n  Draw = W3DModelDraw Tag\n    DefaultConditionState\n      Model = A\n      Model = B\n      HideSubObject = Bone\n    End\n  End\nEnd\n";
        let parse = a.parse(src);
        assert!(!diagnose(&a, &parse, Some(&index), None)
            .iter()
            .any(|d| d.code == "unknown-model-member"));
        index.set_model_member_strictness(ModelMemberStrictness::Strict);
        assert!(diagnose(&a, &parse, Some(&index), None)
            .iter()
            .any(|d| d.code == "unknown-model-member"));
        index.set_model_member_strictness(ModelMemberStrictness::Off);
        assert!(!diagnose(&a, &parse, Some(&index), None)
            .iter()
            .any(|d| d.code == "unknown-model-member"));
    }

    #[test]
    fn raw_asset_warnings_are_gated_per_kind() {
        let a = Analyzer::embedded();
        let src = "DialogEvent Dialog\n  Filename = Missing.wav\nEnd\nMappedImage Image\n  Texture = Missing.tga\nEnd\n";
        let parse = a.parse(src);
        let mut index = WorkspaceIndex::new();
        let codes = |index: &WorkspaceIndex| {
            diagnose(&a, &parse, Some(index), None)
                .into_iter()
                .map(|diagnostic| diagnostic.code)
                .collect::<Vec<_>>()
        };
        assert!(!codes(&index)
            .iter()
            .any(|code| code.starts_with("unknown-")));
        index.set_file_assets(
            "audio",
            vec![crate::index::FileAsset {
                kind: AssetKind::Audio,
                name: "Known.wav".into(),
                uri: "file:///Known.wav".into(),
            }],
        );
        let audio_only = codes(&index);
        assert!(audio_only.contains(&"unknown-audio-file"));
        assert!(!audio_only.contains(&"unknown-texture"));
        index.set_file_assets(
            "texture",
            vec![crate::index::FileAsset {
                kind: AssetKind::Texture,
                name: "Known.dds".into(),
                uri: "file:///Known.dds".into(),
            }],
        );
        assert!(codes(&index).contains(&"unknown-texture"));
    }
}
