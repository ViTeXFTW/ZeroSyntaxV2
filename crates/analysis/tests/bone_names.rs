use zerosyntax_analysis::{completion::complete, index::ModelAsset, Analyzer, WorkspaceIndex};

#[test]
fn bone_completions_deduplicate_aliases_and_preserve_prefix_insertion() {
    let analyzer = Analyzer::embedded();
    let mut index = WorkspaceIndex::new();
    index.set_file_models(
        "good.w3d",
        vec![ModelAsset {
            name: "Good".into(),
            members: vec![
                "Good.Fire01".into(),
                "Fire01".into(),
                "FIRE01".into(),
                "Fire02".into(),
            ],
        }],
    );
    for (value, expected_insert) in [
        ("$", Some("Bone:Fire01")),
        ("Bone:$", None),
        ("Bone: $", None),
        ("Bone: F$", None),
        ("Bone:F$", None),
    ] {
        let marked = format!("Object Tank\n Draw = W3DModelDraw Tag\n  DefaultConditionState\n   Model = Good\n  End\n End\n Behavior = TransitionDamageFX Damage\n  DamagedParticleSystem1 = {value} RandomBone:No PSys:Smoke\n End\nEnd\n");
        let offset = marked.find('$').unwrap();
        let src = marked.replace('$', "");
        let out = complete(
            &analyzer,
            &analyzer.parse(&src),
            offset as u32,
            Some(&index),
            None,
        );
        let matches = out
            .iter()
            .filter(|item| item.label.eq_ignore_ascii_case("Fire01"))
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "{value}: {out:?}");
        assert_eq!(matches[0].insert.as_deref(), expected_insert, "{value}");
        assert!(
            !out.iter().any(|item| item.label.contains("Good.")),
            "{value}: {out:?}"
        );
    }
}
