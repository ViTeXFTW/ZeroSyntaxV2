//! Context-aware completions.
//!
//! Resolves what is valid at a byte offset and returns candidate items:
//! * file scope -> top-level block keywords;
//! * inside a block/module, at the start of a line -> field names + module slots;
//! * after `=` -> enum/bitflag members, `Yes`/`No`, module names, or (with the
//!   workspace index) names of the referenced definition kind.

use zerosyntax_schema::{AudioExtension, RefKind, ValueType};
use zerosyntax_syntax::ast::{Block, Field, Module};
use zerosyntax_syntax::{Parse, SyntaxKind, SyntaxNode};

use crate::index::AssetKind;
use crate::model::{
    is_model_asset_type, is_model_member_type, model_member_mode, model_member_names,
    models_for_source, scope_schema,
};
use crate::{Analyzer, WorkspaceIndex};

/// The role of a completion item, mapped to LSP `CompletionItemKind` by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Block,
    Field,
    Module,
    EnumMember,
    Value,
    Reference,
    W3dModel,
    W3dAnimation,
}

/// A single completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    /// Optional Markdown shown by an LSP client alongside the selected item.
    pub documentation: Option<String>,
    /// Optional LSP snippet string. When the client supports snippets, the
    /// server uses this as `insertText` with `InsertTextFormat::SNIPPET`
    /// instead of the plain `label`. `$0` marks the final cursor position;
    /// `${N:placeholder}` marks tab-stops. `None` means plain-label insertion.
    pub insert: Option<String>,
}

/// Compute completions at byte `offset`.
/// `file` is the current document URI, used for string-key lookup from co-located `.str` files.
pub fn complete(
    analyzer: &Analyzer,
    parse: &Parse,
    offset: u32,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
) -> Vec<Completion> {
    let root = parse.syntax();
    let ctx = classify_position(analyzer, &root, offset, file);
    match ctx {
        PosContext::TopLevel => top_level_completions(analyzer),
        PosContext::ObjectName => object_name_completions(index),
        PosContext::FieldKey(scope_node) => field_key_completions(analyzer, &scope_node),
        PosContext::FieldValue {
            scope_node,
            key,
            value_index,
            current_token,
            first_token,
        } => field_value_completions(
            analyzer,
            &scope_node,
            &key,
            value_index,
            (current_token.as_deref(), first_token.as_deref()),
            index,
            (file, offset),
        ),
        PosContext::ModuleName {
            scope_node,
            slot_accepts,
        } => module_name_completions(analyzer, &scope_node, &slot_accepts),
        PosContext::SubBlockArg { argument_type } => {
            completions_for_type(analyzer, &argument_type, 0, None, None, index)
        }
    }
}

enum PosContext {
    TopLevel,
    /// Completing a top-level Object name in a map override layer. Existing
    /// objects are useful override targets, but this is deliberately a
    /// completion context rather than a Reference value: map files may also
    /// introduce entirely new object names.
    ObjectName,
    /// Completing a field/slot keyword inside this scope node.
    FieldKey(SyntaxNode),
    /// Completing the value of `key` inside this scope node; `value_index` is
    /// how many value tokens already precede the cursor (the position within
    /// a token-list value).
    FieldValue {
        scope_node: SyntaxNode,
        key: String,
        value_index: usize,
        current_token: Option<String>,
        first_token: Option<String>,
    },
    /// Completing a module type name after a slot `=`. Carries the slot's
    /// enclosing scope so its snippet can choose an unused-looking module tag,
    /// and accepted interfaces so candidates can be filtered to valid modules.
    ModuleName {
        scope_node: SyntaxNode,
        slot_accepts: Vec<String>,
    },
    /// Completing the argument of a sub-block header.
    SubBlockArg {
        argument_type: zerosyntax_schema::ValueType,
    },
}

fn classify_position(
    analyzer: &Analyzer,
    root: &SyntaxNode,
    offset: u32,
    file: Option<&str>,
) -> PosContext {
    let off = rowan::TextSize::from(offset.min(root.text_range().end().into()));
    let element = root.covering_element(rowan::TextRange::empty(off));
    let node = match &element {
        rowan::NodeOrToken::Node(n) => n.clone(),
        rowan::NodeOrToken::Token(t) => t.parent().unwrap_or_else(|| root.clone()),
    };

    // Are we on a FIELD line? (most common while typing `Key = value`)
    if let Some(field_node) = ancestor_of_kind(&node, SyntaxKind::FIELD) {
        let scope_node = enclosing_scope(&field_node);
        let field = Field(field_node.clone());
        let after_key = field
            .key()
            .is_some_and(|key| u32::from(key.text_range().end()) < offset);
        if after_equals(&field_node, offset) || after_key {
            let key = field
                .key()
                .map(|k| k.text().to_string())
                .unwrap_or_default();
            let value_tokens = field.value_tokens();
            let raw_value_index = value_tokens
                .iter()
                .filter(|t| u32::from(t.text_range().end()) < offset)
                .count();
            let input = value_tokens
                .iter()
                .map(|token| token.text().trim_matches('"'))
                .collect::<Vec<_>>();
            let value_index = scope_node
                .as_ref()
                .and_then(|scope_node| scope_schema(analyzer, scope_node).field(&key))
                .and_then(|field| {
                    // Animation's following tokens are optional distance and
                    // repeat count, despite its lenient string schema type.
                    if field.parse_fn == "parseAnimation" {
                        return None;
                    }
                    field
                        .value_type
                        .token_index_at_input(&input, raw_value_index)
                })
                .unwrap_or(raw_value_index);
            let current_token = value_tokens
                .iter()
                .position(|t| {
                    let range = t.text_range();
                    u32::from(range.start()) <= offset && offset <= u32::from(range.end())
                })
                .map(|index| {
                    let current = value_tokens[index].text().trim_matches('"');
                    index
                        .checked_sub(1)
                        .and_then(|index| value_tokens.get(index))
                        .map(|previous| previous.text().trim_matches('"'))
                        .filter(|previous| previous.ends_with(':'))
                        .map_or_else(
                            || current.to_string(),
                            |previous| format!("{previous}{current}"),
                        )
                })
                .or_else(|| {
                    raw_value_index
                        .checked_sub(1)
                        .and_then(|index| input.get(index))
                        .filter(|previous| previous.ends_with(':'))
                        .map(|previous| (*previous).to_string())
                });
            let first_token = value_tokens
                .first()
                .map(|t| t.text().trim_matches('"').to_string());
            return PosContext::FieldValue {
                scope_node: scope_node.unwrap_or_else(|| root.clone()),
                key,
                value_index,
                current_token,
                first_token,
            };
        }
        return match scope_node {
            Some(s) => PosContext::FieldKey(s),
            None => PosContext::TopLevel,
        };
    }

    // On a MODULE header line, after `=`, completing the module type name.
    if let Some(module_node) = ancestor_of_kind(&node, SyntaxKind::MODULE) {
        // Only treat as module-name context if the cursor is on the header line
        // (before any nested field/scope) and after `=`, and the slot is a real
        // module slot of the parent block.
        if on_header_line(&module_node, offset) && after_equals(&module_node, offset) {
            let scope_node = enclosing_scope(&module_node);
            let parent = scope_node.as_ref().map(|p| scope_schema(analyzer, p));
            let slot = Module(module_node.clone()).slot();
            let slot_accepts = slot.as_ref().and_then(|s| {
                parent.as_ref().and_then(|p| {
                    p.module_slots()
                        .iter()
                        .find(|ms| ms.keyword == s.text())
                        .map(|ms| ms.accepts.clone())
                })
            });
            if let Some(accepts) = slot_accepts {
                return PosContext::ModuleName {
                    scope_node: scope_node.unwrap_or_else(|| root.clone()),
                    slot_accepts: accepts,
                };
            }
            // Check for a sub-block with an argument_type (e.g. ConditionState = <flags>).
            if let Some(arg_type) = slot
                .as_ref()
                .and_then(|s| sub_block_arg_type(parent.as_ref(), s.text()))
            {
                return PosContext::SubBlockArg {
                    argument_type: arg_type,
                };
            }
        } else if on_header_line(&module_node, offset) {
            let parent = enclosing_scope(&module_node).map(|p| scope_schema(analyzer, &p));
            let slot = Module(module_node.clone()).slot();
            if let Some(arg_type) = slot.as_ref().and_then(|s| {
                (offset >= u32::from(s.text_range().end()))
                    .then(|| sub_block_arg_type(parent.as_ref(), s.text()))
                    .flatten()
            }) {
                return PosContext::SubBlockArg {
                    argument_type: arg_type,
                };
            }
        }
        // Otherwise we're inside the module body -> completing a field key.
        return PosContext::FieldKey(module_node);
    }

    // Inside a BLOCK body -> field key completion for that block.
    // Special case: if the cursor is on the block's keyword token at file scope
    // with no `=` before it, the user is typing a block keyword -> offer block
    // names (TopLevel) so the popup appears while typing.
    if let Some(block_node) = ancestor_of_kind(&node, SyntaxKind::BLOCK) {
        let is_top_level = block_node
            .parent()
            .map(|p| p.kind() == SyntaxKind::ROOT)
            .unwrap_or(false);
        if is_top_level && on_header_line(&block_node, offset) && !after_equals(&block_node, offset)
        {
            if let Some(kw) = Block(block_node.clone()).keyword() {
                if offset <= u32::from(kw.text_range().end()) {
                    return PosContext::TopLevel;
                }
                if file.is_some_and(is_override_layer) && kw.text().eq_ignore_ascii_case("Object") {
                    return PosContext::ObjectName;
                }
            }
        }
        return PosContext::FieldKey(block_node);
    }

    // Not inside anything -> file scope.
    PosContext::TopLevel
}

fn is_override_layer(file: &str) -> bool {
    file.rsplit(['/', '\\']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("map.ini") || name.eq_ignore_ascii_case("solo.ini")
    })
}

fn object_name_completions(index: Option<&WorkspaceIndex>) -> Vec<Completion> {
    index
        .into_iter()
        .flat_map(|idx| idx.override_target_names(RefKind::Object))
        .map(|name| Completion {
            label: name.to_string(),
            kind: CompletionKind::Reference,
            detail: Some("Object (override target)".into()),
            documentation: None,
            insert: None,
        })
        .collect()
}

fn field_key_completions(analyzer: &Analyzer, scope_node: &SyntaxNode) -> Vec<Completion> {
    let scope = scope_schema(analyzer, scope_node);
    let mut out: Vec<Completion> = scope
        .fields()
        .iter()
        .map(|f| Completion {
            label: f.name.clone(),
            kind: CompletionKind::Field,
            detail: Some(type_label(&f.value_type)),
            documentation: None,
            insert: value_snippet(&f.value_type).map(|value| format!("{} = {value}", f.name)),
        })
        .collect();
    for slot in scope.module_slots() {
        // Snippet: `Slot = $0` so cursor lands at the module-name position.
        let insert = Some(format!("{} = $0", slot.keyword));
        out.push(Completion {
            label: slot.keyword.clone(),
            kind: CompletionKind::Field,
            detail: Some("module slot".into()),
            documentation: None,
            insert,
        });
    }
    for sub in scope.sub_blocks() {
        // Snippet: include `= ${1:NONE}` argument placeholder when the sub-block takes one.
        let insert = if sub.argument_type.is_some() {
            let arg = sub
                .argument_type
                .as_ref()
                .map(argument_placeholder)
                .unwrap_or("NONE".into());
            if space_separated_sub_block_arg(&sub.keyword) {
                Some(format!("{} ${{1:{arg}}}\n\t$0\nEnd", sub.keyword))
            } else {
                Some(format!("{} = ${{1:{arg}}}\n\t$0\nEnd", sub.keyword))
            }
        } else {
            Some(format!("{}\n\t$0\nEnd", sub.keyword))
        };
        out.push(Completion {
            label: sub.keyword.clone(),
            kind: CompletionKind::Block,
            detail: Some("sub-block".into()),
            documentation: None,
            insert,
        });
    }
    out
}

fn sub_block_arg_type(
    scope: Option<&crate::model::ScopeSchema<'_>>,
    keyword: &str,
) -> Option<zerosyntax_schema::ValueType> {
    scope?
        .sub_blocks()
        .iter()
        .find(|sb| sb.keyword == keyword)
        .and_then(|sb| sb.argument_type.clone())
}

fn space_separated_sub_block_arg(keyword: &str) -> bool {
    matches!(keyword, "SideInfo" | "SkirmishBuildList" | "Structure")
}

fn argument_placeholder(ty: &ValueType) -> String {
    match ty {
        ValueType::Enum { value_set } if value_set == "ai_side" => "America".into(),
        ValueType::Reference { ref_kind } => format!("{ref_kind:?}"),
        _ => "NONE".into(),
    }
}

fn field_value_completions(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    key: &str,
    value_index: usize,
    tokens: (Option<&str>, Option<&str>),
    index: Option<&WorkspaceIndex>,
    position: (Option<&str>, u32),
) -> Vec<Completion> {
    let (current_token, first_token) = tokens;
    let (file, offset) = position;
    // RemoveModule / ReplaceModule: suggest module tags from the origin object.
    if key.eq_ignore_ascii_case("RemoveModule") || key.eq_ignore_ascii_case("ReplaceModule") {
        if let Some(idx) = index {
            let obj_name = Block(scope_node.clone())
                .name()
                .map(|n| n.text().to_string())
                .unwrap_or_default();
            if !obj_name.is_empty() {
                let tags: Vec<Completion> = idx
                    .effective_module_tags_for_object(&obj_name, file, Some(offset))
                    .into_iter()
                    .map(|tag| Completion {
                        label: tag.name.to_string(),
                        kind: CompletionKind::Reference,
                        detail: Some("module tag".into()),
                        documentation: (!tag.snippet.is_empty())
                            .then(|| format!("```ini\n{}\n```", tag.snippet)),
                        insert: None,
                    })
                    .collect();
                if !tags.is_empty() {
                    return tags;
                }
            }
        }
    }
    // DisplayName: add string table keys from the companion .str file when available.
    let mut base = {
        let scope = scope_schema(analyzer, scope_node);
        if let Some(f) = scope.field(key) {
            if f.parse_fn == "parseAnimation" {
                return if value_index == 0 {
                    index
                        .map(|index| {
                            crate::model::condition_state_model(scope_node)
                                .into_iter()
                                .flat_map(|model| index.model_animations(&model))
                                .map(|name| Completion {
                                    label: name.to_string(),
                                    kind: CompletionKind::W3dAnimation,
                                    detail: Some("W3D animation".into()),
                                    documentation: None,
                                    insert: None,
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
            }
            if let Some(asset_completions) = model_asset_completions(
                analyzer,
                scope_node,
                f,
                (value_index, current_token, first_token),
                index,
                scope_node
                    .children()
                    .find(|node| {
                        node.kind() == SyntaxKind::FIELD
                            && u32::from(node.text_range().start()) <= offset
                            && offset <= u32::from(node.text_range().end())
                    })
                    .map(Field)
                    .as_ref(),
            ) {
                asset_completions
            } else {
                completions_for_type(
                    analyzer,
                    &f.value_type,
                    value_index,
                    current_token,
                    first_token,
                    index,
                )
            }
        } else {
            Vec::new()
        }
    };
    if key.eq_ignore_ascii_case("DisplayName") {
        if let (Some(idx), Some(f)) = (index, file) {
            base.extend(idx.string_keys_for_ini(f).map(|k| Completion {
                label: k.to_string(),
                kind: CompletionKind::Value,
                detail: Some("string key".into()),
                documentation: None,
                insert: None,
            }));
        }
    }
    base
}

fn model_asset_completions(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    field_schema: &zerosyntax_schema::Field,
    position: (usize, Option<&str>, Option<&str>),
    index: Option<&WorkspaceIndex>,
    field: Option<&Field>,
) -> Option<Vec<Completion>> {
    let index = index?;
    if !index.has_model_assets() {
        return None;
    }
    let (value_index, current_token, first_token) = position;
    let ty = field_schema
        .value_type
        .variant_for_first_token(first_token)?
        .token_type_at(value_index)?;
    let (ty, prefix) = match ty {
        ValueType::Prefixed { prefix, value_type } => (value_type.as_ref(), Some(prefix)),
        _ => (ty, None),
    };
    if is_model_asset_type(ty) {
        return Some(
            index
                .model_names()
                .map(|name| Completion {
                    label: name.to_string(),
                    kind: CompletionKind::W3dModel,
                    detail: Some("W3D model".into()),
                    documentation: None,
                    insert: None,
                })
                .collect(),
        );
    }
    if !is_model_member_type(ty) {
        return None;
    }
    let mode = model_member_mode(field_schema.model_member_mode, field);
    let mut seen = std::collections::HashSet::new();
    let out = models_for_source(
        analyzer,
        scope_node,
        field_schema.model_source.as_ref(),
        index,
    )
    .into_iter()
    .flat_map(|model| {
        index
            .model_members(&model)
            .flat_map(|member| {
                model_member_names(member, mode)
                    .into_iter()
                    .map(move |name| {
                        (
                            name.to_string(),
                            mode.is_some() && name != member.rsplit('.').next().unwrap_or(member),
                        )
                    })
            })
            .collect::<Vec<_>>()
    })
    .filter(|(member, _)| seen.insert(member.to_ascii_lowercase()))
    .map(|(member, family)| Completion {
        insert: prefix
            .filter(|prefix| {
                !current_token
                    .and_then(|t| t.split_once(':'))
                    .is_some_and(|(actual, _)| actual.eq_ignore_ascii_case(prefix))
            })
            .map(|prefix| format!("{prefix}:{member}")),
        detail: Some(if family {
            "W3D numbered bone family (01, 02, ...)".into()
        } else {
            "W3D model member".into()
        }),
        label: member,
        kind: CompletionKind::Reference,
        documentation: None,
    })
    .collect();
    Some(out)
}

/// Build a single-token snippet placeholder for a value type, used when
/// generating a full-sequence snippet for TokenList or structured types.
/// `n` is the tab-stop index (1-based).
fn type_snippet_placeholder(ty: &ValueType, n: usize) -> String {
    match ty {
        ValueType::Prefixed { prefix, value_type } => {
            if prefix.eq_ignore_ascii_case("Bone")
                && matches!(
                    value_type.as_ref(),
                    ValueType::AsciiString | ValueType::QuotedString | ValueType::W3dModelMember
                )
            {
                format!("{prefix}:${{{n}:NONE}}")
            } else if prefix.eq_ignore_ascii_case("RandomBone")
                && matches!(value_type.as_ref(), ValueType::Bool)
            {
                format!("{prefix}:${{{n}:No}}")
            } else if prefix.eq_ignore_ascii_case("Loc")
                && matches!(value_type.as_ref(), ValueType::AsciiString)
            {
                format!("{prefix}:X:${{{n}:0}}")
            } else {
                format!("{prefix}:{}", type_snippet_placeholder(value_type, n))
            }
        }
        ValueType::OneOf { variants } => variants
            .first()
            .map(|variant| type_snippet_placeholder(variant, n))
            .unwrap_or_else(|| format!("${{{n}:?}}")),
        ValueType::Bool => format!("${{{n}:Yes}}"),
        ValueType::Int | ValueType::UInt => format!("${{{n}:0}}"),
        ValueType::Real
        | ValueType::PositiveReal
        | ValueType::AngleReal
        | ValueType::Velocity
        | ValueType::Acceleration => format!("${{{n}:0}}"),
        ValueType::Percent => format!("${{{n}:100%}}"),
        ValueType::Duration => format!("${{{n}:1000}}"),
        ValueType::Enum { .. } | ValueType::BitFlags { .. } => format!("${{{n}:NONE}}"),
        ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
            format!("${{{n}:{ref_kind:?}}}")
        }
        ValueType::AsciiString | ValueType::AsciiStringList | ValueType::QuotedString => {
            format!("${{{n}:Value}}")
        }
        ValueType::AudioFile { .. } => format!("${{{n}:Sound.wav}}"),
        ValueType::AudioStemList => format!("${{{n}:Sound}}"),
        ValueType::TextureFile => format!("${{{n}:Texture.tga}}"),
        ValueType::TextureStem | ValueType::TextureSequenceStem => format!("${{{n}:Texture}}"),
        ValueType::W3dModel | ValueType::W3dModelList => format!("${{{n}:Model}}"),
        ValueType::W3dModelMember => format!("${{{n}:Bone}}"),
        _ => format!("${{{n}:?}}"),
    }
}

fn value_snippet(ty: &ValueType) -> Option<String> {
    match ty {
        ValueType::RandomVariable { .. } => Some("${1:0} ${2:0}$0".into()),
        ValueType::RandomKeyframe => Some("${1:0} ${2:0} ${3:0}$0".into()),
        ValueType::ColorKeyframe => Some("R:${1:0} G:${2:0} B:${3:0} ${4:0}$0".into()),
        ValueType::TokenList { tokens } if tokens.len() > 1 => {
            let mut snippet = tokens
                .iter()
                .enumerate()
                .map(|(i, t)| type_snippet_placeholder(t, i + 1))
                .collect::<Vec<_>>()
                .join(" ");
            snippet.push_str("$0");
            Some(snippet)
        }
        ValueType::OneOf { variants } => variants.first().and_then(value_snippet),
        _ => None,
    }
}

fn completions_for_type(
    analyzer: &Analyzer,
    ty: &ValueType,
    value_index: usize,
    current_token: Option<&str>,
    first_token: Option<&str>,
    index: Option<&WorkspaceIndex>,
) -> Vec<Completion> {
    match ty {
        ValueType::RandomVariable { value_set } if value_index == 2 => completions_for_type(
            analyzer,
            &ValueType::Enum {
                value_set: value_set.clone(),
            },
            0,
            current_token,
            first_token,
            index,
        ),
        ValueType::RandomVariable { .. } | ValueType::RandomKeyframe | ValueType::ColorKeyframe => {
            Vec::new()
        }
        ValueType::OneOf { variants } => {
            if current_token.is_some() || value_index > 0 {
                return ty
                    .variant_for_first_token(first_token.or(current_token))
                    .map(|variant| {
                        completions_for_type(
                            analyzer,
                            variant,
                            value_index,
                            current_token,
                            first_token,
                            index,
                        )
                    })
                    .unwrap_or_default();
            }
            variants
                .iter()
                .flat_map(|variant| {
                    completions_for_type(analyzer, variant, 0, None, first_token, index)
                })
                .collect()
        }
        ValueType::Prefixed { prefix, value_type } => {
            let prefix_already_typed = current_token
                .and_then(|t| t.split_once(':').map(|(actual, _)| actual))
                .is_some_and(|actual| actual.eq_ignore_ascii_case(prefix));
            completions_for_type(
                analyzer,
                value_type,
                value_index,
                current_token,
                first_token,
                index,
            )
            .into_iter()
            .map(|mut c| {
                if !prefix_already_typed {
                    c.insert = Some(format!(
                        "{prefix}:{}",
                        c.insert.unwrap_or_else(|| c.label.clone())
                    ));
                }
                c
            })
            .collect()
        }
        // Token lists: at position 0 offer a full-sequence snippet plus per-token completions.
        ValueType::TokenList { tokens } => {
            let mut out = ty
                .token_type_at(value_index)
                .map(|elem| {
                    completions_for_type(analyzer, elem, 0, current_token, first_token, index)
                })
                .unwrap_or_default();
            // At the first token, also inject a full-sequence snippet.
            if value_index == 0 {
                if let Some(snippet) = value_snippet(ty) {
                    out.insert(
                        0,
                        Completion {
                            label: "<full sequence>".into(),
                            kind: CompletionKind::Value,
                            detail: Some(format!("{} tokens", tokens.len())),
                            documentation: None,
                            insert: Some(snippet),
                        },
                    );
                }
            }
            out
        }
        // Structured positional types: offer a single snippet.
        ValueType::Color => vec![Completion {
            label: "R: G: B:".into(),
            kind: CompletionKind::Value,
            detail: Some("color".into()),
            documentation: None,
            insert: Some("R:${1:255} G:${2:255} B:${3:255}".into()),
        }],
        ValueType::Coord2D => vec![Completion {
            label: "X: Y:".into(),
            kind: CompletionKind::Value,
            detail: Some("2D coordinate".into()),
            documentation: None,
            insert: Some("X:${1:0} Y:${2:0}".into()),
        }],
        ValueType::Coord3D => vec![Completion {
            label: "X: Y: Z:".into(),
            kind: CompletionKind::Value,
            detail: Some("3D coordinate".into()),
            documentation: None,
            insert: Some("X:${1:0} Y:${2:0} Z:${3:0}".into()),
        }],
        ValueType::Bool => ["Yes", "No"]
            .iter()
            .map(|v| Completion {
                label: v.to_string(),
                kind: CompletionKind::Value,
                detail: None,
                documentation: None,
                insert: None,
            })
            .collect(),
        ValueType::Enum { value_set } | ValueType::BitFlags { value_set } => analyzer
            .value_set(value_set)
            .map(|set| {
                set.members
                    .iter()
                    .map(|m| Completion {
                        label: m.name.clone(),
                        kind: CompletionKind::EnumMember,
                        detail: Some(value_set.clone()),
                        documentation: None,
                        insert: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
            let mut out: Vec<Completion> = index
                .map(|idx| {
                    idx.names(*ref_kind)
                        .map(|n| Completion {
                            label: n.to_string(),
                            kind: CompletionKind::Reference,
                            detail: Some(format!("{ref_kind:?}")),
                            documentation: None,
                            insert: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Engine-synthesized names (e.g. Upgrade_Veterancy_*) are valid
            // targets that appear in no file.
            out.extend(analyzer.builtin_names(*ref_kind).map(|n| Completion {
                label: n.to_string(),
                kind: CompletionKind::Reference,
                detail: Some(format!("{ref_kind:?} (engine builtin)")),
                documentation: None,
                insert: None,
            }));
            out
        }
        ValueType::AudioFile { extension } => asset_completions(
            index,
            AssetKind::Audio,
            "audio file",
            |name| match extension {
                AudioExtension::Any => Some(name.to_string()),
                AudioExtension::Wav if has_extension(name, "wav") => Some(name.to_string()),
                AudioExtension::Mp3 if has_extension(name, "mp3") => Some(name.to_string()),
                _ => None,
            },
        ),
        ValueType::AudioStemList => {
            asset_completions(index, AssetKind::Audio, "sound stem", |name| {
                has_extension(name, "wav").then(|| file_stem(name).to_string())
            })
        }
        ValueType::TextureFile => asset_completions(index, AssetKind::Texture, "texture", |name| {
            Some(format!("{}.tga", file_stem(name)))
        }),
        ValueType::TextureStem => asset_completions(index, AssetKind::Texture, "texture", |name| {
            Some(file_stem(name).to_string())
        }),
        ValueType::TextureSequenceStem => {
            asset_completions(index, AssetKind::Texture, "texture", |name| {
                let stem = file_stem(name);
                if let Some(base) = stem.strip_suffix("0000") {
                    Some(base.to_string())
                } else if stem
                    .as_bytes()
                    .get(stem.len().saturating_sub(4)..)
                    .is_some_and(|suffix| {
                        suffix.len() == 4 && suffix.iter().all(u8::is_ascii_digit)
                    })
                {
                    None
                } else {
                    Some(stem.to_string())
                }
            })
        }
        ValueType::W3dModel | ValueType::W3dModelList | ValueType::W3dModelMember => Vec::new(),
        _ => Vec::new(),
    }
}

fn file_stem(name: &str) -> &str {
    name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name)
}

fn has_extension(name: &str, extension: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, actual)| actual.eq_ignore_ascii_case(extension))
}

fn asset_completions(
    index: Option<&WorkspaceIndex>,
    kind: AssetKind,
    detail: &str,
    label: impl Fn(&str) -> Option<String>,
) -> Vec<Completion> {
    let Some(index) = index.filter(|index| index.has_assets(kind)) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    index
        .asset_names(kind)
        .filter_map(label)
        .filter(|label| seen.insert(label.to_ascii_lowercase()))
        .map(|label| Completion {
            label,
            kind: CompletionKind::Reference,
            detail: Some(detail.to_string()),
            documentation: None,
            insert: None,
        })
        .collect()
}

fn top_level_completions(analyzer: &Analyzer) -> Vec<Completion> {
    analyzer
        .schema()
        .blocks
        .iter()
        .map(|b| {
            let insert = if !b.terminated {
                // Single-line directive — no End needed.
                None
            } else if b.named {
                Some(format!("{} ${{1:Name}}\n\t$0\nEnd", b.name))
            } else {
                Some(format!("{}\n\t$0\nEnd", b.name))
            };
            Completion {
                label: b.name.clone(),
                kind: CompletionKind::Block,
                detail: Some("block".into()),
                documentation: None,
                insert,
            }
        })
        .collect()
}

fn module_name_completions(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    slot_accepts: &[String],
) -> Vec<Completion> {
    let tag = next_module_tag(analyzer, scope_node);
    analyzer
        .schema()
        .modules
        .iter()
        .filter(|m| {
            slot_accepts.is_empty() || m.interfaces.iter().any(|i| slot_accepts.contains(i))
        })
        .map(|m| {
            // Snippet: module name + placeholder tag + indented body + End.
            // Also satisfies missing-module-tag in one accept.
            let insert = Some(format!("{} ${{1:{tag}}}\n\t$0\nEnd", m.name));
            Completion {
                label: m.name.clone(),
                kind: CompletionKind::Module,
                detail: Some("module".into()),
                documentation: None,
                insert,
            }
        })
        .collect()
}

/// Suggest the next numeric tag used by module slots in the enclosing Object.
///
/// Descriptive tags (such as `ModuleTag_Draw`) deliberately do not affect the
/// numeric sequence. Only genuine module slots are considered: sub-block
/// headers can also have several arguments, but those arguments are not tags.
fn next_module_tag(analyzer: &Analyzer, scope_node: &SyntaxNode) -> String {
    const PREFIX: &str = "ModuleTag_";
    let object_node = scope_node
        .ancestors()
        .find(|node| {
            Block(node.clone())
                .keyword()
                .is_some_and(|keyword| keyword.text().eq_ignore_ascii_case("Object"))
        })
        .unwrap_or_else(|| scope_node.clone());
    let highest = object_node
        .descendants()
        .filter_map(Module::cast)
        .filter(|module| {
            let parent = enclosing_scope(&module.0);
            let module_slots = parent
                .as_ref()
                .map(|parent| scope_schema(analyzer, parent).module_slots())
                .unwrap_or_default();
            module.slot().is_some_and(|slot| {
                module_slots
                    .iter()
                    .any(|module_slot| module_slot.keyword.eq_ignore_ascii_case(slot.text()))
            })
        })
        .filter_map(|module| module.tag())
        .filter_map(|tag| {
            let text = tag.text();
            text.get(..PREFIX.len())
                .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
                .and_then(|_| text.get(PREFIX.len()..))
                .and_then(|number| number.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0);
    format!("ModuleTag_{:02}", highest + 1)
}

// --- position helpers ---

fn ancestor_of_kind(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxNode> {
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if n.kind() == kind {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// The nearest BLOCK/MODULE ancestor of `node` (its enclosing scope).
fn enclosing_scope(node: &SyntaxNode) -> Option<SyntaxNode> {
    node.ancestors()
        .skip(1)
        .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
}

/// True if there is an `=` token before `offset` within `node`'s own tokens.
fn after_equals(node: &SyntaxNode, offset: u32) -> bool {
    for el in node.children_with_tokens() {
        if let Some(t) = el.as_token() {
            if t.kind() == SyntaxKind::EQUALS && u32::from(t.text_range().end()) <= offset {
                return true;
            }
        } else {
            break; // reached nested nodes; header is over
        }
    }
    false
}

/// True if `offset` lies on the header line of a scope.
///
/// The header ends at the first NEWLINE token among the node's direct
/// children_with_tokens. This correctly handles module nodes with empty bodies:
/// a cursor on the line after the header is NOT on the header, even when the
/// body has no child nodes yet.
fn on_header_line(node: &SyntaxNode, offset: u32) -> bool {
    for el in node.children_with_tokens() {
        if let Some(t) = el.as_token() {
            if t.kind() == SyntaxKind::NEWLINE {
                return offset <= u32::from(t.text_range().start());
            }
        } else {
            // First child node encountered before any NEWLINE; past the header.
            break;
        }
    }
    // No NEWLINE found: single-line node — entire node is header.
    true
}

fn type_label(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "Yes/No".into(),
        ValueType::Enum { value_set } => format!("enum {value_set}"),
        ValueType::BitFlags { value_set } => format!("flags {value_set}"),
        ValueType::Reference { ref_kind } => format!("ref {ref_kind:?}"),
        ValueType::W3dModel => "w3d model".into(),
        ValueType::W3dModelList => "w3d models".into(),
        ValueType::W3dModelMember => "w3d model member".into(),
        ValueType::RandomVariable { .. } => "real real [distribution]".into(),
        ValueType::RandomKeyframe => "real real frame".into(),
        ValueType::ColorKeyframe => "R: G: B: frame".into(),
        ValueType::Prefixed { prefix, value_type } => {
            format!("{prefix}:{}", type_label(value_type))
        }
        ValueType::OneOf { variants } => variants
            .iter()
            .map(type_label)
            .collect::<Vec<_>>()
            .join(" | "),
        other => format!("{other:?}")
            .split(['{', ' '])
            .next()
            .unwrap_or("")
            .to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(src: &str, offset: u32) -> Vec<String> {
        let a = Analyzer::embedded();
        complete(&a, &a.parse(src), offset, None, None)
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    fn item(src: &str, offset: u32, label: &str) -> Completion {
        let a = Analyzer::embedded();
        complete(&a, &a.parse(src), offset, None, None)
            .into_iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("missing completion `{label}`"))
    }

    fn item_with_defs(src: &str, offset: u32, defs: &str, label: &str) -> Completion {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file(
            "defs.ini",
            crate::index::definitions_in(&a, &a.parse(defs), "defs.ini"),
        );
        complete(&a, &a.parse(src), offset, Some(&index), None)
            .into_iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("missing completion `{label}`"))
    }

    #[test]
    fn top_level_suggests_block_keywords() {
        let out = labels("", 0);
        assert!(out.contains(&"Object".to_string()));
        assert!(out.contains(&"Weapon".to_string()));
    }

    #[test]
    fn inside_weapon_suggests_fields() {
        // Cursor on the indented blank line inside the block.
        let src = "Weapon AK47\n  \nEnd\n";
        let offset = "Weapon AK47\n  ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"PrimaryDamage".to_string()), "{out:?}");
        assert!(out.contains(&"ClipSize".to_string()));
    }

    #[test]
    fn bool_value_suggests_yes_no() {
        let src = "Weapon AK47\n  ScaleWeaponSpeed = \nEnd\n";
        let offset = "Weapon AK47\n  ScaleWeaponSpeed = ".len() as u32;
        let out = labels(src, offset);
        assert!(
            out.contains(&"Yes".to_string()) && out.contains(&"No".to_string()),
            "{out:?}"
        );
    }

    #[test]
    fn new_map_object_suggests_default_module_tags() {
        let a = Analyzer::embedded();
        let defaults = a.parse(
            "Object DefaultThingTemplate\n  Behavior = DestroyDie ModuleTag_DefaultDestroyDie\n  End\nEnd\n",
        );
        let mut index = WorkspaceIndex::new();
        index.set_file_tags(
            "data/INI/Default/Object.ini",
            crate::index::module_tags_in(&a, &defaults),
        );
        let src = "Object NewMapObject\n  RemoveModule \nEnd\n";
        let offset = "Object NewMapObject\n  RemoveModule ".len() as u32;
        let out = complete(&a, &a.parse(src), offset, Some(&index), Some("map.ini"));
        assert!(
            out.iter()
                .any(|item| item.label == "ModuleTag_DefaultDestroyDie"),
            "{out:?}"
        );
    }

    #[test]
    fn map_object_header_suggests_existing_objects_without_requiring_one() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        let base = a.parse("Object AmericaVehicleHumvee\nEnd\n");
        index.set_file(
            "data/INI/Object.ini",
            crate::index::definitions_in(&a, &base, "data/INI/Object.ini"),
        );
        let map_only = a.parse("Object MapOnlyObject\nEnd\n");
        index.set_file(
            "maps/other/map.ini",
            crate::index::definitions_in(&a, &map_only, "maps/other/map.ini"),
        );

        // `NewMapObject` intentionally does not exist in the index: an Object
        // header in map.ini may define a new template as well as override one.
        let src = "Object NewMapObject\nEnd\n";
        let offset = "Object New".len() as u32;
        let out = complete(
            &a,
            &a.parse(src),
            offset,
            Some(&index),
            Some("maps/map.ini"),
        );
        assert!(
            out.iter().any(|item| item.label == "AmericaVehicleHumvee"),
            "{out:?}"
        );
        assert!(
            !out.iter().any(|item| item.label == "MapOnlyObject"),
            "{out:?}"
        );

        let blank_header = "Object \nEnd\n";
        let blank_out = complete(
            &a,
            &a.parse(blank_header),
            "Object ".len() as u32,
            Some(&index),
            Some("maps/map.ini"),
        );
        assert!(
            blank_out
                .iter()
                .any(|item| item.label == "AmericaVehicleHumvee"),
            "{blank_out:?}"
        );

        // This is a completion-only affordance; typing a new name remains
        // valid and produces no unknown-reference diagnostic.
        assert!(crate::diagnostics::diagnose(
            &a,
            &a.parse(src),
            Some(&index),
            Some("maps/map.ini")
        )
        .iter()
        .all(|diagnostic| diagnostic.code != "unresolved-reference"));

        let non_map = complete(&a, &a.parse(src), offset, Some(&index), Some("Object.ini"));
        assert!(!non_map
            .iter()
            .any(|item| item.label == "AmericaVehicleHumvee"));
    }

    #[test]
    fn remove_module_completion_excludes_later_declarations() {
        let a = Analyzer::embedded();
        let src =
            "Object Tank\n  RemoveModule \n  Behavior = DestroyDie ModuleTag_Later\n  End\nEnd\n";
        let parse = a.parse(src);
        let mut index = WorkspaceIndex::new();
        index.set_file_tags("map.ini", crate::index::module_tags_in(&a, &parse));
        let offset = "Object Tank\n  RemoveModule ".len() as u32;
        let out = complete(&a, &parse, offset, Some(&index), Some("map.ini"));
        assert!(!out.iter().any(|item| item.label == "ModuleTag_Later"));
    }

    #[test]
    fn remove_module_completion_documents_the_defining_module() {
        let a = Analyzer::embedded();
        let defs =
            a.parse("Object Tank\n  Behavior = PhysicsBehavior ModuleTag_Physics\n  End\nEnd\n");
        let mut index = WorkspaceIndex::new();
        index.set_file_tags("base.ini", crate::index::module_tags_in(&a, &defs));
        let src = "Object Tank\n  RemoveModule \nEnd\n";
        let offset = "Object Tank\n  RemoveModule ".len() as u32;
        let item = complete(&a, &a.parse(src), offset, Some(&index), Some("map.ini"))
            .into_iter()
            .find(|item| item.label == "ModuleTag_Physics")
            .expect("module tag completion");
        assert_eq!(
            item.documentation.as_deref(),
            Some("```ini\nBehavior = PhysicsBehavior ModuleTag_Physics\n```")
        );
    }

    #[test]
    fn enum_value_suggests_members() {
        let src = "Weapon AK47\n  DeathType = \nEnd\n";
        let offset = "Weapon AK47\n  DeathType = ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"BURNED".to_string()), "{out:?}");
        assert!(out.contains(&"NORMAL".to_string()));
    }

    #[test]
    fn module_slot_value_suggests_modules() {
        let src = "Object Tank\n  Body = \n  End\nEnd\n";
        let offset = "Object Tank\n  Body = ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"ActiveBody".to_string()), "{out:?}");
    }

    #[test]
    fn module_snippet_uses_next_numeric_tag_in_object() {
        let src = "Object Tank\n  Draw = W3DTankDraw ModuleTag_01\n  End\n  Behavior = SlowDeathBehavior MODULETAG_03\n  End\n  Behavior = \nEnd\n";
        let offset = "Object Tank\n  Draw = W3DTankDraw ModuleTag_01\n  End\n  Behavior = SlowDeathBehavior MODULETAG_03\n  End\n  Behavior = ".len() as u32;
        let completion = item(src, offset, "AutoHealBehavior");
        assert_eq!(
            completion.insert.as_deref(),
            Some("AutoHealBehavior ${1:ModuleTag_04}\n\t$0\nEnd")
        );
    }

    #[test]
    fn module_snippet_ignores_descriptive_tags_and_sub_block_arguments() {
        let src = "Object Tank\n  Draw = W3DTankDraw ModuleTag_Draw\n    ConditionState = DAMAGED REALLYDAMAGED\n    End\n  End\n  Behavior = \nEnd\n";
        let offset = "Object Tank\n  Draw = W3DTankDraw ModuleTag_Draw\n    ConditionState = DAMAGED REALLYDAMAGED\n    End\n  End\n  Behavior = ".len() as u32;
        let completion = item(src, offset, "AutoHealBehavior");
        assert_eq!(
            completion.insert.as_deref(),
            Some("AutoHealBehavior ${1:ModuleTag_01}\n\t$0\nEnd")
        );
    }

    #[test]
    fn module_snippet_in_reentrant_scope_uses_object_wide_sequence() {
        let src = "Object Tank\n  AddModule\n    Behavior = SlowDeathBehavior ModuleTag_04\n    End\n    Behavior = \n  End\nEnd\n";
        let offset = "Object Tank\n  AddModule\n    Behavior = SlowDeathBehavior ModuleTag_04\n    End\n    Behavior = ".len() as u32;
        let completion = item(src, offset, "AutoHealBehavior");
        assert_eq!(
            completion.insert.as_deref(),
            Some("AutoHealBehavior ${1:ModuleTag_05}\n\t$0\nEnd")
        );
    }

    #[test]
    fn module_snippet_continues_past_u32_tag_values() {
        let src = "Object Tank\n  Behavior = SlowDeathBehavior ModuleTag_4294967295\n  End\n  Behavior = \nEnd\n";
        let offset =
            "Object Tank\n  Behavior = SlowDeathBehavior ModuleTag_4294967295\n  End\n  Behavior = "
                .len() as u32;
        let completion = item(src, offset, "AutoHealBehavior");
        assert_eq!(
            completion.insert.as_deref(),
            Some("AutoHealBehavior ${1:ModuleTag_4294967296}\n\t$0\nEnd")
        );
    }

    #[test]
    fn model_asset_completions_use_index() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "Good".into(),
                members: vec!["Cargo01".into(), "Tire01".into()],
            }],
        );
        let src = "\
Object Tank
  Draw = W3DTruckDraw ModuleTag_01
    DefaultConditionState
      Model = 
      HideSubObject = 
    End
  End
End
";
        let model_offset = src.find("Model = ").unwrap() + "Model = ".len();
        let out: Vec<_> = complete(&a, &a.parse(src), model_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Good".to_string()), "{out:?}");

        let src = src.replace("Model = ", "Model = Good");
        let bone_offset = src.find("HideSubObject = ").unwrap() + "HideSubObject = ".len();
        let out: Vec<_> = complete(&a, &a.parse(&src), bone_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Cargo".to_string()), "{out:?}");
        assert!(out.contains(&"Tire".to_string()), "{out:?}");
    }

    #[test]
    fn model_completions_inside_named_condition_state() {
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
      Model = 
    End
  End
End
";
        let offset = src
            .find("ConditionState = DAMAGED\n      Model = ")
            .unwrap()
            + "ConditionState = DAMAGED\n      Model = ".len();
        let out: Vec<_> = complete(&a, &a.parse(src), offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Good".to_string()), "{out:?}");
    }

    #[test]
    fn ocl_member_completions_use_transport_models() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "a10.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "A10".into(),
                members: vec!["WeaponA01".into()],
            }],
        );
        index.set_file_object_models(
            "objects.ini",
            vec![("AmericaJetA10Thunderbolt".into(), vec!["A10".into()])],
        );
        let src = "ObjectCreationList Strike\n  DeliverPayload\n    Transport = AmericaJetA10Thunderbolt\n    VisibleDropBoneBaseName = \n  End\nEnd\n";
        let offset =
            src.find("VisibleDropBoneBaseName = ").unwrap() + "VisibleDropBoneBaseName = ".len();
        let out = complete(&a, &a.parse(src), offset as u32, Some(&index), None)
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();
        assert!(out.contains(&"WeaponA".to_string()), "{out:?}");
    }

    #[test]
    fn ocl_model_list_completes_every_position() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "Good".into(),
                members: vec![],
            }],
        );
        let src =
            "ObjectCreationList Debris\n  CreateDebris\n    ModelNames = First \n  End\nEnd\n";
        let offset = src.find("First ").unwrap() + "First ".len();
        let out = complete(&a, &a.parse(src), offset as u32, Some(&index), None)
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();
        assert!(out.contains(&"Good".to_string()), "{out:?}");
    }

    #[test]
    fn weapon_bone_completions_use_token_positions() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                hierarchy: None,
                name: "Good".into(),
                members: vec!["Muzzle01".into(), "Muzzle02".into()],
            }],
        );
        let src = "\
Object Tank
  Draw = W3DTruckDraw ModuleTag_01
    DefaultConditionState
      Model = Good
      WeaponFireFXBone = 
    End
  End
End
";
        let slot_offset = src.find("WeaponFireFXBone = ").unwrap() + "WeaponFireFXBone = ".len();
        let out: Vec<_> = complete(&a, &a.parse(src), slot_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"PRIMARY".to_string()), "{out:?}");
        assert!(!out.contains(&"Muzzle".to_string()), "{out:?}");

        let src = src.replace("WeaponFireFXBone = ", "WeaponFireFXBone = PRIMARY ");
        let bone_offset = src.find("PRIMARY ").unwrap() + "PRIMARY ".len();
        let out: Vec<_> = complete(&a, &a.parse(&src), bone_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Muzzle".to_string()), "{out:?}");
        assert_eq!(
            out.iter()
                .filter(|label| label.as_str() == "Muzzle")
                .count(),
            1
        );
        assert!(!out.contains(&"PRIMARY".to_string()), "{out:?}");
    }

    #[test]
    fn transition_damage_particle_field_inserts_full_snippet() {
        let src = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    \n  End\nEnd\n";
        let offset = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    ".len() as u32;
        let got = item(src, offset, "DamagedParticleSystem1");
        assert_eq!(
            got.insert.as_deref(),
            Some(
                "DamagedParticleSystem1 = Bone:${1:NONE} RandomBone:${2:No} PSys:${3:ParticleSystem}$0"
            )
        );
    }

    #[test]
    fn transition_damage_fx_and_ocl_fields_insert_full_snippets() {
        let src = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    \n  End\nEnd\n";
        let offset = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    ".len() as u32;
        assert_eq!(
            item(src, offset, "DamagedFXList1").insert.as_deref(),
            Some("DamagedFXList1 = Bone:${1:NONE} RandomBone:${2:No} FXList:${3:FxList}$0")
        );
        assert_eq!(
            item(src, offset, "DamagedOCL10").insert.as_deref(),
            Some("DamagedOCL10 = Bone:${1:NONE} RandomBone:${2:No} OCL:${3:ObjectCreationList}$0")
        );
    }

    #[test]
    fn prefixed_particle_reference_suggests_while_typing() {
        let src = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    DamagedParticleSystem1 = Bone:NONE RandomBone:No PSys:Str\n  End\nEnd\n";
        let offset = src.find("PSys:Str").unwrap() + "PSys:Str".len();
        let got = item_with_defs(
            src,
            offset as u32,
            "ParticleSystem StructureTransitionMediumSmoke\nEnd\n",
            "StructureTransitionMediumSmoke",
        );
        assert_eq!(got.insert, None);
    }

    #[test]
    fn loc_variant_reference_suggests_while_typing() {
        let src = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    ReallyDamagedFXList1 = Loc: X:0 Y:0 Z:0 FXList: FX_\n  End\nEnd\n";
        let offset = src.find("FXList: FX_").unwrap() + "FXList: FX_".len();
        let got = item_with_defs(
            src,
            offset as u32,
            "FXList FX_TankDamageTransition\nEnd\n",
            "FX_TankDamageTransition",
        );
        assert_eq!(got.insert, None);
    }
}
