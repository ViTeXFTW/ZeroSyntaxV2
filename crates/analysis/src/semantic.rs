//! Semantic tokens: classify each token for highlighting, using the schema so
//! enum members, references, numbers, etc. are coloured by meaning rather than
//! by lexical shape. The server maps [`SemKind`] to LSP semantic token types.

use std::collections::BTreeMap;

use zerosyntax_schema::ValueType;
use zerosyntax_syntax::ast::{Block, Field, Module};
use zerosyntax_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::model::{scope_schema, ScopeSchema};
use crate::{Analyzer, Span};

/// Semantic classification of a token. Names mirror the project's documented
/// mapping to standard LSP `SemanticTokenType`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemKind {
    /// Block keyword (`Object`, `Weapon`, `End`) and module-slot keywords.
    Keyword,
    /// A block/definition name (`AK47`).
    BlockName,
    /// A field key (left of `=`).
    Field,
    /// A module type name (`W3DModelDraw`).
    Module,
    /// An enum / bitflag member, or a boolean literal.
    EnumMember,
    /// `=` and bitflag `+`/`-` operators.
    Operator,
    /// A numeric literal.
    Number,
    /// A quoted or asset string.
    StringLit,
    /// A reference to another named definition.
    Reference,
    /// A comment.
    Comment,
}

/// A classified token over a byte span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemToken {
    pub span: Span,
    pub kind: SemKind,
}

/// Produce semantic tokens for the whole document, sorted by start offset and
/// non-overlapping (one classification per token).
pub fn semantic_tokens(analyzer: &Analyzer, parse: &Parse) -> Vec<SemToken> {
    semantic_tokens_range(analyzer, parse, Span::new(0, u32::MAX))
}

/// Produce semantic tokens for the top-level children intersecting `span`
/// (the LSP `semanticTokens/range` viewport path). Works block-granular, so
/// the result may extend past `span` — the LSP allows returning a superset.
pub fn semantic_tokens_range(analyzer: &Analyzer, parse: &Parse, span: Span) -> Vec<SemToken> {
    let mut sem = Sem {
        analyzer,
        by_start: BTreeMap::new(),
    };
    let root = parse.syntax();
    let intersects = |node: &SyntaxNode| {
        let r = node.text_range();
        u32::from(r.start()) <= span.end && u32::from(r.end()) >= span.start
    };

    // Structured pass: headers and field values, schema-aware.
    for node in root.children().filter(|n| intersects(n)) {
        match node.kind() {
            SyntaxKind::BLOCK => {
                let schema = scope_schema(analyzer, &node);
                sem.block_header(&node);
                sem.walk(&node, &schema);
            }
            SyntaxKind::FIELD => sem.field(&Field(node.clone()), &ScopeSchema::Unknown),
            _ => {}
        }
    }

    // Lexical pass: comments, `=`, `End`, and any uncovered strings — over the
    // intersecting children plus ROOT's own trivia tokens in range.
    let mut lexical = |tok: &SyntaxToken| match tok.kind() {
        SyntaxKind::COMMENT => sem.set(tok, SemKind::Comment),
        SyntaxKind::EQUALS => sem.set(tok, SemKind::Operator),
        SyntaxKind::WORD if tok.text().eq_ignore_ascii_case("End") => {
            sem.set(tok, SemKind::Keyword)
        }
        SyntaxKind::STRING => sem.set_if_absent(tok, SemKind::StringLit),
        _ => {}
    };
    for el in root.children_with_tokens() {
        match el {
            rowan::NodeOrToken::Node(node) if intersects(&node) => {
                for inner in node.descendants_with_tokens() {
                    if let Some(tok) = inner.as_token() {
                        lexical(tok);
                    }
                }
            }
            rowan::NodeOrToken::Token(tok) => {
                let r = tok.text_range();
                if u32::from(r.start()) <= span.end && u32::from(r.end()) >= span.start {
                    lexical(&tok);
                }
            }
            _ => {}
        }
    }

    sem.by_start.into_values().collect()
}

struct Sem<'a> {
    analyzer: &'a Analyzer,
    by_start: BTreeMap<u32, SemToken>,
}

impl<'a> Sem<'a> {
    fn block_header(&mut self, node: &SyntaxNode) {
        let block = Block(node.clone());
        if let Some(kw) = block.keyword() {
            self.set(&kw, SemKind::Keyword);
        }
        if let Some(name) = block.name() {
            self.set(&name, SemKind::BlockName);
        }
    }

    fn module_header(&mut self, node: &SyntaxNode, is_real_module: bool) {
        let module = Module(node.clone());
        if let Some(slot) = module.slot() {
            self.set(&slot, SemKind::Keyword);
        }
        if let Some(name) = module.module_name() {
            // Real module: the name is a module type; sub-block: it's an arg.
            self.set(
                &name,
                if is_real_module {
                    SemKind::Module
                } else {
                    SemKind::EnumMember
                },
            );
        }
        if is_real_module {
            if let Some(tag) = module.tag() {
                self.set(&tag, SemKind::Reference);
            }
        }
    }

    fn walk(&mut self, node: &SyntaxNode, scope: &ScopeSchema) {
        for child in node.children() {
            match child.kind() {
                SyntaxKind::FIELD => self.field(&Field(child.clone()), scope),
                SyntaxKind::MODULE => {
                    let module = Module(child.clone());
                    let is_real = module
                        .slot()
                        .map(|s| scope.module_slots().iter().any(|ms| ms.keyword == s.text()))
                        .unwrap_or(false);
                    self.module_header(&child, is_real);
                    // Sub-blocks (`WeaponSet`, FX nuggets, ...) resolve to
                    // their declared field schema too.
                    let inner = scope_schema(self.analyzer, &child);
                    self.walk(&child, &inner);
                }
                SyntaxKind::BLOCK => {
                    let inner = scope_schema(self.analyzer, &child);
                    self.block_header(&child);
                    self.walk(&child, &inner);
                }
                _ => {}
            }
        }
    }

    fn field(&mut self, field: &Field, scope: &ScopeSchema) {
        let is_remove_module = field
            .key()
            .is_some_and(|key| key.text().eq_ignore_ascii_case("RemoveModule"));
        if let Some(key) = field.key() {
            self.set(
                &key,
                if is_remove_module {
                    SemKind::Keyword
                } else {
                    SemKind::Field
                },
            );
        }
        let schema_field = field.key().and_then(|k| scope.field(k.text()));
        let is_animation = schema_field.is_some_and(|f| f.parse_fn == "parseAnimation");
        let ty = schema_field.map(|f| &f.value_type);
        let value_tokens = field.value_tokens();
        let active_ty = ty.and_then(|ty| {
            ty.variant_for_first_token(value_tokens.first().map(|t| t.text().trim_matches('"')))
        });
        let input = value_tokens
            .iter()
            .map(|token| token.text().trim_matches('"'))
            .collect::<Vec<_>>();
        for (i, tok) in value_tokens.iter().enumerate() {
            if is_remove_module {
                self.set(tok, SemKind::Reference);
                continue;
            }
            // parseAnimation accepts a name, optional distance, and optional
            // repeat count. Its lenient string schema must not hide those
            // meanings, including when the animation name is quoted.
            if is_animation && i < 3 {
                self.set(
                    tok,
                    if i == 0 {
                        SemKind::Reference
                    } else {
                        SemKind::Number
                    },
                );
                continue;
            }
            if matches!(active_ty, Some(ValueType::RandomVariable { .. })) {
                self.set(
                    tok,
                    if i == 2 {
                        SemKind::EnumMember
                    } else {
                        SemKind::Number
                    },
                );
                continue;
            }
            // Coordinates are a single schema value expressed as several raw
            // syntax tokens (`X:… Y:… [Z:…]`). Highlight every axis alike.
            if matches!(active_ty, Some(ValueType::Coord2D | ValueType::Coord3D)) {
                self.set(tok, SemKind::Number);
                continue;
            }
            // Token lists classify each position by its own element type.
            let elem = active_ty.and_then(|ty| ty.token_type_at_input(&input, i));
            self.set(tok, value_token_kind(tok, elem));
        }
    }

    fn set(&mut self, tok: &SyntaxToken, kind: SemKind) {
        let span: Span = tok.text_range().into();
        self.by_start.insert(span.start, SemToken { span, kind });
    }

    fn set_if_absent(&mut self, tok: &SyntaxToken, kind: SemKind) {
        let span: Span = tok.text_range().into();
        self.by_start
            .entry(span.start)
            .or_insert(SemToken { span, kind });
    }
}

/// Classify a field value token according to the field's value type.
fn value_token_kind(tok: &SyntaxToken, ty: Option<&ValueType>) -> SemKind {
    if tok.kind() == SyntaxKind::STRING {
        return SemKind::StringLit;
    }
    match ty {
        Some(ValueType::Bool) | Some(ValueType::Enum { .. }) | Some(ValueType::BitFlags { .. }) => {
            SemKind::EnumMember
        }
        Some(ValueType::OneOf { variants }) => variants
            .first()
            .map(|variant| value_token_kind(tok, Some(variant)))
            .unwrap_or(SemKind::StringLit),
        Some(ValueType::Prefixed { value_type, .. }) => value_token_kind(tok, Some(value_type)),
        Some(ValueType::Int)
        | Some(ValueType::UInt)
        | Some(ValueType::Real)
        | Some(ValueType::PositiveReal)
        | Some(ValueType::AngleReal)
        | Some(ValueType::Percent)
        | Some(ValueType::Duration)
        | Some(ValueType::Velocity)
        | Some(ValueType::Acceleration)
        | Some(ValueType::Color)
        | Some(ValueType::RandomVariable { .. })
        | Some(ValueType::RandomKeyframe)
        | Some(ValueType::ColorKeyframe)
        | Some(ValueType::Coord2D)
        | Some(ValueType::Coord3D) => SemKind::Number,
        Some(ValueType::Reference { .. })
        | Some(ValueType::ReferenceList { .. })
        | Some(ValueType::W3dModel)
        | Some(ValueType::W3dModelList)
        | Some(ValueType::W3dModelMember)
        | Some(ValueType::AudioFile { .. })
        | Some(ValueType::AudioStemList)
        | Some(ValueType::TextureFile)
        | Some(ValueType::TextureStem)
        | Some(ValueType::TextureSequenceStem) => SemKind::Reference,
        _ => SemKind::StringLit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<(SemKind, String)> {
        let a = Analyzer::embedded();
        let parse = a.parse(src);
        semantic_tokens(&a, &parse)
            .into_iter()
            .map(|t| {
                let s = &src[t.span.start as usize..t.span.end as usize];
                (t.kind, s.to_string())
            })
            .collect()
    }

    #[test]
    fn animations_are_references_with_numeric_arguments() {
        let src = "Object Soldier\n Draw = W3DModelDraw Tag\n  DefaultConditionState\n   Model = SoldierSkin\n   Animation = HumanSKL.Run 16\n   IdleAnimation = \"HumanSKL.Idle\" 0 9\n  End\n  ConditionState = MOVING\n   Animation = HumanSKL.Walk\n  End\n  TransitionState = TRANS_A TRANS_B\n   Animation = HumanSKL.Stop\n  End\n End\n DisplayName = PlainString\nEnd\n";
        let tokens = toks(src);
        for name in [
            "HumanSKL.Run",
            "\"HumanSKL.Idle\"",
            "HumanSKL.Walk",
            "HumanSKL.Stop",
        ] {
            assert!(
                tokens.contains(&(SemKind::Reference, name.into())),
                "expected animation reference for {name}: {tokens:?}"
            );
        }
        for number in ["16", "0", "9"] {
            assert!(
                tokens.contains(&(SemKind::Number, number.into())),
                "expected numeric animation argument {number}"
            );
        }
        assert!(tokens.contains(&(SemKind::StringLit, "PlainString".into())));
        let analyzer = Analyzer::embedded();
        let parse = analyzer.parse(src);
        let offset = src.find("HumanSKL.Walk").unwrap() as u32;
        assert_eq!(
            semantic_tokens_range(&analyzer, &parse, Span::new(offset, offset + 1)),
            semantic_tokens(&analyzer, &parse)
        );
    }

    #[test]
    fn classifies_weapon_tokens() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\n  DeathType = BURNED\nEnd ; done\n";
        let t = toks(src);
        let has = |k: SemKind, s: &str| t.iter().any(|(kk, ss)| *kk == k && ss == s);
        assert!(has(SemKind::Keyword, "Weapon"));
        assert!(has(SemKind::BlockName, "AK47"));
        assert!(has(SemKind::Field, "PrimaryDamage"));
        assert!(has(SemKind::Operator, "="));
        assert!(has(SemKind::Number, "50.0"));
        assert!(has(SemKind::EnumMember, "BURNED"));
        assert!(has(SemKind::Keyword, "End"));
        assert!(t.iter().any(|(k, _)| *k == SemKind::Comment));
    }

    #[test]
    fn classifies_module_type_name() {
        let src = "Object Tank\n  Body = ActiveBody Tag01\n    MaxHealth = 100\n  End\nEnd\n";
        let t = toks(src);
        assert!(t
            .iter()
            .any(|(k, s)| *k == SemKind::Module && s == "ActiveBody"));
        assert!(t.iter().any(|(k, s)| *k == SemKind::Number && s == "100"));
    }

    #[test]
    fn classifies_particle_keyframe_values_as_numbers() {
        let src = "ParticleSystem Test\n  Alpha1 = 0 1 2\n  Color1 = R:0 G:1 B:2 3\nEnd\n";
        let t = toks(src);
        for value in ["0", "1", "2", "R:0", "G:1", "B:2", "3"] {
            assert!(
                t.iter()
                    .any(|(kind, text)| *kind == SemKind::Number && text == value),
                "{value} was not classified as a number"
            );
        }
    }

    #[test]
    fn classifies_coordinate_axes_as_numbers() {
        let src = "Object Test\n  Behavior = DefaultProductionExitUpdate ModuleTag_07\n    NaturalRallyPoint = X:0.0 Y:-60.0 Z:0.0\n  End\nEnd\n";
        let t = toks(src);
        for axis in ["X:0.0", "Y:-60.0", "Z:0.0"] {
            assert!(
                t.iter()
                    .any(|(kind, text)| *kind == SemKind::Number && text == axis),
                "{axis} was not classified as a number"
            );
        }
    }

    #[test]
    fn remove_module_is_keyword_and_tag_is_reference() {
        let src = "Object Tank\n  Behavior = DestroyDie ModuleTag_01\n  End\n  RemoveModule ModuleTag_01\nEnd\n";
        let t = toks(src);
        assert!(t
            .iter()
            .any(|(kind, text)| *kind == SemKind::Keyword && text == "RemoveModule"));
        assert_eq!(
            t.iter()
                .filter(|(kind, text)| *kind == SemKind::Reference && text == "ModuleTag_01")
                .count(),
            2
        );
    }

    #[test]
    fn range_tokens_cover_exactly_the_intersecting_blocks() {
        let a = Analyzer::embedded();
        let src = "Weapon A\nEnd\nWeapon B\n  ClipSize = 1\nEnd\nWeapon C\nEnd\n";
        let parse = a.parse(src);
        let all = semantic_tokens(&a, &parse);

        // A span inside block B only: tokens must include B's and none of C's
        // (block-granular, so a superset of the span itself is fine).
        let b_start = src.find("Weapon B").unwrap() as u32;
        let ranged = semantic_tokens_range(&a, &parse, Span::new(b_start + 2, b_start + 3));
        assert!(ranged.iter().all(|t| all.contains(t)), "subset of full");
        let text = |t: &SemToken| &src[t.span.start as usize..t.span.end as usize];
        assert!(ranged.iter().any(|t| text(t) == "B"));
        assert!(ranged.iter().any(|t| text(t) == "ClipSize"));
        assert!(!ranged.iter().any(|t| text(t) == "A"));
        assert!(!ranged.iter().any(|t| text(t) == "C"));

        // The whole-document range equals the full pass.
        assert_eq!(
            semantic_tokens_range(&a, &parse, Span::new(0, src.len() as u32)),
            all
        );
    }

    #[test]
    fn tokens_are_sorted_and_non_overlapping() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n";
        let toks = semantic_tokens(&a, &a.parse(src));
        for w in toks.windows(2) {
            assert!(w[0].span.end <= w[1].span.start, "overlap: {:?}", w);
        }
    }
}
