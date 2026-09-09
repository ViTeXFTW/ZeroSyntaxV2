//! Data model for the C&C Generals: Zero Hour INI schema.
//!
//! This crate defines the serde types that describe how the game engine
//! interprets INI files. The schema itself is the hand-written
//! `schema.json` (modeled on the engine's `FieldParse` tables in the bundled
//! C++ source), and is consumed by `analysis` (diagnostics, completions,
//! semantic tokens) and embedded into the language `server` binary.
//!
//! The schema is intentionally a flat, self-contained document: blocks and
//! modules reference value sets and each other by string id, so it round-trips
//! cleanly through JSON and needs no post-load fixups beyond building lookup
//! maps (see [`Schema::index`]).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The complete engine schema. Root of the hand-written embedded `schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    /// Schema format version (this crate's contract), bumped on breaking changes.
    pub format_version: u32,
    /// Identifier of the engine source snapshot this schema was extracted from
    /// (e.g. a git revision or tag). Lets the server report what it mirrors.
    #[serde(default)]
    pub engine_revision: String,
    /// Top-level block types, keyed by their INI keyword (e.g. `Object`, `Weapon`).
    pub blocks: Vec<BlockType>,
    /// Nested module types (e.g. `W3DModelDraw`, `ActiveBody`) that appear inside
    /// blocks under a [`ModuleSlot`].
    pub modules: Vec<ModuleType>,
    /// Named value sets used by enum / bitflag fields, keyed by id.
    pub value_sets: Vec<ValueSet>,
    /// Definitions the engine synthesizes at runtime rather than reading from
    /// INI (e.g. the `Upgrade_Veterancy_*` upgrades) — valid reference targets
    /// that exist in no file.
    #[serde(default)]
    pub builtins: Vec<BuiltinDef>,
}

/// An engine-synthesized definition (see [`Schema::builtins`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltinDef {
    pub ref_kind: RefKind,
    pub name: String,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A top-level INI block, e.g. `Object GLATankMarauder` ... `End`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockType {
    /// The INI keyword that introduces the block (`Object`, `Weapon`, ...).
    pub name: String,
    /// The C++ parse function this block dispatches to (provenance / debugging).
    #[serde(default)]
    pub parse_fn: String,
    /// Whether the block is named (`Object Foo`) or anonymous (`GameData`).
    #[serde(default = "default_true")]
    pub named: bool,
    /// Whether the block is `End`-terminated. A few typeTable entries are
    /// single-line directives (`BenchProfile = ...`, `ReallyLowMHz = 600`)
    /// whose parse functions consume only their own line.
    #[serde(default = "default_true")]
    pub terminated: bool,
    /// The kind of symbol a named block of this type defines, for the cross-file
    /// reference index. `None` means it is not referenceable by name.
    #[serde(default)]
    pub defines: Option<RefKind>,
    /// Fields legal directly inside this block.
    pub fields: Vec<Field>,
    /// Module slots this block exposes (e.g. `Object` exposes `Draw`, `Body`,
    /// `Behavior`, ...). Empty for leaf blocks.
    #[serde(default)]
    pub module_slots: Vec<ModuleSlot>,
    /// Structural sub-block scopes the engine parses recursively inside this
    /// block via custom parse functions (e.g. `FXList`'s `ParticleSystem`
    /// nugget). These open `End`-terminated scopes only inside their declaring
    /// scope — unlike module slots they carry no module name after `=`.
    #[serde(default)]
    pub sub_blocks: Vec<SubBlock>,
    /// Optional human-readable documentation.
    #[serde(default)]
    pub doc: Option<String>,
}

/// A structural sub-block scope (see [`BlockType::sub_blocks`]). Sub-blocks
/// nest: e.g. `AIData` → `SideInfo` → `SkillSet1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubBlock {
    /// The keyword opening the scope (`Sound`, `CreateObject`, `Mission`, ...).
    pub keyword: String,
    /// Fields legal inside this sub-block (empty = lenient, not yet modeled).
    #[serde(default)]
    pub fields: Vec<Field>,
    /// Sub-blocks nested inside this one.
    #[serde(default)]
    pub sub_blocks: Vec<SubBlock>,
    /// The scope re-enters the *parent's* field table: the engine parse
    /// function calls `ini->initFromINI(self, self->getFieldParse())` back on
    /// the enclosing template (e.g. `Object`'s `AddModule`/`ReplaceModule`/
    /// `InheritableModule`/`OverrideableByLikeKind`), so everything legal in
    /// the parent — fields, module slots, other sub-blocks — is legal here.
    /// When set, `fields`/`sub_blocks` need not be declared.
    #[serde(default)]
    pub reenters_parent: bool,
    /// Type of the header argument that follows `=` when opening this sub-block
    /// (e.g. `BitFlags { model_condition }` for ConditionState). `None` when
    /// the sub-block has no header argument or the type is not yet modeled.
    #[serde(default)]
    pub argument_type: Option<ValueType>,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A slot inside a block that hosts nested modules, e.g. `Behavior = <Module> <Tag>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleSlot {
    /// The field keyword introducing the slot (`Draw`, `Body`, `Behavior`, ...).
    pub keyword: String,
    /// The module interfaces that are accepted in this slot. A module is valid
    /// here when `module.interfaces` and `slot.accepts` share at least one entry.
    /// Driven by the engine's `findModuleInterfaceMask` / interface-mask checks
    /// in `ThingTemplate::parseModuleName`.
    pub accepts: Vec<String>,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A nested module definition, e.g. `Draw = W3DModelDraw ModuleTag_01` ... `End`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleType {
    /// Module name as written in INI (`W3DModelDraw`, `ActiveBody`, ...).
    pub name: String,
    /// Interfaces this module implements; must intersect a slot's `interface`.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Fields legal inside this module.
    pub fields: Vec<Field>,
    /// Structural sub-block scopes inside this module (e.g. `W3DModelDraw`'s
    /// `ConditionState`).
    #[serde(default)]
    pub sub_blocks: Vec<SubBlock>,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A single `Key = value [value...]` field within a block or module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    /// The field keyword (left-hand side of `=`).
    pub name: String,
    /// The value type, determining lexing/validation of the right-hand side.
    pub value_type: ValueType,
    /// The C++ parse function name (provenance / debugging).
    #[serde(default)]
    pub parse_fn: String,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub model_source: Option<ModelSource>,
    /// Engine lookup semantics for the model-member token in this field.
    /// Omitted for fields whose lookup has not yet been audited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_member_mode: Option<ModelMemberMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMemberMode {
    Exact,
    /// Enumerates base01, base02, ... starting at 01.
    Indexed,
    /// Enumerates numbered bones, with an unadorned-name fallback.
    ExactOrIndexed,
    /// TransitionDamageFX: selected by the field's RandomBone:Yes/No value.
    RandomBone,
}

/// Selects the Object whose W3D models own a model-member field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelSource {
    EnclosingObject,
    ObjectReferenceField { field: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioExtension {
    Any,
    Wav,
    Mp3,
}

/// The type of a field's value, derived from its engine parse function.
///
/// The variant determines how the value tokens are validated and which
/// completions/semantic tokens apply. `value_set` ids refer to entries in
/// [`Schema::value_sets`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueType {
    /// `Yes` / `No`.
    Bool,
    /// Signed integer.
    Int,
    /// Unsigned integer.
    UInt,
    /// Floating-point real.
    Real,
    /// Real constrained > 0.
    PositiveReal,
    /// Angle in degrees (engine converts to radians).
    AngleReal,
    /// Percentage literal (e.g. `50%`) mapped to a real.
    Percent,
    /// Duration in milliseconds (int or real ms).
    Duration,
    /// Velocity (dist/sec).
    Velocity,
    /// Acceleration (dist/sec^2).
    Acceleration,
    /// A bare (unquoted, single-token) ASCII string.
    AsciiString,
    /// A quoted ASCII string.
    QuotedString,
    /// A list of ASCII strings (variadic).
    AsciiStringList,
    /// A W3D model asset name, backed by indexed `.w3d` files.
    W3dModel,
    /// One or more W3D model asset names.
    W3dModelList,
    /// A bone, subobject, mesh, or other member of a W3D model asset.
    W3dModelMember,
    /// An indexed audio filename, optionally restricted by extension.
    AudioFile { extension: AudioExtension },
    /// A variadic list of extensionless indexed WAV names.
    AudioStemList,
    /// An indexed texture filename. DDS transparently aliases the same TGA stem.
    TextureFile,
    /// An extensionless indexed TGA/DDS texture name.
    TextureStem,
    /// A texture stem that may be backed by a numbered `0000` first frame.
    TextureSequenceStem,
    /// `R:r G:g B:b [A:a]` color.
    Color,
    /// `X:x Y:y` coordinate.
    Coord2D,
    /// `X:x Y:y Z:z` coordinate.
    Coord3D,
    /// Two real bounds followed by an optional distribution name.
    RandomVariable { value_set: String },
    /// Two real bounds followed by an unsigned frame number.
    RandomKeyframe,
    /// `R:r G:g B:b` followed by an unsigned frame number.
    ColorKeyframe,
    /// One name drawn from a value set (an enum).
    Enum { value_set: String },
    /// One or more flag names from a value set, with optional `+`/`-` modifiers
    /// and the special `NONE` / `ALL` tokens.
    BitFlags { value_set: String },
    /// A reference to a named definition elsewhere (cross-file index target).
    Reference { ref_kind: RefKind },
    /// One or more references of the same kind (e.g. `TriggeredBy` upgrade
    /// lists, `Science` vectors): every token resolves against the index.
    ReferenceList { ref_kind: RefKind },
    /// A fixed sequence of individually-typed tokens, for engine parse
    /// functions that consume several tokens in order (e.g. Armor
    /// coefficients: `Armor = <DamageType> <percent>`, or WeaponSet's
    /// `Weapon = <slot> <weapon name>`). Each listed token is required. A
    /// trailing BitFlags element consumes all remaining tokens.
    TokenList { tokens: Vec<ValueType> },
    /// A single token with a literal prefix and colon before the typed value
    /// (e.g. `PSys:<ParticleSystem>` or `RandomBone:<Yes|No>`).
    Prefixed {
        prefix: String,
        value_type: Box<ValueType>,
    },
    /// One of several value shapes accepted by the same parser.
    OneOf { variants: Vec<ValueType> },
    /// Anything we could not map precisely; treated leniently (token soup).
    /// Carries the originating parse-fn so it can be refined later.
    Unknown { parse_fn: String },
}

impl ValueType {
    pub fn split_prefix_value_type(&self, token: &str) -> Option<&ValueType> {
        let ValueType::Prefixed { prefix, value_type } = self else {
            return None;
        };
        token
            .trim_matches('"')
            .strip_suffix(':')
            .is_some_and(|actual| actual.eq_ignore_ascii_case(prefix))
            .then_some(value_type.as_ref())
    }

    pub fn first_prefix(&self) -> Option<&str> {
        match self {
            ValueType::Prefixed { prefix, .. } => Some(prefix),
            ValueType::TokenList { tokens } => tokens.first().and_then(ValueType::first_prefix),
            _ => None,
        }
    }

    pub fn variant_for_first_token(&self, first_token: Option<&str>) -> Option<&ValueType> {
        let ValueType::OneOf { variants } = self else {
            return Some(self);
        };
        let actual = first_token.and_then(|t| t.split_once(':').map(|(prefix, _)| prefix));
        actual
            .and_then(|actual| {
                variants.iter().find(|variant| {
                    variant
                        .first_prefix()
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(actual))
                })
            })
            .or_else(|| variants.first())
    }

    /// The value type consumed at a token position. A trailing bitflag type
    /// represents the variable-length mask accepted by `INI::parseBitString32`.
    pub fn token_type_at(&self, index: usize) -> Option<&ValueType> {
        let ValueType::TokenList { tokens } = self else {
            return Some(self);
        };
        tokens.get(index).or_else(|| {
            tokens
                .last()
                .filter(|ty| matches!(ty, ValueType::BitFlags { .. }))
        })
    }

    /// The value type at a raw syntax-token position, accounting for the
    /// engine-valid split form `Prefix: Value`.
    pub fn token_type_at_input(&self, input: &[&str], index: usize) -> Option<&ValueType> {
        let active = self.variant_for_first_token(input.first().copied())?;
        if !std::ptr::eq(active, self) {
            return active.token_type_at_input(input, index);
        }
        let ValueType::TokenList { tokens } = active else {
            if let Some(value_type) =
                active.split_prefix_value_type(input.first().copied().unwrap_or_default())
            {
                return match index {
                    0 => Some(active),
                    1 => Some(value_type),
                    _ => None,
                };
            }
            return match active {
                ValueType::BitFlags { .. }
                | ValueType::ReferenceList { .. }
                | ValueType::RandomVariable { .. }
                | ValueType::RandomKeyframe
                | ValueType::ColorKeyframe => Some(active),
                _ if index == 0 => Some(active),
                _ => None,
            };
        };
        let mut raw = 0;
        for ty in tokens {
            if let Some(value_type) =
                ty.split_prefix_value_type(input.get(raw).copied().unwrap_or_default())
            {
                if index == raw {
                    return Some(ty);
                }
                if index == raw + 1 {
                    return Some(value_type);
                }
                raw += 2;
            } else {
                if index == raw {
                    return Some(ty);
                }
                raw += 1;
            }
        }
        tokens
            .last()
            .filter(|ty| index >= raw && matches!(ty, ValueType::BitFlags { .. }))
    }

    /// The schema token position corresponding to a raw syntax-token position.
    /// A split `Prefix: Value` pair occupies one schema position.
    pub fn token_index_at_input(&self, input: &[&str], index: usize) -> Option<usize> {
        let active = self.variant_for_first_token(input.first().copied())?;
        if !std::ptr::eq(active, self) {
            return active.token_index_at_input(input, index);
        }
        let ValueType::TokenList { tokens } = active else {
            return match active {
                ValueType::RandomVariable { .. }
                | ValueType::RandomKeyframe
                | ValueType::ColorKeyframe => Some(index),
                _ => Some(0),
            };
        };
        let mut raw = 0;
        for (logical, ty) in tokens.iter().enumerate() {
            let width = if ty
                .split_prefix_value_type(input.get(raw).copied().unwrap_or_default())
                .is_some()
            {
                2
            } else {
                1
            };
            if index < raw + width {
                return Some(logical);
            }
            raw += width;
        }
        tokens
            .last()
            .filter(|ty| index >= raw && matches!(ty, ValueType::BitFlags { .. }))
            .map(|_| tokens.len() - 1)
    }
}

/// The kind of named definition a reference points at (or a block defines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Object,
    Weapon,
    Armor,
    DamageFx,
    FxList,
    ObjectCreationList,
    ParticleSystem,
    Locomotor,
    Upgrade,
    Science,
    CommandButton,
    CommandSet,
    SpecialPower,
    MappedImage,
    Anim2D,
    PlayerTemplate,
    EvaEvent,
    CrateData,
    /// A named audio event (`AudioEvent` / `DialogEvent` / `MusicTrack`).
    AudioEvent,
}

/// A named set of enum / bitflag members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueSet {
    /// Stable id referenced by [`ValueType::Enum`] / [`ValueType::BitFlags`].
    pub id: String,
    /// Member name → integer value (value is informational; names drive validation).
    pub members: Vec<ValueMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueMember {
    pub name: String,
    #[serde(default)]
    pub value: i64,
}

fn default_true() -> bool {
    true
}

/// The hand-written schema JSON, embedded at compile time. This is the single
/// artifact the language server ships with — it has no runtime dependency on
/// the engine C++ source.
pub const EMBEDDED_SCHEMA_JSON: &str = include_str!("../schema.json");

/// Parse the embedded schema. Panics only if the committed `schema.json` is
/// malformed, which a build-time test guards against.
pub fn embedded() -> Schema {
    Schema::from_json(EMBEDDED_SCHEMA_JSON).expect("embedded schema.json is valid")
}

/// Indexed views over a [`Schema`] for fast lookup by name. Built once after load.
pub struct SchemaIndex<'a> {
    pub schema: &'a Schema,
    pub blocks: HashMap<&'a str, &'a BlockType>,
    pub modules: HashMap<&'a str, &'a ModuleType>,
    pub value_sets: HashMap<&'a str, &'a ValueSet>,
}

impl Schema {
    /// Parse a schema from JSON text (e.g. the embedded `schema.json`).
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        serde_json::from_str(text)
    }

    /// Serialize the schema to pretty JSON (round-trip / tooling helper).
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Build name → item lookup maps for O(1) resolution.
    pub fn index(&self) -> SchemaIndex<'_> {
        SchemaIndex {
            schema: self,
            blocks: self.blocks.iter().map(|b| (b.name.as_str(), b)).collect(),
            modules: self.modules.iter().map(|m| (m.name.as_str(), m)).collect(),
            value_sets: self.value_sets.iter().map(|v| (v.id.as_str(), v)).collect(),
        }
    }
}

impl<'a> SchemaIndex<'a> {
    pub fn block(&self, name: &str) -> Option<&'a BlockType> {
        self.blocks.get(name).copied()
    }

    pub fn module(&self, name: &str) -> Option<&'a ModuleType> {
        self.modules.get(name).copied()
    }

    pub fn value_set(&self, id: &str) -> Option<&'a ValueSet> {
        self.value_sets.get(id).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use super::*;

    #[test]
    fn round_trips_through_json() {
        let schema = Schema {
            format_version: 1,
            engine_revision: "test".into(),
            blocks: vec![BlockType {
                name: "Weapon".into(),
                parse_fn: "parseWeaponTemplateDefinition".into(),
                named: true,
                terminated: true,
                defines: Some(RefKind::Weapon),
                fields: vec![Field {
                    name: "PrimaryDamage".into(),
                    value_type: ValueType::Real,
                    parse_fn: "parseReal".into(),
                    doc: None,
                    model_source: None,
                    model_member_mode: None,
                }],
                module_slots: vec![],
                sub_blocks: vec![],
                doc: None,
            }],
            modules: vec![],
            value_sets: vec![],
            builtins: vec![],
        };
        let json = schema.to_json_pretty().unwrap();
        let back = Schema::from_json(&json).unwrap();
        assert_eq!(back.blocks.len(), 1);
        let idx = back.index();
        assert_eq!(
            idx.block("Weapon").unwrap().fields[0].value_type,
            ValueType::Real
        );
    }

    /// The committed, embedded schema must parse and contain core blocks. This
    /// guards `embedded()` against a malformed or stale `schema.json`.
    #[test]
    fn embedded_schema_is_valid() {
        let schema = embedded();
        let idx = schema.index();
        assert!(idx.block("Object").is_some(), "Object block missing");
        assert!(idx.block("Weapon").is_some(), "Weapon block missing");
        assert!(
            schema.modules.iter().any(|m| m.name == "ActiveBody"),
            "ActiveBody module missing"
        );
    }

    #[test]
    fn split_prefixes_keep_raw_tokens_on_their_schema_positions() {
        let ty = token_list(vec![
            prefixed("Loc", ValueType::AsciiString),
            prefixed("Y", ValueType::Real),
            prefixed("Z", ValueType::Real),
            prefixed("FXList", reference(RefKind::FxList)),
        ]);
        let input = ["Loc:", "X:0", "Y:", "0", "Z:", "0", "FXList:", "Effect"];

        assert_eq!(
            (0..input.len())
                .map(|index| ty.token_index_at_input(&input, index).unwrap())
                .collect::<Vec<_>>(),
            [0, 0, 1, 1, 2, 2, 3, 3]
        );
        assert!(matches!(
            ty.token_type_at_input(&input, 7),
            Some(ValueType::Reference {
                ref_kind: RefKind::FxList
            })
        ));
    }

    #[test]
    fn structured_particle_values_type_every_raw_token() {
        for (ty, input) in [
            (ValueType::RandomKeyframe, vec!["0", "1", "2"]),
            (ValueType::ColorKeyframe, vec!["R:0", "G:0", "B:0", "2"]),
        ] {
            assert!((0..input.len()).all(|index| ty.token_type_at_input(&input, index).is_some()));
        }
    }

    fn contains(ty: &ValueType, predicate: fn(&ValueType) -> bool) -> bool {
        predicate(ty)
            || match ty {
                ValueType::TokenList { tokens } => tokens.iter().any(|ty| contains(ty, predicate)),
                ValueType::Prefixed { value_type, .. } => contains(value_type, predicate),
                ValueType::OneOf { variants } => variants.iter().any(|ty| contains(ty, predicate)),
                _ => false,
            }
    }

    fn visit_sub_block_fields(sub_blocks: &[SubBlock], visit: &mut impl FnMut(&Field)) {
        for sub_block in sub_blocks {
            sub_block.fields.iter().for_each(&mut *visit);
            visit_sub_block_fields(&sub_block.sub_blocks, visit);
        }
    }

    fn visit_sub_blocks(sub_blocks: &[SubBlock], visit: &mut impl FnMut(&SubBlock)) {
        for sub_block in sub_blocks {
            visit(sub_block);
            visit_sub_blocks(&sub_block.sub_blocks, visit);
        }
    }

    fn collect_fields<'a>(
        owner: &str,
        fields: &'a [Field],
        sub_blocks: &'a [SubBlock],
        out: &mut Vec<(String, &'a Field)>,
    ) {
        out.extend(
            fields
                .iter()
                .map(|field| (format!("{owner}.{}", field.name), field)),
        );
        for sub_block in sub_blocks {
            collect_fields(
                &format!("{owner}/{}", sub_block.keyword),
                &sub_block.fields,
                &sub_block.sub_blocks,
                out,
            );
        }
    }

    fn module_fields(schema: &Schema) -> Vec<(String, &Field)> {
        let mut out = Vec::new();
        for module in &schema.modules {
            collect_fields(&module.name, &module.fields, &module.sub_blocks, &mut out);
        }
        out
    }

    fn module_field<'a>(schema: &'a Schema, module: &str, field: &str) -> &'a Field {
        schema
            .index()
            .module(module)
            .unwrap_or_else(|| panic!("module {module} missing"))
            .fields
            .iter()
            .find(|candidate| candidate.name == field)
            .unwrap_or_else(|| panic!("{module}.{field} missing"))
    }

    fn sub_block<'a>(schema: &'a Schema, module: &str, keyword: &str) -> &'a SubBlock {
        schema
            .index()
            .module(module)
            .unwrap_or_else(|| panic!("module {module} missing"))
            .sub_blocks
            .iter()
            .find(|candidate| candidate.keyword == keyword)
            .unwrap_or_else(|| panic!("{module}/{keyword} missing"))
    }

    fn reference(ref_kind: RefKind) -> ValueType {
        ValueType::Reference { ref_kind }
    }

    fn reference_list(ref_kind: RefKind) -> ValueType {
        ValueType::ReferenceList { ref_kind }
    }

    fn enum_type(value_set: &str) -> ValueType {
        ValueType::Enum {
            value_set: value_set.into(),
        }
    }

    fn bit_flags(value_set: &str) -> ValueType {
        ValueType::BitFlags {
            value_set: value_set.into(),
        }
    }

    fn prefixed(prefix: &str, value_type: ValueType) -> ValueType {
        ValueType::Prefixed {
            prefix: prefix.into(),
            value_type: Box::new(value_type),
        }
    }

    fn token_list(tokens: Vec<ValueType>) -> ValueType {
        ValueType::TokenList { tokens }
    }

    fn assert_parser_contract(
        schema: &Schema,
        parse_fn: &str,
        expected_count: usize,
        expected_type: &ValueType,
    ) {
        let matches = module_fields(schema)
            .into_iter()
            .filter(|(_, field)| field.parse_fn == parse_fn)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), expected_count, "{parse_fn} occurrence count");
        for (path, field) in matches {
            assert_eq!(
                &field.value_type, expected_type,
                "{path} disagrees with {parse_fn}"
            );
        }
    }

    fn assert_unique(owner: &str, names: impl IntoIterator<Item = String>) {
        let mut seen = HashSet::new();
        for name in names {
            assert!(!name.trim().is_empty(), "{owner} contains an empty name");
            assert!(
                seen.insert(name.to_ascii_lowercase()),
                "duplicate {owner} entry `{name}`"
            );
        }
    }

    fn validate_type(
        path: &str,
        value_type: &ValueType,
        value_sets: &HashSet<&str>,
        definitions: &HashSet<RefKind>,
    ) {
        match value_type {
            ValueType::Enum { value_set }
            | ValueType::BitFlags { value_set }
            | ValueType::RandomVariable { value_set } => assert!(
                value_sets.contains(value_set.as_str()),
                "{path} uses missing value set `{value_set}`"
            ),
            ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => assert!(
                definitions.contains(ref_kind),
                "{path} references {ref_kind:?}, but no block defines it"
            ),
            ValueType::TokenList { tokens } => {
                assert!(!tokens.is_empty(), "{path} has an empty token list");
                for (index, token) in tokens.iter().enumerate() {
                    validate_type(&format!("{path}[{index}]"), token, value_sets, definitions);
                }
            }
            ValueType::Prefixed { prefix, value_type } => {
                assert!(!prefix.is_empty(), "{path} has an empty prefix");
                validate_type(path, value_type, value_sets, definitions);
            }
            ValueType::OneOf { variants } => {
                assert!(variants.len() > 1, "{path} has a pointless one_of");
                for (index, variant) in variants.iter().enumerate() {
                    validate_type(
                        &format!("{path}.variant[{index}]"),
                        variant,
                        value_sets,
                        definitions,
                    );
                }
            }
            ValueType::Unknown { parse_fn } => {
                assert!(
                    !parse_fn.is_empty(),
                    "{path} has an untraceable unknown type"
                )
            }
            _ => {}
        }
    }

    fn validate_scope(
        owner: &str,
        fields: &[Field],
        sub_blocks: &[SubBlock],
        value_sets: &HashSet<&str>,
        definitions: &HashSet<RefKind>,
    ) {
        assert_unique(owner, fields.iter().map(|field| field.name.clone()));
        assert_unique(
            &format!("{owner} sub-block"),
            sub_blocks.iter().map(|sub_block| sub_block.keyword.clone()),
        );
        for field in fields {
            let path = format!("{owner}.{}", field.name);
            assert!(!field.parse_fn.is_empty(), "{path} has no parse function");
            validate_type(&path, &field.value_type, value_sets, definitions);
        }
        for sub_block in sub_blocks {
            let path = format!("{owner}/{}", sub_block.keyword);
            if let Some(argument_type) = &sub_block.argument_type {
                validate_type(
                    &format!("{path} argument"),
                    argument_type,
                    value_sets,
                    definitions,
                );
            }
            validate_scope(
                &path,
                &sub_block.fields,
                &sub_block.sub_blocks,
                value_sets,
                definitions,
            );
        }
    }

    #[test]
    fn module_fields_have_concrete_value_types() {
        let schema = embedded();
        for module in &schema.modules {
            let mut check = |field: &Field| {
                assert!(
                    !contains(&field.value_type, |ty| matches!(
                        ty,
                        ValueType::Unknown { .. }
                    )),
                    "{}.{} still has an unknown value type",
                    module.name,
                    field.name
                );
            };
            module.fields.iter().for_each(&mut check);
            visit_sub_block_fields(&module.sub_blocks, &mut check);
            visit_sub_blocks(&module.sub_blocks, &mut |sub_block| {
                if let Some(argument_type) = &sub_block.argument_type {
                    assert!(
                        !contains(argument_type, |ty| matches!(ty, ValueType::Unknown { .. })),
                        "{}/{} argument still has an unknown value type",
                        module.name,
                        sub_block.keyword
                    );
                }
            });
        }
    }

    #[test]
    fn weapon_fields_have_concrete_value_types() {
        let schema = embedded();
        let weapon = schema
            .index()
            .block("Weapon")
            .expect("Weapon block missing");
        for field in &weapon.fields {
            assert!(
                !contains(&field.value_type, |ty| matches!(
                    ty,
                    ValueType::Unknown { .. }
                )),
                "Weapon.{} still has an unknown value type",
                field.name
            );
        }
    }

    #[test]
    fn object_backed_module_fields_are_object_references() {
        let schema = embedded();
        let expected = [
            ("AssistedTargetingUpdate", "LaserFromAssisted"),
            ("AssistedTargetingUpdate", "LaserToTarget"),
            ("BaikonurLaunchPower", "DetonationObject"),
            ("BattlePlanUpdate", "VisionObjectName"),
            ("ChinookAIUpdate", "RopeName"),
            ("CountermeasuresBehavior", "FlareTemplateName"),
            ("DeliverPayloadAIUpdate", "PutInContainer"),
            ("FlightDeckBehavior", "PayloadTemplate"),
            ("GarrisonContain", "InitialRoster"),
            ("GenerateMinefieldBehavior", "MineName"),
            ("GenerateMinefieldBehavior", "UpgradedMineName"),
            ("HelicopterSlowDeathBehavior", "BladeObjectName"),
            ("HelicopterSlowDeathBehavior", "FinalRubbleObject"),
            ("HelixContain", "InitialPayload"),
            ("HelixContain", "PayloadTemplateName"),
            ("HijackerUpdate", "ParachuteName"),
            ("InternetHackContain", "InitialPayload"),
            ("JetAIUpdate", "LockonCursor"),
            ("MobNexusContain", "InitialPayload"),
            ("OCLSpecialPower", "ReferenceObject"),
            ("OverlordContain", "InitialPayload"),
            ("OverlordContain", "PayloadTemplateName"),
            ("ParticleUplinkCannonUpdate", "ConnectorMediumLaserName"),
            ("ParticleUplinkCannonUpdate", "ConnectorIntenseLaserName"),
            ("ParticleUplinkCannonUpdate", "ParticleBeamLaserName"),
            ("ParticleUplinkCannonUpdate", "DamagePulseRemnantObjectName"),
            ("ProductionUpdate", "QuantityModifier"),
            ("RailedTransportContain", "InitialPayload"),
            ("RailroadBehavior", "CarriageTemplateName"),
            ("RebuildHoleBehavior", "WorkerObjectName"),
            ("RebuildHoleExposeDie", "HoleName"),
            ("ReplaceObjectUpgrade", "ReplaceObject"),
            ("RiderChangeContain", "InitialPayload"),
            ("RiderChangeContain", "Rider1"),
            ("RiderChangeContain", "Rider2"),
            ("RiderChangeContain", "Rider3"),
            ("RiderChangeContain", "Rider4"),
            ("RiderChangeContain", "Rider5"),
            ("RiderChangeContain", "Rider6"),
            ("RiderChangeContain", "Rider7"),
            ("RiderChangeContain", "Rider8"),
            ("SpawnBehavior", "SpawnTemplateName"),
            ("SpecialAbilityUpdate", "SpecialObject"),
            ("SpectreGunshipDeploymentUpdate", "GunshipTemplateName"),
            ("SpectreGunshipUpdate", "GattlingTemplateName"),
            ("ToppleUpdate", "StumpName"),
            ("TransportContain", "InitialPayload"),
            ("UnitCrateCollide", "UnitName"),
        ]
        .into_iter()
        .map(|(module, field)| format!("{module}.{field}"))
        .collect::<BTreeSet<_>>();
        let actual = module_fields(&schema)
            .into_iter()
            .filter(|(_, field)| {
                contains(&field.value_type, |ty| {
                    matches!(
                        ty,
                        ValueType::Reference {
                            ref_kind: RefKind::Object
                        } | ValueType::ReferenceList {
                            ref_kind: RefKind::Object
                        }
                    )
                })
            })
            .map(|(path, _)| path)
            .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn module_inventory_matches_engine_source() {
        let expected = include_str!("../../../scripts/engine_modules.txt")
            .trim()
            .split(',')
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let actual = embedded()
            .modules
            .into_iter()
            .map(|module| module.name)
            .collect::<BTreeSet<_>>();

        assert_eq!(actual.len(), 223);
        assert_eq!(actual, expected);
    }

    #[test]
    fn schema_links_and_names_are_internally_consistent() {
        let schema = embedded();
        assert_eq!(schema.format_version, 4);
        assert!(!schema.engine_revision.is_empty());
        assert_unique(
            "block",
            schema.blocks.iter().map(|block| block.name.clone()),
        );
        assert_unique(
            "module",
            schema.modules.iter().map(|module| module.name.clone()),
        );
        assert_unique(
            "value set",
            schema
                .value_sets
                .iter()
                .map(|value_set| value_set.id.clone()),
        );
        assert_unique(
            "builtin",
            schema
                .builtins
                .iter()
                .map(|builtin| format!("{:?}:{}", builtin.ref_kind, builtin.name)),
        );

        let value_sets = schema
            .value_sets
            .iter()
            .map(|value_set| value_set.id.as_str())
            .collect::<HashSet<_>>();
        let definitions = schema
            .blocks
            .iter()
            .filter_map(|block| block.defines)
            .chain(schema.builtins.iter().map(|builtin| builtin.ref_kind))
            .collect::<HashSet<_>>();

        for value_set in &schema.value_sets {
            assert!(!value_set.members.is_empty(), "{} is empty", value_set.id);
            assert_unique(
                &format!("{} member", value_set.id),
                value_set.members.iter().map(|member| member.name.clone()),
            );
        }
        for block in &schema.blocks {
            assert!(
                !block.parse_fn.is_empty(),
                "{} has no parse function",
                block.name
            );
            assert!(
                block.defines.is_none() || block.named,
                "anonymous block {} cannot define a symbol",
                block.name
            );
            validate_scope(
                &block.name,
                &block.fields,
                &block.sub_blocks,
                &value_sets,
                &definitions,
            );
        }
        for module in &schema.modules {
            assert!(
                !module.interfaces.is_empty(),
                "{} has no interface",
                module.name
            );
            assert_unique(
                &format!("{} interface", module.name),
                module.interfaces.iter().cloned(),
            );
            validate_scope(
                &module.name,
                &module.fields,
                &module.sub_blocks,
                &value_sets,
                &definitions,
            );
        }

        let module_interfaces = schema
            .modules
            .iter()
            .flat_map(|module| module.interfaces.iter().map(String::as_str))
            .collect::<HashSet<_>>();
        let accepted_interfaces = schema
            .blocks
            .iter()
            .flat_map(|block| &block.module_slots)
            .flat_map(|slot| slot.accepts.iter().map(String::as_str))
            .collect::<HashSet<_>>();
        assert_eq!(module_interfaces, accepted_interfaces);
        for block in &schema.blocks {
            assert_unique(
                &format!("{} module slot", block.name),
                block.module_slots.iter().map(|slot| slot.keyword.clone()),
            );
            for slot in &block.module_slots {
                assert!(
                    !slot.accepts.is_empty(),
                    "{}.{} accepts nothing",
                    block.name,
                    slot.keyword
                );
                assert!(
                    slot.accepts
                        .iter()
                        .all(|interface| module_interfaces.contains(interface.as_str())),
                    "{}.{} accepts an unknown interface",
                    block.name,
                    slot.keyword
                );
            }
        }
    }

    #[test]
    fn common_module_parsers_keep_their_engine_types() {
        let schema = embedded();
        let contracts = [
            ("INI::parseAccelerationReal", 1, ValueType::Acceleration),
            ("INI::parseAngleReal", 356, ValueType::AngleReal),
            ("INI::parseAngularVelocityReal", 74, ValueType::AngleReal),
            ("INI::parseBool", 606, ValueType::Bool),
            ("INI::parseColorInt", 7, ValueType::Color),
            ("INI::parseCoord3D", 9, ValueType::Coord3D),
            ("INI::parseDurationReal", 31, ValueType::Duration),
            ("INI::parseDurationUnsignedInt", 379, ValueType::Duration),
            ("INI::parseInt", 94, ValueType::Int),
            ("INI::parsePercentToReal", 64, ValueType::Percent),
            ("INI::parsePositiveNonZeroReal", 12, ValueType::PositiveReal),
            ("INI::parseReal", 359, ValueType::Real),
            ("INI::parseRGBColor", 3, ValueType::Color),
            ("INI::parseUnsignedInt", 27, ValueType::UInt),
            ("INI::parseVelocityReal", 39, ValueType::Velocity),
        ];

        for (parse_fn, count, value_type) in contracts {
            assert_parser_contract(&schema, parse_fn, count, &value_type);
        }
        for (parse_fn, count, value_type) in [
            (
                "INI::parseAudioEventRTS",
                67,
                reference(RefKind::AudioEvent),
            ),
            ("INI::parseFXList", 89, reference(RefKind::FxList)),
            (
                "INI::parseObjectCreationList",
                20,
                reference(RefKind::ObjectCreationList),
            ),
            (
                "INI::parseParticleSystemTemplate",
                31,
                reference(RefKind::ParticleSystem),
            ),
            ("INI::parseScience", 19, reference(RefKind::Science)),
            (
                "INI::parseSpecialPowerTemplate",
                18,
                reference(RefKind::SpecialPower),
            ),
            ("INI::parseWeaponTemplate", 22, reference(RefKind::Weapon)),
        ] {
            assert_parser_contract(&schema, parse_fn, count, &value_type);
        }
        let ascii_vectors = module_fields(&schema)
            .into_iter()
            .filter(|(_, field)| field.parse_fn == "INI::parseAsciiStringVector")
            .map(|(_, field)| field.value_type.clone())
            .collect::<Vec<_>>();
        assert_eq!(ascii_vectors.len(), 91);
        assert_eq!(
            ascii_vectors
                .iter()
                .filter(|value_type| **value_type == reference_list(RefKind::Upgrade))
                .count(),
            84
        );
        assert_eq!(
            ascii_vectors
                .iter()
                .filter(|value_type| **value_type == ValueType::AsciiStringList)
                .count(),
            7
        );
        let appended_ascii_vectors = module_fields(&schema)
            .into_iter()
            .filter(|(_, field)| field.parse_fn == "INI::parseAsciiStringVectorAppend")
            .map(|(_, field)| field.value_type.clone())
            .collect::<Vec<_>>();
        assert_eq!(appended_ascii_vectors.len(), 17);
        assert_eq!(
            appended_ascii_vectors
                .iter()
                .filter(|value_type| **value_type == reference_list(RefKind::Object))
                .count(),
            4
        );
        assert_eq!(
            appended_ascii_vectors
                .iter()
                .filter(|value_type| **value_type == ValueType::AsciiStringList)
                .count(),
            13
        );
    }

    #[test]
    fn custom_module_parser_inventory_is_explicit() {
        let expected = [
            "BoneFXUpdateModuleData::parseFXList",
            "BoneFXUpdateModuleData::parseObjectCreationList",
            "BoneFXUpdateModuleData::parseParticleSystem",
            "CreateCrateDieModuleData::parseCrateData",
            "Eva::parseEvaMessageFromIni",
            "parseAngleFX",
            "parseAnimation",
            "parseAppendQuantityModifier",
            "parseAsciiStringLC",
            "parseBoneNameKey",
            "parseBountyUpgradePair",
            "parseCashHackUpgradePair",
            "parseFactionObjectCreationList",
            "parseFrictionPerSec",
            "parseFX",
            "parseHeightToSpeed",
            "parseInitialPayload",
            "parseInitialRoster",
            "parseLowercaseNameKey",
            "parseOCL",
            "parseOCLUpgradePair",
            "parseParticleSysBone",
            "parseRealRange",
            "parseRiderInfo",
            "parseRunwayStrip",
            "parseShowHideSubObject",
            "parseTWS",
            "parseUpgradePair",
            "parseWeapon",
            "parseWeaponBoneName",
            "TransitionDamageFXModuleData::parseFXList",
            "TransitionDamageFXModuleData::parseObjectCreationList",
            "TransitionDamageFXModuleData::parseParticleSystem",
            "TurretAIData::parseTurretSweep",
            "TurretAIData::parseTurretSweepSpeed",
            "W3DModelDrawModuleData::parseConditionState",
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let schema = embedded();
        let actual = module_fields(&schema)
            .into_iter()
            .map(|(_, field)| field.parse_fn.as_str())
            .filter(|parse_fn| {
                !parse_fn.starts_with("INI::")
                    && !parse_fn.ends_with("::parseFromINI")
                    && !parse_fn.ends_with("::parseSingleBitFromINI")
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn structured_custom_parsers_match_engine_token_order() {
        let schema = embedded();
        assert_parser_contract(
            &schema,
            "parseWeaponBoneName",
            165,
            &token_list(vec![enum_type("weapon_slot"), ValueType::W3dModelMember]),
        );
        assert_parser_contract(
            &schema,
            "parseParticleSysBone",
            33,
            &token_list(vec![
                ValueType::W3dModelMember,
                reference(RefKind::ParticleSystem),
            ]),
        );
        assert_parser_contract(
            &schema,
            "parseRealRange",
            33,
            &token_list(vec![ValueType::Real, ValueType::Real]),
        );
        assert_parser_contract(
            &schema,
            "TurretAIData::parseTurretSweep",
            30,
            &token_list(vec![enum_type("weapon_slot"), ValueType::AngleReal]),
        );
        assert_parser_contract(
            &schema,
            "TurretAIData::parseTurretSweepSpeed",
            30,
            &token_list(vec![enum_type("weapon_slot"), ValueType::Real]),
        );
        assert_parser_contract(&schema, "parseTWS", 30, &bit_flags("weapon_slot"));
        assert_parser_contract(
            &schema,
            "parseInitialPayload",
            7,
            &token_list(vec![reference(RefKind::Object), ValueType::Int]),
        );
        assert_parser_contract(
            &schema,
            "parseInitialRoster",
            1,
            &token_list(vec![reference(RefKind::Object), ValueType::Int]),
        );
        assert_parser_contract(
            &schema,
            "parseAppendQuantityModifier",
            1,
            &token_list(vec![reference(RefKind::Object), ValueType::Int]),
        );
        assert_parser_contract(
            &schema,
            "parseRiderInfo",
            8,
            &token_list(vec![
                reference(RefKind::Object),
                enum_type("model_condition"),
                enum_type("weapon_set_conditions"),
                enum_type("object_status"),
                reference(RefKind::CommandSet),
                enum_type("locomotor_set"),
            ]),
        );
        assert_parser_contract(&schema, "parseRunwayStrip", 4, &ValueType::AsciiStringList);
        assert_parser_contract(&schema, "parseFrictionPerSec", 8, &ValueType::Real);
        assert_parser_contract(&schema, "parseHeightToSpeed", 2, &ValueType::Real);
        assert_parser_contract(
            &schema,
            "Eva::parseEvaMessageFromIni",
            2,
            &reference(RefKind::EvaEvent),
        );
        assert_parser_contract(
            &schema,
            "CreateCrateDieModuleData::parseCrateData",
            1,
            &reference(RefKind::CrateData),
        );
        assert_parser_contract(
            &schema,
            "parseAngleFX",
            1,
            &token_list(vec![ValueType::AngleReal, reference(RefKind::FxList)]),
        );
        assert_parser_contract(
            &schema,
            "parseUpgradePair",
            2,
            &token_list(vec![
                prefixed("UpgradeType", reference(RefKind::Upgrade)),
                prefixed("Boost", ValueType::Int),
            ]),
        );
        assert_parser_contract(
            &schema,
            "parseCashHackUpgradePair",
            1,
            &token_list(vec![reference(RefKind::Science), ValueType::Int]),
        );
        assert_parser_contract(
            &schema,
            "parseBountyUpgradePair",
            1,
            &token_list(vec![reference(RefKind::Science), ValueType::Percent]),
        );
        assert_parser_contract(
            &schema,
            "parseOCLUpgradePair",
            1,
            &token_list(vec![
                reference(RefKind::Science),
                reference(RefKind::ObjectCreationList),
            ]),
        );
        assert_parser_contract(
            &schema,
            "parseFactionObjectCreationList",
            1,
            &token_list(vec![
                // Compares against `PlayerTemplate::getSide()` (e.g. `America`),
                // not a PlayerTemplate object name (`FactionAmerica`) — see
                // OCLUpdate.cpp.
                prefixed("Faction", enum_type("ai_side")),
                prefixed("OCL", reference(RefKind::ObjectCreationList)),
            ]),
        );
    }

    #[test]
    fn inherited_draw_damage_and_death_tables_keep_their_shapes() {
        let schema = embedded();
        assert_parser_contract(&schema, "parseBoneNameKey", 132, &ValueType::W3dModelMember);
        assert_parser_contract(
            &schema,
            "parseShowHideSubObject",
            66,
            &ValueType::W3dModelMember,
        );
        assert_parser_contract(&schema, "parseAnimation", 66, &ValueType::AsciiString);
        assert_parser_contract(
            &schema,
            "parseLowercaseNameKey",
            66,
            &ValueType::AsciiString,
        );

        for (parse_fn, prefix, ref_kind) in [
            (
                "BoneFXUpdateModuleData::parseFXList",
                "FXList",
                RefKind::FxList,
            ),
            (
                "BoneFXUpdateModuleData::parseObjectCreationList",
                "OCL",
                RefKind::ObjectCreationList,
            ),
            (
                "BoneFXUpdateModuleData::parseParticleSystem",
                "PSys",
                RefKind::ParticleSystem,
            ),
        ] {
            assert_parser_contract(
                &schema,
                parse_fn,
                32,
                &token_list(vec![
                    prefixed("Bone", ValueType::W3dModelMember),
                    prefixed("OnlyOnce", ValueType::Bool),
                    ValueType::Duration,
                    ValueType::Duration,
                    prefixed(prefix, reference(ref_kind)),
                ]),
            );
        }

        for (parse_fn, prefix, ref_kind) in [
            (
                "TransitionDamageFXModuleData::parseFXList",
                "FXList",
                RefKind::FxList,
            ),
            (
                "TransitionDamageFXModuleData::parseObjectCreationList",
                "OCL",
                RefKind::ObjectCreationList,
            ),
            (
                "TransitionDamageFXModuleData::parseParticleSystem",
                "PSys",
                RefKind::ParticleSystem,
            ),
        ] {
            assert_parser_contract(
                &schema,
                parse_fn,
                36,
                &ValueType::OneOf {
                    variants: vec![
                        token_list(vec![
                            prefixed("Bone", ValueType::W3dModelMember),
                            prefixed("RandomBone", ValueType::Bool),
                            prefixed(prefix, reference(ref_kind)),
                        ]),
                        token_list(vec![
                            prefixed("Loc", ValueType::AsciiString),
                            prefixed("Y", ValueType::Real),
                            prefixed("Z", ValueType::Real),
                            prefixed(prefix, reference(ref_kind)),
                        ]),
                    ],
                },
            );
        }

        let feedback_slots = module_fields(&schema)
            .into_iter()
            .filter(|(_, field)| field.name == "ProjectileBoneFeedbackEnabledSlots")
            .collect::<Vec<_>>();
        assert_eq!(feedback_slots.len(), 11);
        assert!(feedback_slots
            .iter()
            .all(|(_, field)| field.value_type == bit_flags("weapon_slot")));

        let slow_death_modules = [
            "BattleBusSlowDeathBehavior",
            "HelicopterSlowDeathBehavior",
            "JetSlowDeathBehavior",
            "NeutronMissileSlowDeathBehavior",
            "SlowDeathBehavior",
        ];
        for module in slow_death_modules {
            for (field, ref_kind) in [
                ("FX", RefKind::FxList),
                ("OCL", RefKind::ObjectCreationList),
                ("Weapon", RefKind::Weapon),
            ] {
                assert_eq!(
                    module_field(&schema, module, field).value_type,
                    token_list(vec![
                        enum_type("slow_death_phase"),
                        reference_list(ref_kind),
                    ]),
                    "{module}.{field}"
                );
            }
        }
        for (field, ref_kind) in [
            ("FX", RefKind::FxList),
            ("OCL", RefKind::ObjectCreationList),
            ("Weapon", RefKind::Weapon),
        ] {
            assert_eq!(
                module_field(&schema, "InstantDeathBehavior", field).value_type,
                reference_list(ref_kind),
                "InstantDeathBehavior.{field}"
            );
        }
    }

    #[test]
    fn source_structures_and_disabled_rows_stay_pinned() {
        let schema = embedded();

        for (module, field) in [
            ("BoneFXDamage", "DamageTypes"),
            ("TransitionDamageFX", "DamageTypes"),
            ("EMPUpdate", "SpinRateMax"),
            ("ParkingPlaceBehavior", "ExtraHealAmount4Helicopters"),
            ("ParkingPlaceBehavior", "TimeForFullHeal"),
            ("RadiusDecalUpdate", "DeliveryDecal"),
            ("RadiusDecalUpdate", "DeliveryDecalRadius"),
        ] {
            assert!(
                schema
                    .index()
                    .module(module)
                    .unwrap()
                    .fields
                    .iter()
                    .all(|candidate| candidate.name != field),
                "commented-out engine row {module}.{field} leaked into the schema"
            );
        }
        assert_eq!(
            module_field(&schema, "HealContain", "TimeForFullHeal").value_type,
            ValueType::Duration
        );
        assert_eq!(
            module_field(&schema, "FireWeaponWhenDamagedBehavior", "DamageTypes").value_type,
            bit_flags("damage_type")
        );

        let turret_modules = schema
            .modules
            .iter()
            .filter(|module| {
                module
                    .sub_blocks
                    .iter()
                    .any(|sub_block| sub_block.keyword == "Turret")
            })
            .collect::<Vec<_>>();
        assert_eq!(turret_modules.len(), 15);
        for module in turret_modules {
            assert!(module.fields.iter().all(|field| field.name != "Turret"));
            assert!(module.fields.iter().all(|field| field.name != "AltTurret"));
            assert!(module
                .sub_blocks
                .iter()
                .any(|sub_block| sub_block.keyword == "AltTurret"));
        }

        let decal_fields = [
            ("Texture", ValueType::TextureStem),
            ("Style", bit_flags("shadow_type")),
            ("OpacityMin", ValueType::Percent),
            ("OpacityMax", ValueType::Percent),
            ("OpacityThrobTime", ValueType::Duration),
            ("Color", ValueType::Color),
            ("OnlyVisibleToOwningPlayer", ValueType::Bool),
        ];
        for (module, keyword) in [
            ("DeliverPayloadAIUpdate", "DeliveryDecal"),
            ("DynamicShroudClearingRangeUpdate", "GridDecalTemplate"),
            ("NeutronMissileUpdate", "DeliveryDecal"),
            ("SpectreGunshipUpdate", "AttackAreaDecal"),
            ("SpectreGunshipUpdate", "TargetingReticleDecal"),
        ] {
            let actual = sub_block(&schema, module, keyword)
                .fields
                .iter()
                .map(|field| (field.name.as_str(), field.value_type.clone()))
                .collect::<Vec<_>>();
            assert_eq!(actual, decal_fields, "{module}/{keyword}");
        }
        let radius = schema.index().module("RadiusDecalUpdate").unwrap();
        assert!(radius.fields.is_empty());
        assert!(radius.sub_blocks.is_empty());

        let cursor_blocks = &schema.index().block("InGameUI").unwrap().sub_blocks;
        assert_eq!(cursor_blocks.len(), 29);
        assert!(cursor_blocks.iter().all(|cursor| cursor
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.value_type.clone()))
            .eq(decal_fields.iter().cloned())));

        let ai_data = schema.index().block("AIData").unwrap();
        let build_list = ai_data
            .sub_blocks
            .iter()
            .find(|sub_block| sub_block.keyword == "SkirmishBuildList")
            .unwrap();
        let structure = build_list
            .sub_blocks
            .iter()
            .find(|sub_block| sub_block.keyword == "Structure")
            .unwrap();
        assert_eq!(structure.argument_type, Some(reference(RefKind::Object)));

        assert_eq!(
            schema.index().block("EvaEvent").unwrap().defines,
            Some(RefKind::EvaEvent)
        );
        let eva_side_sounds = schema
            .index()
            .block("EvaEvent")
            .unwrap()
            .sub_blocks
            .iter()
            .find(|sub_block| sub_block.keyword == "SideSounds")
            .unwrap();
        assert_eq!(
            eva_side_sounds
                .fields
                .iter()
                .map(|field| (field.name.as_str(), field.value_type.clone()))
                .collect::<Vec<_>>(),
            [
                ("Side", ValueType::AsciiString),
                ("Sounds", ValueType::AudioStemList),
            ]
        );
        for module in [
            "W3DDependencyModelDraw",
            "W3DModelDraw",
            "W3DOverlordAircraftDraw",
            "W3DOverlordTankDraw",
            "W3DOverlordTruckDraw",
            "W3DPoliceCarDraw",
            "W3DScienceModelDraw",
            "W3DSupplyDraw",
            "W3DTankDraw",
            "W3DTankTruckDraw",
            "W3DTruckDraw",
        ] {
            assert_eq!(
                module_field(&schema, module, "TrackMarks").value_type,
                ValueType::TextureFile,
                "{module}.TrackMarks"
            );
        }
        assert_eq!(
            schema.index().block("CrateData").unwrap().defines,
            Some(RefKind::CrateData)
        );
    }

    #[test]
    fn source_enum_sets_keep_their_exact_members() {
        let schema = embedded();
        for (id, expected) in [
            (
                "disabled_type",
                &[
                    "DEFAULT",
                    "DISABLED_HACKED",
                    "DISABLED_EMP",
                    "DISABLED_HELD",
                    "DISABLED_PARALYZED",
                    "DISABLED_UNMANNED",
                    "DISABLED_UNDERPOWERED",
                    "DISABLED_FREEFALL",
                    "DISABLED_AWESTRUCK",
                    "DISABLED_BRAINWASHED",
                    "DISABLED_SUBDUED",
                    "DISABLED_SCRIPT_DISABLED",
                    "DISABLED_SCRIPT_UNDERPOWERED",
                ][..],
            ),
            ("relationship", &["ENEMIES", "NEUTRAL", "ALLIES"]),
            (
                "weapon_affects",
                &[
                    "SELF",
                    "ALLIES",
                    "ENEMIES",
                    "NEUTRALS",
                    "SUICIDE",
                    "NOT_SIMILAR",
                    "NOT_AIRBORNE",
                ],
            ),
        ] {
            let actual = schema
                .index()
                .value_set(id)
                .unwrap()
                .members
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{id}");
        }
    }

    #[test]
    fn ascii_backed_engine_names_keep_their_semantic_types() {
        let schema = embedded();
        for (module, field, value_type) in [
            ("ArmorUpgrade", "FXListUpgrade", reference(RefKind::FxList)),
            (
                "BunkerBusterBehavior",
                "UpgradeRequired",
                reference(RefKind::Upgrade),
            ),
            (
                "PropagandaTowerBehavior",
                "UpgradeRequired",
                reference(RefKind::Upgrade),
            ),
            (
                "CommandSetUpgrade",
                "CommandSet",
                reference(RefKind::CommandSet),
            ),
            (
                "CommandSetUpgrade",
                "CommandSetAlt",
                reference(RefKind::CommandSet),
            ),
            (
                "CommandSetUpgrade",
                "TriggerAlt",
                reference(RefKind::Upgrade),
            ),
            (
                "FlammableUpdate",
                "BurningSoundName",
                reference(RefKind::AudioEvent),
            ),
            (
                "GrantScienceUpgrade",
                "GrantScience",
                reference(RefKind::Science),
            ),
            (
                "GrantUpgradeCreate",
                "UpgradeToGrant",
                reference(RefKind::Upgrade),
            ),
            (
                "StructureToppleUpdate",
                "CrushingWeaponName",
                reference(RefKind::Weapon),
            ),
            ("UpgradeDie", "UpgradeToRemove", reference(RefKind::Upgrade)),
            (
                "ChinookAIUpdate",
                "RotorWashParticleSystem",
                reference(RefKind::ParticleSystem),
            ),
            (
                "LaserUpdate",
                "MuzzleParticleSystem",
                reference(RefKind::ParticleSystem),
            ),
            (
                "LaserUpdate",
                "TargetParticleSystem",
                reference(RefKind::ParticleSystem),
            ),
            (
                "SlavedUpdate",
                "RepairWeldingSys",
                reference(RefKind::ParticleSystem),
            ),
            ("W3DPropDraw", "ModelName", ValueType::W3dModel),
            ("W3DTreeDraw", "ModelName", ValueType::W3dModel),
            ("W3DTreeDraw", "StumpName", ValueType::W3dModel),
        ] {
            assert_eq!(
                module_field(&schema, module, field).value_type,
                value_type,
                "{module}.{field}"
            );
        }

        let battle_plan_audio = [
            "BombardmentPlanUnpackSoundName",
            "BombardmentPlanPackSoundName",
            "BombardmentAnnouncementName",
            "SearchAndDestroyPlanUnpackSoundName",
            "SearchAndDestroyPlanIdleLoopSoundName",
            "SearchAndDestroyPlanPackSoundName",
            "SearchAndDestroyAnnouncementName",
            "HoldTheLinePlanUnpackSoundName",
            "HoldTheLinePlanPackSoundName",
            "HoldTheLineAnnouncementName",
        ];
        for field in battle_plan_audio {
            assert_eq!(
                module_field(&schema, "BattlePlanUpdate", field).value_type,
                reference(RefKind::AudioEvent),
                "BattlePlanUpdate.{field}"
            );
        }

        let animations = module_fields(&schema)
            .into_iter()
            .filter(|(_, field)| field.name == "ExecuteAnimation")
            .collect::<Vec<_>>();
        assert_eq!(animations.len(), 16);
        assert!(animations
            .iter()
            .all(|(_, field)| field.value_type == reference(RefKind::Anim2D)));

        for module in [
            "W3DOverlordTruckDraw",
            "W3DPoliceCarDraw",
            "W3DTankTruckDraw",
            "W3DTruckDraw",
        ] {
            for field in ["Dust", "DirtSpray", "PowerslideSpray"] {
                assert_eq!(
                    module_field(&schema, module, field).value_type,
                    reference(RefKind::ParticleSystem),
                    "{module}.{field}"
                );
            }
        }
        for module in ["W3DOverlordTankDraw", "W3DTankDraw", "W3DTankTruckDraw"] {
            for field in ["TreadDebrisLeft", "TreadDebrisRight"] {
                assert_eq!(
                    module_field(&schema, module, field).value_type,
                    reference(RefKind::ParticleSystem),
                    "{module}.{field}"
                );
            }
        }
    }
}
