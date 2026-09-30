// SPDX-License-Identifier: GPL-3.0-or-later
//! Analysis-path benchmarks over synthetic schema-conformant documents:
//! parse with the real schema oracle, then each per-keystroke analysis pass
//! (diagnose, semantic tokens, definition extraction) in isolation.
//!
//! Together with the syntax benches these decide roadmap Phase 3: whether the
//! keystroke path needs incremental reparse, incremental analysis, or neither.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use zerosyntax_analysis::diagnostics::DiagnosticsCache;
use zerosyntax_analysis::{
    completion, diagnostics, index, semantic, Analyzer, Span, WorkspaceIndex,
};
use zerosyntax_syntax::{Edit, Strategy};

/// Same shape as the syntax-crate generator, but kept schema-conformant so the
/// diagnostics pass runs its real (non-error) field/value validation paths.
fn synthetic_ini(target_lines: usize) -> String {
    let mut out = String::new();
    let mut lines = 0usize;
    let mut i = 0usize;
    while lines < target_lines {
        if i.is_multiple_of(2) {
            out.push_str(&format!(
                "Weapon GenBenchWeapon{i}\n\
                 \x20 PrimaryDamage = 40.0\n\
                 \x20 PrimaryDamageRadius = 5.0\n\
                 \x20 SecondaryDamage = 10.0\n\
                 \x20 SecondaryDamageRadius = 10.0\n\
                 \x20 AttackRange = 150.0\n\
                 \x20 MinimumAttackRange = 10.0\n\
                 \x20 DamageType = ARMOR_PIERCING\n\
                 \x20 DeathType = EXPLODED\n\
                 \x20 WeaponSpeed = 600.0\n\
                 \x20 ProjectileObject = GenBenchProjectile{i}\n\
                 \x20 FireSound = TankFire\n\
                 \x20 ScatterRadius = 2.5\n\
                 \x20 AcceptableAimDelta = 5.0 ; degrees\n\
                 \x20 RadiusDamageAngle = 180.0\n\
                 End\n\n"
            ));
            lines += 17;
        } else {
            out.push_str(&format!(
                "Object GenBenchTank{i}\n\
                 \x20 Side = America\n\
                 \x20 BuildCost = 900\n\
                 \x20 BuildTime = 10.0\n\
                 \x20 VisionRange = 150.0\n\
                 \x20 KindOf = VEHICLE SELECTABLE\n\
                 \x20 Draw = W3DModelDraw ModuleTag_Draw\n\
                 \x20   ConditionState NONE\n\
                 \x20     Animation = GenBenchTank{i}.Idle\n\
                 \x20   End\n\
                 \x20   ConditionState REALLYDAMAGED\n\
                 \x20     Animation = GenBenchTank{i}.IdleDamaged\n\
                 \x20   End\n\
                 \x20 End\n\
                 \x20 Body = ActiveBody ModuleTag_Body\n\
                 \x20   MaxHealth = 300.0\n\
                 \x20   InitialHealth = 300.0\n\
                 \x20 End\n\
                 \x20 Behavior = ArmorUpgrade ModuleTag_Armor\n\
                 \x20   TriggeredBy = Upgrade_GenBench\n\
                 \x20 End\n\
                 End\n\n"
            ));
            lines += 23;
        }
        i += 1;
    }
    out
}

fn bench_analyze(c: &mut Criterion) {
    let analyzer = Analyzer::embedded();

    let mut group = c.benchmark_group("analysis");
    group.sample_size(50);
    for &lines in &[1_000usize, 10_000, 50_000] {
        let src = synthetic_ini(lines);
        let parse = analyzer.parse(&src);
        assert!(parse.errors.is_empty(), "synthetic input must parse clean");
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::new("parse", lines), &src, |b, src| {
            b.iter(|| black_box(analyzer.parse(src)))
        });
        group.bench_with_input(BenchmarkId::new("diagnose", lines), &parse, |b, parse| {
            b.iter(|| black_box(diagnostics::diagnose(&analyzer, parse, None, None)))
        });
        group.bench_with_input(
            BenchmarkId::new("semantic_tokens", lines),
            &parse,
            |b, parse| b.iter(|| black_box(semantic::semantic_tokens(&analyzer, parse))),
        );
        group.bench_with_input(
            BenchmarkId::new("definitions_in", lines),
            &parse,
            |b, parse| b.iter(|| black_box(index::definitions_in(&analyzer, parse, "bench.ini"))),
        );
        if lines == 50_000 {
            group.bench_with_input(
                BenchmarkId::new("index_refresh", lines),
                &parse,
                |b, parse| {
                    b.iter(|| {
                        let file = "bench.ini";
                        let mut index = WorkspaceIndex::new();
                        index.set_file(file, index::definitions_in(&analyzer, parse, file));
                        index.set_file_refs(file, index::references_in(&analyzer, parse));
                        index.set_file_tags(file, index::module_tags_in(&analyzer, parse));
                        index.set_file_object_models(
                            file,
                            index::object_models_in(&analyzer, parse),
                        );
                        index.set_file_object_parents(file, index::object_parents_in(parse));
                        black_box(index)
                    })
                },
            );

            let mut completion_src = src.clone();
            completion_src.push_str("Weapon CompletionBench\n  ProjectileObject = \nEnd\n");
            let completion_offset = (completion_src.len() - "\nEnd\n".len()) as u32;
            let completion_parse = analyzer.parse(&completion_src);
            let mut completion_index = WorkspaceIndex::new();
            completion_index.set_file(
                "bench.ini",
                index::definitions_in(&analyzer, &completion_parse, "bench.ini"),
            );
            assert!(completion::complete(
                &analyzer,
                &completion_parse,
                completion_offset,
                Some(&completion_index),
                Some("bench.ini"),
            )
            .iter()
            .any(|item| item.label == "GenBenchTank1"));
            group.bench_function(BenchmarkId::new("completion_reference", lines), |b| {
                b.iter(|| {
                    black_box(completion::complete(
                        &analyzer,
                        &completion_parse,
                        completion_offset,
                        Some(&completion_index),
                        Some("bench.ini"),
                    ))
                })
            });

            let start = (src.len() / 2) as u32;
            let viewport = Span::new(start, (start + 2_000).min(src.len() as u32));
            group.bench_with_input(
                BenchmarkId::new("semantic_tokens_range", lines),
                &parse,
                |b, parse| {
                    b.iter(|| {
                        black_box(semantic::semantic_tokens_range(&analyzer, parse, viewport))
                    })
                },
            );
        }
        // The full keystroke path as the server ran it pre-Phase-3:
        // full parse + full diagnose.
        group.bench_with_input(BenchmarkId::new("keystroke", lines), &src, |b, src| {
            b.iter(|| {
                let parse = analyzer.parse(src);
                black_box(diagnostics::diagnose(&analyzer, &parse, None, None))
            })
        });

        // The Phase-3 keystroke path: incremental reparse (block splice) +
        // cached diagnose. Simulates typing one digit mid-file.
        let at = src.len() / 2 + src[src.len() / 2..].find("= 40.0").unwrap() + 2;
        let edited = format!("{}9{}", &src[..at], &src[at..]);
        group.bench_with_input(
            BenchmarkId::new("keystroke_incremental", lines),
            &(&src, &edited, &parse),
            |b, (src, edited, parse)| {
                let mut cache = DiagnosticsCache::new();
                // Warm the cache as the server would have after the open.
                diagnostics::diagnose_with_cache(&analyzer, parse, None, None, &mut cache);
                let edit = Edit {
                    start: at,
                    old_end: at,
                    new_len: 1,
                };
                b.iter(|| {
                    let (inc, strategy) = analyzer.reparse(parse, src, edited, edit);
                    assert_eq!(strategy, Strategy::Spliced);
                    black_box(diagnostics::diagnose_with_cache(
                        &analyzer, &inc, None, None, &mut cache,
                    ))
                })
            },
        );
    }
    group.finish();
}

/// Same Phase-3 keystroke path over the largest real corpus file, when the
/// corpus has been fetched (silently skipped otherwise).
fn bench_corpus_keystroke(c: &mut Criterion) {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/GeneralsGamePatch2/GeneralsZH/Data/INI/ParticleSystem.ini");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let src = String::from_utf8_lossy(&bytes).into_owned();
    let analyzer = Analyzer::embedded();
    let parse = analyzer.parse(&src);

    let at = src.len() / 2 + src[src.len() / 2..].find('=').unwrap() + 1;
    let edited = format!("{} 9{}", &src[..at], &src[at..]);
    let edit = Edit {
        start: at,
        old_end: at,
        new_len: 2,
    };

    let mut group = c.benchmark_group("corpus");
    group.sample_size(30);
    group.bench_function("keystroke_full_ParticleSystem", |b| {
        b.iter(|| {
            let p = analyzer.parse(&edited);
            black_box(diagnostics::diagnose(&analyzer, &p, None, None))
        })
    });
    group.bench_function("keystroke_incremental_ParticleSystem", |b| {
        let mut cache = DiagnosticsCache::new();
        diagnostics::diagnose_with_cache(&analyzer, &parse, None, None, &mut cache);
        b.iter(|| {
            let (inc, strategy) = analyzer.reparse(&parse, &src, &edited, edit);
            assert_eq!(strategy, Strategy::Spliced);
            black_box(diagnostics::diagnose_with_cache(
                &analyzer, &inc, None, None, &mut cache,
            ))
        })
    });
    group.bench_function("format_edits_ParticleSystem", |b| {
        b.iter(|| {
            black_box(zerosyntax_analysis::format::format_edits(
                &parse, &src, "  ",
            ))
        })
    });
    group.bench_function("semantic_tokens_range_ParticleSystem", |b| {
        // A ~60-line viewport in the middle of the file.
        let start = (src.len() / 2) as u32;
        let span = zerosyntax_analysis::Span::new(start, start + 2_000);
        b.iter(|| black_box(semantic::semantic_tokens_range(&analyzer, &parse, span)))
    });
    group.finish();
}

criterion_group!(corpus_benches, bench_corpus_keystroke);

criterion_group!(benches, bench_analyze);
criterion_main!(benches, corpus_benches);
