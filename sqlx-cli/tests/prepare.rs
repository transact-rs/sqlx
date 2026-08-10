use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};

fn run_prepare(project_dir: &Path, target_dir: &Path) {
    Command::cargo_bin("cargo-sqlx")
        .unwrap()
        .current_dir(project_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .args([
            "sqlx",
            "prepare",
            "--database-url",
            "sqlite::memory:",
            "--",
            "--lib",
        ])
        .assert()
        .success();
}

fn query_file(project_dir: &Path) -> PathBuf {
    let mut query_files = fs::read_dir(project_dir.join(".sqlx"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("query-") && name.ends_with(".json"))
        });
    let query_file = query_files
        .next()
        .expect("prepare did not create query data");

    assert!(
        query_files.next().is_none(),
        "prepare created more than one query file"
    );

    query_file
}

#[test]
fn repeated_sqlite_prepare_is_byte_stable() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let temp_dir = tempfile::Builder::new()
        .prefix("sqlx-cli-prepare-")
        .tempdir_in(workspace_root.join("target"))
        .unwrap();
    let project_dir = temp_dir.path().join("fixture");
    let target_dir = temp_dir.path().join("target");
    fs::create_dir_all(project_dir.join("src")).unwrap();
    fs::write(
        project_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "sqlite-prepare-fixture"
version = "0.0.0"
edition = "2021"

[workspace]

[dependencies]
sqlx = {{ path = {:?}, default-features = false, features = ["macros", "runtime-tokio", "sqlite"] }}
"#,
            workspace_root
        ),
    )
    .unwrap();
    fs::write(
        project_dir.join("src/lib.rs"),
        r#"pub fn query() {
    let _ = sqlx::query!("SELECT 1 AS value");
}
"#,
    )
    .unwrap();

    run_prepare(&project_dir, &target_dir);
    let first = fs::read(query_file(&project_dir)).unwrap();
    run_prepare(&project_dir, &target_dir);
    let second = fs::read(query_file(&project_dir)).unwrap();
    let expected = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sqlx-macros-core/tests/fixtures/sqlite-query.json"
    ));

    assert_eq!(first, expected);
    assert_eq!(second, expected);
    assert_eq!(first, second);
}
