//! Conversions between analysis byte offsets and LSP line/character positions,
//! and between analysis result types and `lsp-types`.
//!
//! LSP `character` columns are counted in *negotiated* code units: UTF-16 by
//! default (the protocol's mandatory baseline), or UTF-8 when the client
//! advertises support and we pick it at `initialize`. Every conversion here
//! therefore takes the negotiated [`PositionEnc`].

use ropey::Rope;
use tower_lsp::lsp_types::*;
use zerosyntax_analysis::completion::{Completion, CompletionKind};
use zerosyntax_analysis::diagnostics::{Diagnostic as AnDiagnostic, Severity};
use zerosyntax_analysis::outline::{DocSymbol, SymKind};
use zerosyntax_analysis::semantic::SemKind;
use zerosyntax_analysis::Span;

/// The position encoding negotiated with the client at `initialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEnc {
    /// `character` counts UTF-8 bytes from the line start.
    Utf8,
    /// `character` counts UTF-16 code units from the line start (LSP default).
    #[default]
    Utf16,
}

/// Pick the best encoding the client supports: UTF-8 if offered (no conversion
/// cost on our byte-offset spans), otherwise the mandatory UTF-16 baseline.
pub fn negotiate_encoding(caps: &ClientCapabilities) -> (PositionEnc, PositionEncodingKind) {
    let offered = caps
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_deref())
        .unwrap_or(&[]);
    if offered.contains(&PositionEncodingKind::UTF8) {
        (PositionEnc::Utf8, PositionEncodingKind::UTF8)
    } else {
        (PositionEnc::Utf16, PositionEncodingKind::UTF16)
    }
}

/// Char range of `line`'s content, excluding any trailing line break — LSP
/// clamps an oversized `character` to the line *length*; the break itself is
/// addressed as `(line + 1, 0)`.
fn line_content(rope: &Rope, line: usize) -> (usize, usize) {
    let start = rope.line_to_char(line);
    let mut end = if line + 1 < rope.len_lines() {
        rope.line_to_char(line + 1)
    } else {
        rope.len_chars()
    };
    while end > start {
        let c = rope.char(end - 1);
        if c == '\n' || c == '\r' {
            end -= 1;
        } else {
            break;
        }
    }
    (start, end)
}

/// Convert a byte offset to an LSP position via the rope.
pub fn offset_to_position(rope: &Rope, byte: u32, enc: PositionEnc) -> Position {
    let byte = (byte as usize).min(rope.len_bytes());
    let char = rope.byte_to_char(byte);
    let line = rope.char_to_line(char);
    let line_start = rope.line_to_char(line);
    let character = match enc {
        PositionEnc::Utf8 => byte - rope.char_to_byte(line_start),
        PositionEnc::Utf16 => rope.char_to_utf16_cu(char) - rope.char_to_utf16_cu(line_start),
    };
    Position {
        line: line as u32,
        character: character as u32,
    }
}

/// Convert an LSP position to a rope char index (clamped per the LSP rules:
/// line to the last line, character to the line's content length).
pub fn position_to_char(rope: &Rope, pos: Position, enc: PositionEnc) -> usize {
    let line = (pos.line as usize).min(rope.len_lines().saturating_sub(1));
    let (start, end) = line_content(rope, line);
    match enc {
        PositionEnc::Utf8 => {
            let start_b = rope.char_to_byte(start);
            let end_b = rope.char_to_byte(end);
            let target = (start_b + pos.character as usize).min(end_b);
            // byte_to_char floors to a char boundary, tolerating mid-scalar input.
            rope.byte_to_char(target)
        }
        PositionEnc::Utf16 => {
            let start_cu = rope.char_to_utf16_cu(start);
            let end_cu = rope.char_to_utf16_cu(end);
            let target = (start_cu + pos.character as usize).min(end_cu);
            rope.utf16_cu_to_char(target)
        }
    }
}

/// Convert an LSP position to a byte offset via the rope (clamped to bounds).
pub fn position_to_offset(rope: &Rope, pos: Position, enc: PositionEnc) -> u32 {
    rope.char_to_byte(position_to_char(rope, pos, enc)) as u32
}

/// Apply one LSP `didChange` content change to the rope in place. A change
/// without a range replaces the whole document (clients may always fall back
/// to full sync for a single change).
pub fn apply_change(rope: &mut Rope, range: Option<Range>, text: &str, enc: PositionEnc) {
    match range {
        None => *rope = Rope::from_str(text),
        Some(r) => {
            let start = position_to_char(rope, r.start, enc);
            let end = position_to_char(rope, r.end, enc).max(start);
            rope.remove(start..end);
            rope.insert(start, text);
        }
    }
}

/// Convert an analysis span to an LSP range.
pub fn span_to_range(rope: &Rope, span: Span, enc: PositionEnc) -> Range {
    Range {
        start: offset_to_position(rope, span.start, enc),
        end: offset_to_position(rope, span.end, enc),
    }
}

/// Convert an analysis diagnostic to an LSP diagnostic.
pub fn to_lsp_diagnostic(rope: &Rope, d: &AnDiagnostic, enc: PositionEnc) -> Diagnostic {
    Diagnostic {
        range: span_to_range(rope, d.span, enc),
        severity: Some(match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Hint => DiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(d.code.to_string())),
        source: Some("zerosyntax".to_string()),
        message: d.message.clone(),
        ..Default::default()
    }
}

/// Convert an analysis completion to an LSP completion item.
///
/// When `snippets_supported` is true and the item carries an `insert` string,
/// the response uses `insertText` with `InsertTextFormat::SNIPPET` instead of
/// the plain label. Module-slot field completions (e.g. `Behavior = $0`) also
/// set a `triggerSuggest` command so the module-name popup opens automatically
/// after the `=` is inserted.
pub fn to_lsp_completion(c: Completion, snippets_supported: bool) -> CompletionItem {
    let (insert_text, insert_text_format, command) = if snippets_supported {
        if let Some(snippet) = c.insert {
            // Slot completions like `Behavior = $0` — trigger the suggest
            // popup so the module list appears immediately.
            let cmd = if snippet.ends_with("= $0") {
                Some(Command {
                    title: "Trigger Suggest".into(),
                    command: "editor.action.triggerSuggest".into(),
                    arguments: None,
                })
            } else {
                None
            };
            (Some(snippet), Some(InsertTextFormat::SNIPPET), cmd)
        } else {
            (None, None, None)
        }
    } else {
        (None, None, None)
    };

    let data = (c.kind == CompletionKind::W3dModel).then(|| {
        serde_json::json!({
            "zerosyntax": "w3d-model-preview",
            "model": c.label,
        })
    });
    let documentation = c.documentation.map(|value| {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        })
    });
    CompletionItem {
        label: c.label,
        kind: Some(match c.kind {
            CompletionKind::Block => CompletionItemKind::CLASS,
            CompletionKind::Field => CompletionItemKind::PROPERTY,
            CompletionKind::Module => CompletionItemKind::CONSTRUCTOR,
            CompletionKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
            CompletionKind::Value => CompletionItemKind::VALUE,
            CompletionKind::Reference => CompletionItemKind::REFERENCE,
            CompletionKind::W3dModel => CompletionItemKind::REFERENCE,
        }),
        detail: c.detail,
        documentation,
        data,
        insert_text,
        insert_text_format,
        command,
        ..Default::default()
    }
}

/// Convert an analysis outline symbol to an LSP `DocumentSymbol` (recursive).
pub fn to_lsp_document_symbol(rope: &Rope, s: &DocSymbol, enc: PositionEnc) -> DocumentSymbol {
    #[allow(deprecated)] // `deprecated` field is required by the struct literal
    DocumentSymbol {
        name: s.name.clone(),
        detail: s.detail.clone(),
        kind: match s.kind {
            SymKind::Block => SymbolKind::CLASS,
            SymKind::Module => SymbolKind::CONSTRUCTOR,
            SymKind::SubBlock => SymbolKind::STRUCT,
        },
        tags: None,
        deprecated: None,
        range: span_to_range(rope, s.span, enc),
        selection_range: span_to_range(rope, s.selection, enc),
        children: Some(
            s.children
                .iter()
                .map(|c| to_lsp_document_symbol(rope, c, enc))
                .collect(),
        ),
    }
}

/// Convert a foldable scope span to an LSP folding range. Folds from the
/// header line to the closing `End`'s line, so the header stays visible.
pub fn to_lsp_folding_range(rope: &Rope, span: Span, enc: PositionEnc) -> Option<FoldingRange> {
    let start = offset_to_position(rope, span.start, enc);
    // The node's range ends after the trailing newline; step back one byte so
    // the end line is the `End` line itself.
    let end = offset_to_position(rope, span.end.saturating_sub(1).max(span.start), enc);
    (end.line > start.line).then_some(FoldingRange {
        start_line: start.line,
        end_line: end.line,
        kind: Some(FoldingRangeKind::Region),
        ..Default::default()
    })
}

/// The semantic-token legend (type names), ordered to match [`sem_kind_index`].
pub fn semantic_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::KEYWORD,     // 0
            SemanticTokenType::CLASS,       // 1
            SemanticTokenType::PROPERTY,    // 2
            SemanticTokenType::TYPE,        // 3
            SemanticTokenType::ENUM_MEMBER, // 4
            SemanticTokenType::OPERATOR,    // 5
            SemanticTokenType::NUMBER,      // 6
            SemanticTokenType::STRING,      // 7
            SemanticTokenType::VARIABLE,    // 8
            SemanticTokenType::COMMENT,     // 9
        ],
        token_modifiers: vec![],
    }
}

fn sem_kind_index(kind: SemKind) -> u32 {
    match kind {
        SemKind::Keyword => 0,
        SemKind::BlockName => 1,
        SemKind::Field => 2,
        SemKind::Module => 3,
        SemKind::EnumMember => 4,
        SemKind::Operator => 5,
        SemKind::Number => 6,
        SemKind::StringLit => 7,
        SemKind::Reference => 8,
        SemKind::Comment => 9,
    }
}

/// Delta-encode analysis semantic tokens into the LSP wire format. Input must be
/// sorted by start offset (as produced by `semantic_tokens`). Tokens never span
/// lines, so `length` is the column difference in negotiated units.
pub fn to_lsp_semantic_tokens(
    rope: &Rope,
    tokens: &[zerosyntax_analysis::semantic::SemToken],
    enc: PositionEnc,
) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for t in tokens {
        let start = offset_to_position(rope, t.span.start, enc);
        let end = offset_to_position(rope, t.span.end, enc);
        let length = end.character.saturating_sub(start.character);
        let delta_line = start.line - prev_line;
        let delta_start = if delta_line == 0 {
            start.character - prev_start
        } else {
            start.character
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: sem_kind_index(t.kind),
            token_modifiers_bitset: 0,
        });
        prev_line = start.line;
        prev_start = start.character;
    }
    out
}

/// The single splice turning `prev` into `next`, as a `semanticTokens/delta`
/// edit. Token-aligned common prefix/suffix; protocol offsets count raw
/// `u32`s, i.e. tokens × 5.
pub fn semantic_tokens_splice(
    prev: &[SemanticToken],
    next: &[SemanticToken],
) -> SemanticTokensEdit {
    let prefix = prev
        .iter()
        .zip(next.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = prev[prefix..]
        .iter()
        .rev()
        .zip(next[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    SemanticTokensEdit {
        start: (prefix * 5) as u32,
        delete_count: ((prev.len() - prefix - suffix) * 5) as u32,
        data: Some(next[prefix..next.len() - suffix].to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerosyntax_analysis::completion::{Completion, CompletionKind};
    use zerosyntax_analysis::semantic::{SemKind, SemToken};

    /// Apply a splice the way a client would, for the equivalence test below.
    fn apply_splice(prev: &[SemanticToken], edit: &SemanticTokensEdit) -> Vec<SemanticToken> {
        assert_eq!(edit.start % 5, 0, "token-aligned");
        assert_eq!(edit.delete_count % 5, 0, "token-aligned");
        let start = (edit.start / 5) as usize;
        let del = (edit.delete_count / 5) as usize;
        let mut out = prev[..start].to_vec();
        out.extend(edit.data.clone().unwrap_or_default());
        out.extend(prev[start + del..].iter().copied());
        out
    }

    #[test]
    fn model_completion_is_tagged_but_not_eagerly_rendered() {
        let item = to_lsp_completion(
            Completion {
                label: "AVTank".into(),
                kind: CompletionKind::W3dModel,
                detail: Some("W3D model".into()),
                documentation: None,
                insert: None,
            },
            false,
        );
        assert_eq!(
            item.data.as_ref().and_then(|data| data.get("model")),
            Some(&serde_json::json!("AVTank"))
        );
        assert!(item.documentation.is_none());
    }

    #[test]
    fn completion_documentation_is_sent_as_markdown() {
        let item = to_lsp_completion(
            Completion {
                label: "ModuleTag_Physics".into(),
                kind: CompletionKind::Reference,
                detail: Some("module tag".into()),
                documentation: Some(
                    "```ini\nBehavior = PhysicsBehavior ModuleTag_Physics\n```".into(),
                ),
                insert: None,
            },
            false,
        );
        assert!(matches!(
            item.documentation,
            Some(Documentation::MarkupContent(_))
        ));
    }

    #[test]
    fn splice_reproduces_next_from_prev() {
        let tok = |dl, ds, len, ty| SemanticToken {
            delta_line: dl,
            delta_start: ds,
            length: len,
            token_type: ty,
            token_modifiers_bitset: 0,
        };
        let prev = vec![tok(0, 0, 6, 0), tok(0, 7, 4, 1), tok(1, 0, 3, 0)];
        for next in [
            vec![tok(0, 0, 6, 0), tok(0, 7, 9, 1), tok(1, 0, 3, 0)], // middle change
            vec![tok(0, 0, 6, 0), tok(1, 0, 3, 0)],                  // deletion
            prev.clone(),                                            // no change
            vec![],                                                  // everything gone
            vec![
                tok(0, 0, 6, 0),
                tok(0, 7, 4, 1),
                tok(0, 5, 2, 2),
                tok(1, 0, 3, 0),
            ], // insert
        ] {
            let edit = semantic_tokens_splice(&prev, &next);
            assert_eq!(apply_splice(&prev, &edit), next);
        }
    }

    #[test]
    fn position_offset_round_trip() {
        let rope = Rope::from_str("Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n");
        for enc in [PositionEnc::Utf8, PositionEnc::Utf16] {
            for byte in [0u32, 7, 14, 30, 35] {
                let pos = offset_to_position(&rope, byte, enc);
                assert_eq!(
                    position_to_offset(&rope, pos, enc),
                    byte,
                    "byte {byte} via {pos:?} ({enc:?})"
                );
            }
        }
    }

    #[test]
    fn position_on_second_line() {
        let rope = Rope::from_str("Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n");
        // The 'P' of PrimaryDamage is at line 1, char 2.
        let byte = "Weapon AK47\n  ".len() as u32;
        assert_eq!(
            offset_to_position(&rope, byte, PositionEnc::Utf16),
            Position {
                line: 1,
                character: 2
            }
        );
    }

    #[test]
    fn non_ascii_columns_differ_by_encoding() {
        // "; émoji 🚀 comment" — é is 2 UTF-8 bytes / 1 UTF-16 unit,
        // 🚀 is 4 UTF-8 bytes / 2 UTF-16 units.
        let rope = Rope::from_str("; \u{e9}moji \u{1F680} x\nEnd\n");
        let byte_of_x = "; \u{e9}moji \u{1F680} ".len() as u32;
        let p8 = offset_to_position(&rope, byte_of_x, PositionEnc::Utf8);
        let p16 = offset_to_position(&rope, byte_of_x, PositionEnc::Utf16);
        assert_eq!(p8.character, byte_of_x);
        assert_eq!(p16.character, "; émoji ".chars().count() as u32 + 2 + 1);
        // Both round-trip to the same byte.
        assert_eq!(position_to_offset(&rope, p8, PositionEnc::Utf8), byte_of_x);
        assert_eq!(
            position_to_offset(&rope, p16, PositionEnc::Utf16),
            byte_of_x
        );
    }

    #[test]
    fn position_clamps_to_line_content() {
        let rope = Rope::from_str("abc\r\ndef\n");
        // Past-end character clamps to the line length, not into the \r\n.
        let pos = Position {
            line: 0,
            character: 99,
        };
        assert_eq!(position_to_offset(&rope, pos, PositionEnc::Utf16), 3);
        // Past-end line clamps to the last line.
        let pos = Position {
            line: 99,
            character: 0,
        };
        assert_eq!(position_to_offset(&rope, pos, PositionEnc::Utf16), 9);
    }

    #[test]
    fn apply_change_replaces_range() {
        let mut rope = Rope::from_str("Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n");
        // Replace "50.0" with "75.5".
        let range = Range {
            start: Position {
                line: 1,
                character: 18,
            },
            end: Position {
                line: 1,
                character: 22,
            },
        };
        apply_change(&mut rope, Some(range), "75.5", PositionEnc::Utf16);
        assert_eq!(
            rope.to_string(),
            "Weapon AK47\n  PrimaryDamage = 75.5\nEnd\n"
        );
    }

    #[test]
    fn apply_change_across_lines_and_at_eof() {
        let mut rope = Rope::from_str("Weapon A\nEnd\nWeapon B\nEnd\n");
        // Delete the whole second block including its leading newline.
        let range = Range {
            start: Position {
                line: 2,
                character: 0,
            },
            end: Position {
                line: 4,
                character: 0,
            },
        };
        apply_change(&mut rope, Some(range), "", PositionEnc::Utf16);
        assert_eq!(rope.to_string(), "Weapon A\nEnd\n");
        // Insert at EOF.
        let at_eof = Range {
            start: Position {
                line: 2,
                character: 0,
            },
            end: Position {
                line: 2,
                character: 0,
            },
        };
        apply_change(
            &mut rope,
            Some(at_eof),
            "Weapon C\nEnd\n",
            PositionEnc::Utf16,
        );
        assert_eq!(rope.to_string(), "Weapon A\nEnd\nWeapon C\nEnd\n");
        // Range-less change replaces everything.
        apply_change(&mut rope, None, "GameData\nEnd\n", PositionEnc::Utf16);
        assert_eq!(rope.to_string(), "GameData\nEnd\n");
    }

    #[test]
    fn apply_change_crlf_document() {
        let mut rope = Rope::from_str("Weapon AK47\r\n  PrimaryDamage = 50.0\r\nEnd\r\n");
        let range = Range {
            start: Position {
                line: 1,
                character: 18,
            },
            end: Position {
                line: 1,
                character: 22,
            },
        };
        apply_change(&mut rope, Some(range), "75.5", PositionEnc::Utf16);
        assert_eq!(
            rope.to_string(),
            "Weapon AK47\r\n  PrimaryDamage = 75.5\r\nEnd\r\n"
        );
    }

    #[test]
    fn negotiates_utf8_when_offered() {
        let mut caps = ClientCapabilities::default();
        assert_eq!(negotiate_encoding(&caps).0, PositionEnc::Utf16);
        caps.general = Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8,
            ]),
            ..Default::default()
        });
        assert_eq!(negotiate_encoding(&caps).0, PositionEnc::Utf8);
    }

    /// Property test: random LSP delta sequences applied to the rope must
    /// reproduce the text computed by an independent string-based reference.
    /// Positions are generated freely (past-end lines/columns included) but
    /// only BMP text is used, so UTF-16 clamping is unambiguous.
    #[test]
    fn random_delta_sequences_match_reference() {
        fn ref_byte(text: &str, line: u32, ch: u32, enc: PositionEnc) -> usize {
            let lines: Vec<&str> = text.split('\n').collect();
            let line = (line as usize).min(lines.len() - 1);
            let start: usize = lines[..line].iter().map(|l| l.len() + 1).sum();
            let content = lines[line].strip_suffix('\r').unwrap_or(lines[line]);
            match enc {
                PositionEnc::Utf8 => {
                    let mut t = start + (ch as usize).min(content.len());
                    while !text.is_char_boundary(t) {
                        t -= 1;
                    }
                    t
                }
                PositionEnc::Utf16 => {
                    let mut cu = 0usize;
                    for (i, c) in content.char_indices() {
                        if cu >= ch as usize {
                            return start + i;
                        }
                        cu += c.len_utf16();
                    }
                    start + content.len()
                }
            }
        }
        fn ref_apply(text: &str, r: Option<Range>, new: &str, enc: PositionEnc) -> String {
            match r {
                None => new.to_string(),
                Some(r) => {
                    let s = ref_byte(text, r.start.line, r.start.character, enc);
                    let e = ref_byte(text, r.end.line, r.end.character, enc).max(s);
                    format!("{}{}{}", &text[..s], new, &text[e..])
                }
            }
        }

        let snippets = [
            "",
            "X",
            "\u{e9}",
            "\n",
            "\r\n",
            "Weapon Z\nEnd\n",
            "; comment \u{e9}\n",
            " = 5.0",
            "End",
        ];
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };

        for enc in [PositionEnc::Utf8, PositionEnc::Utf16] {
            for round in 0..200 {
                let mut expected = String::from("Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n");
                let mut rope = Rope::from_str(&expected);
                for edit in 0..8 {
                    let text = snippets[(next() % snippets.len() as u32) as usize];
                    let range = if next() % 10 == 0 {
                        None // full-replacement fallback
                    } else {
                        let a = Position {
                            line: next() % 8,
                            character: next() % 40,
                        };
                        let b = Position {
                            line: next() % 8,
                            character: next() % 40,
                        };
                        // LSP requires start <= end; order by reference bytes.
                        let (s, e) = if ref_byte(&expected, a.line, a.character, enc)
                            <= ref_byte(&expected, b.line, b.character, enc)
                        {
                            (a, b)
                        } else {
                            (b, a)
                        };
                        Some(Range { start: s, end: e })
                    };
                    expected = ref_apply(&expected, range, text, enc);
                    apply_change(&mut rope, range, text, enc);
                    assert_eq!(
                        rope.to_string(),
                        expected,
                        "diverged ({enc:?}, round {round}, edit {edit}, range {range:?}, text {text:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn delta_encodes_tokens_across_lines() {
        let rope = Rope::from_str("Weapon AK47\nEnd\n");
        let tokens = vec![
            SemToken {
                span: Span::new(0, 6),
                kind: SemKind::Keyword,
            }, // "Weapon" line0 col0
            SemToken {
                span: Span::new(7, 11),
                kind: SemKind::BlockName,
            }, // "AK47"   line0 col7
            SemToken {
                span: Span::new(12, 15),
                kind: SemKind::Keyword,
            }, // "End"    line1 col0
        ];
        let lsp = to_lsp_semantic_tokens(&rope, &tokens, PositionEnc::Utf16);
        assert_eq!(lsp[0].delta_line, 0);
        assert_eq!(lsp[0].delta_start, 0);
        assert_eq!(lsp[0].length, 6);
        assert_eq!(lsp[1].delta_line, 0);
        assert_eq!(lsp[1].delta_start, 7); // 7 - 0
        assert_eq!(lsp[2].delta_line, 1);
        assert_eq!(lsp[2].delta_start, 0); // new line resets column
        assert_eq!(lsp[2].length, 3);
    }
}
