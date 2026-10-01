//! The tool catalog is a contract: every harness and every model reads exactly
//! these names, descriptions and schemas. A snapshot test keeps a description
//! edit from shipping by accident — and makes the diff reviewable.
//!
//! Regenerate after an intentional change: `UPDATE_GOLDEN=1 cargo test -p
//! opencraylsp-tools --test tool_defs_golden`.

use std::path::PathBuf;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("tool_defs.json")
}

#[test]
fn tool_defs_match_the_golden_file() {
    let actual =
        serde_json::to_string_pretty(&opencraylsp_tools::tool_defs()).expect("serializable") + "\n";
    let path = golden_path();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
        std::fs::write(&path, &actual).expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error} (run with UPDATE_GOLDEN=1 to record it)",
            path.display()
        )
    });
    assert_eq!(
        actual, expected,
        "the tool catalog changed; re-run with UPDATE_GOLDEN=1 to accept it"
    );
}

#[test]
fn the_catalog_fits_the_size_budget() {
    // The catalog is pasted into every conversation's context, so it has a
    // budget (10 KB compact, the wire form the MCP layer sends).
    let defs = opencraylsp_tools::tool_defs();
    for def in &defs {
        assert!(
            def.description.chars().count() <= 600,
            "`{}` has a {} character description",
            def.name,
            def.description.chars().count()
        );
        for (field, property) in def.input_schema["properties"]
            .as_object()
            .into_iter()
            .flatten()
        {
            if let Some(description) = property["description"].as_str() {
                assert!(
                    description.chars().count() <= 80,
                    "`{}`.`{field}` has a {} character description",
                    def.name,
                    description.chars().count()
                );
            }
        }
    }
    let compact = serde_json::to_string(&defs).expect("serializable");
    assert!(
        compact.len() <= 10 * 1024,
        "the serialized catalog is {} bytes; the budget is 10240",
        compact.len()
    );
}

#[test]
fn every_schema_property_is_described() {
    // A model guessing what `column` means is a model that calls the tool
    // wrong; every property says what it is.
    for def in opencraylsp_tools::tool_defs() {
        let properties = def.input_schema["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (field, property) in properties {
            assert!(
                property["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "`{}`.`{field}` has no description",
                def.name
            );
        }
    }
}
