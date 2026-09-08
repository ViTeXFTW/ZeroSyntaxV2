//! Bridges the syntax tree to the schema: given a scope node (a `BLOCK` or
//! `MODULE`), determine which schema entity it is and look up fields / slots.

use zerosyntax_schema::{
    BlockType, Field, ModelSource, ModuleSlot, ModuleType, SubBlock, ValueType,
};
use zerosyntax_syntax::ast::{Block as AstBlock, Field as AstField};

/// Returns true when `module` implements at least one of the interfaces the
/// `slot` accepts. Used to filter completions and validate module placement.
pub fn module_fits_slot(module: &ModuleType, slot: &ModuleSlot) -> bool {
    module.interfaces.iter().any(|i| slot.accepts.contains(i))
}
use zerosyntax_syntax::ast::{Block, Module};
use zerosyntax_syntax::{SyntaxKind, SyntaxNode};

use crate::Analyzer;

/// What schema entity a scope node corresponds to.
pub enum ScopeSchema<'a> {
    /// A top-level block with a known schema.
    Block(&'a BlockType),
    /// A nested module with a known schema.
    Module(&'a ModuleType),
    /// A structural sub-block declared by the enclosing scope (e.g. `Object`'s
    /// `WeaponSet`, `FXList`'s `Tracer` nugget).
    SubBlock(&'a SubBlock),
    /// A scope we have no schema for (unknown block/module, or an anonymous
    /// sub-block such as `DefaultConditionState`). Validation is lenient here:
    /// fields are not flagged as unknown.
    Unknown,
}

impl<'a> ScopeSchema<'a> {
    /// The declared fields of this scope, or empty when unknown.
    pub fn fields(&self) -> &'a [Field] {
        match self {
            ScopeSchema::Block(b) => &b.fields,
            ScopeSchema::Module(m) => &m.fields,
            ScopeSchema::SubBlock(s) => &s.fields,
            ScopeSchema::Unknown => &[],
        }
    }

    /// Look up a field by name.
    pub fn field(&self, name: &str) -> Option<&'a Field> {
        self.fields().iter().find(|f| f.name == name)
    }

    /// Module slots exposed by this scope (only blocks have them in the schema).
    pub fn module_slots(&self) -> &'a [ModuleSlot] {
        match self {
            ScopeSchema::Block(b) => &b.module_slots,
            _ => &[],
        }
    }

    /// Structural sub-blocks declared by this scope.
    pub fn sub_blocks(&self) -> &'a [SubBlock] {
        match self {
            ScopeSchema::Block(b) => &b.sub_blocks,
            ScopeSchema::Module(m) => &m.sub_blocks,
            ScopeSchema::SubBlock(s) => &s.sub_blocks,
            ScopeSchema::Unknown => &[],
        }
    }

    /// Whether we have a field schema at all (drives unknown-field diagnostics).
    pub fn has_field_schema(&self) -> bool {
        !matches!(self, ScopeSchema::Unknown) && !self.fields().is_empty()
    }

    /// A human label for diagnostics, e.g. `block Weapon` / `module ActiveBody`.
    pub fn label(&self) -> String {
        match self {
            ScopeSchema::Block(b) => format!("block `{}`", b.name),
            ScopeSchema::Module(m) => format!("module `{}`", m.name),
            ScopeSchema::SubBlock(s) => format!("`{}`", s.keyword),
            ScopeSchema::Unknown => "this block".to_string(),
        }
    }
}

/// Resolve the schema for a `BLOCK` or `MODULE` node.
///
/// MODULE nodes resolve first by module name (`Body = ActiveBody ...`), then —
/// for nameless / sub-block scopes — by looking the head keyword up in the
/// enclosing scope's declared `sub_blocks` (recursively through the parents,
/// mirroring how the `OpenerOracle` decided to open the scope).
pub fn scope_schema<'a>(analyzer: &'a Analyzer, node: &SyntaxNode) -> ScopeSchema<'a> {
    match node.kind() {
        SyntaxKind::BLOCK => {
            let kw = Block(node.clone()).keyword().map(|t| t.text().to_string());
            match kw.and_then(|k| analyzer.block(&k)) {
                Some(b) => ScopeSchema::Block(b),
                None => ScopeSchema::Unknown,
            }
        }
        SyntaxKind::MODULE => {
            let module = Module(node.clone());
            if let Some(m) = module.module_name().and_then(|n| analyzer.module(n.text())) {
                return ScopeSchema::Module(m);
            }
            let Some(slot) = module.slot() else {
                return ScopeSchema::Unknown;
            };
            let Some(parent) = node
                .ancestors()
                .skip(1)
                .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
            else {
                return ScopeSchema::Unknown;
            };
            let parent_schema = scope_schema(analyzer, &parent);
            match parent_schema
                .sub_blocks()
                .iter()
                .find(|s| s.keyword == slot.text())
            {
                // A reentrant scope (`AddModule`, `InheritableModule`, ...)
                // re-enters the parent's field table: validate and complete
                // it exactly as the parent scope.
                Some(sb) if sb.reenters_parent => parent_schema,
                Some(sb) => ScopeSchema::SubBlock(sb),
                None => ScopeSchema::Unknown,
            }
        }
        _ => ScopeSchema::Unknown,
    }
}

/// The chain of scope schemas enclosing `node`, innermost first. Used by
/// completion to know what is valid at the cursor.
pub fn enclosing_scopes<'a>(analyzer: &'a Analyzer, node: &SyntaxNode) -> Vec<ScopeSchema<'a>> {
    let mut out = Vec::new();
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE) {
            out.push(scope_schema(analyzer, &n));
        }
        cur = n.parent();
    }
    out
}

pub(crate) fn is_model_asset_type(ty: &ValueType) -> bool {
    matches!(ty, ValueType::W3dModel | ValueType::W3dModelList)
}

pub(crate) fn is_model_member_type(ty: &ValueType) -> bool {
    matches!(ty, ValueType::W3dModelMember)
}

pub(crate) fn model_member_ini_name(member: &str) -> &str {
    let member = member.rsplit('.').next().unwrap_or(member);
    let trimmed = member.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.is_empty() {
        member
    } else {
        trimmed
    }
}

/// Resolve a contextual mode once, using the entire field (including tokens
/// after the cursor). Colon-separated values may occupy one or two tokens.
pub(crate) fn model_member_mode(
    mode: Option<zerosyntax_schema::ModelMemberMode>,
    field: Option<&AstField>,
) -> Option<zerosyntax_schema::ModelMemberMode> {
    use zerosyntax_schema::ModelMemberMode;
    if mode != Some(ModelMemberMode::RandomBone) {
        return mode;
    }
    let tokens = field.map(AstField::value_tokens).unwrap_or_default();
    for (i, token) in tokens.iter().enumerate() {
        let Some((key, value)) = token.text().split_once(':') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("RandomBone") {
            continue;
        }
        let value = if value.is_empty() {
            tokens.get(i + 1).map(|t| t.text()).unwrap_or("")
        } else {
            value
        };
        if value.eq_ignore_ascii_case("Yes") {
            return Some(ModelMemberMode::Indexed);
        }
        if value.eq_ignore_ascii_case("No") {
            return Some(ModelMemberMode::Exact);
        }
    }
    // While the controlling value is incomplete, offer both legal forms.
    Some(ModelMemberMode::ExactOrIndexed)
}

pub(crate) fn model_member_names(
    member: &str,
    mode: Option<zerosyntax_schema::ModelMemberMode>,
) -> Vec<&str> {
    use zerosyntax_schema::ModelMemberMode;
    let short = member.rsplit('.').next().unwrap_or(member);
    let base = short.strip_suffix("01").filter(|base| !base.is_empty());
    match mode {
        None => vec![model_member_ini_name(short)],
        Some(ModelMemberMode::Exact) => vec![short],
        Some(ModelMemberMode::Indexed) => base.into_iter().collect(),
        Some(ModelMemberMode::ExactOrIndexed | ModelMemberMode::RandomBone) => {
            std::iter::once(short).chain(base).collect()
        }
    }
}

pub(crate) fn model_member_matches(
    member: &str,
    value: &str,
    mode: Option<zerosyntax_schema::ModelMemberMode>,
) -> bool {
    (mode.is_none() && member.eq_ignore_ascii_case(value))
        || model_member_names(member, mode)
            .iter()
            .any(|name| name.eq_ignore_ascii_case(value))
}

/// Models referenced by W3D-model-typed fields visible from `scope_node`:
/// the scope's own subtree first, then — because a `ConditionState` inherits
/// its model from `DefaultConditionState` when it sets none (see the engine's
/// `W3DModelDraw`) — each enclosing scope's subtree, stopping at the nearest
/// one that declares any model. The climb never crosses the top-level block,
/// so per-root cached diagnostics stay independent of sibling blocks.
pub(crate) fn models_in_scope(analyzer: &Analyzer, scope_node: &SyntaxNode) -> Vec<String> {
    let mut out = Vec::new();
    let mut node = scope_node.clone();
    loop {
        collect_models(analyzer, &node, &mut out);
        if !out.is_empty() {
            return out;
        }
        match node
            .parent()
            .filter(|p| matches!(p.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
        {
            Some(parent) => node = parent,
            None => return out,
        }
    }
}

pub(crate) fn models_for_source(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    source: Option<&ModelSource>,
    index: &crate::WorkspaceIndex,
) -> Vec<String> {
    match source {
        None | Some(ModelSource::EnclosingObject) => {
            let local = models_in_scope(analyzer, scope_node);
            if !local.is_empty() {
                return local;
            }
            scope_node
                .ancestors()
                .find_map(|node| {
                    let block = AstBlock(node);
                    block
                        .keyword()
                        .filter(|keyword| keyword.text().eq_ignore_ascii_case("Object"))?;
                    block.name().map(|name| name.text().to_string())
                })
                .map(|object| {
                    index
                        .models_for_object(&object)
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        }
        Some(ModelSource::ObjectReferenceField { field }) => {
            let Some(object) = scope_node.children().find_map(|child| {
                let ini = AstField(child);
                ini.key()
                    .filter(|key| key.text().eq_ignore_ascii_case(field))?;
                ini.value_tokens()
                    .first()
                    .map(|value| value.text().trim_matches('"').to_string())
            }) else {
                return Vec::new();
            };
            index
                .models_for_object(&object)
                .into_iter()
                .map(str::to_string)
                .collect()
        }
    }
}

fn collect_models(analyzer: &Analyzer, node: &SyntaxNode, out: &mut Vec<String>) {
    let scope = scope_schema(analyzer, node);
    for child in node.children() {
        match child.kind() {
            SyntaxKind::FIELD => {
                let field = AstField(child);
                let Some(schema_field) = field.key().and_then(|key| scope.field(key.text())) else {
                    continue;
                };
                let values = field.value_tokens();
                let input = values
                    .iter()
                    .map(|value| value.text().trim_matches('"'))
                    .collect::<Vec<_>>();
                match &schema_field.value_type {
                    ValueType::W3dModelList => out.extend(
                        values
                            .iter()
                            .map(|value| value.text().trim_matches('"').to_string()),
                    ),
                    ValueType::TokenList { .. }
                    | ValueType::OneOf { .. }
                    | ValueType::Prefixed { .. } => {
                        for (index, value) in values.iter().enumerate() {
                            let Some(spec) =
                                schema_field.value_type.token_type_at_input(&input, index)
                            else {
                                continue;
                            };
                            if is_model_asset_type(spec) {
                                out.push(value.text().trim_matches('"').to_string());
                            }
                        }
                    }
                    ty if is_model_asset_type(ty) => {
                        if let Some(value) = values.first() {
                            out.push(value.text().trim_matches('"').to_string());
                        }
                    }
                    _ => {}
                }
            }
            SyntaxKind::MODULE | SyntaxKind::BLOCK => collect_models(analyzer, &child, out),
            _ => {}
        }
    }
}
