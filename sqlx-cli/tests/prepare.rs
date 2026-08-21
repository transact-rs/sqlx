use assert_cmd::cargo_bin_cmd;
use tempfile::TempDir;

/// Run `cargo sqlx prepare` in an empty directory with the given extra arguments,
/// returning whether it succeeded along with everything it printed.
///
/// A `--database-url` is always passed so that argument validation, not the missing
/// environment, is what decides the outcome. Nothing here gets far enough to connect.
fn run_prepare(args: &[&str]) -> (bool, String) {
    let tempdir = TempDir::new().unwrap();

    let output = cargo_bin_cmd!("cargo-sqlx")
        .current_dir(tempdir.path())
        .args([
            "sqlx",
            "prepare",
            "--database-url",
            "postgres://localhost/_",
        ])
        .args(args)
        .output()
        .unwrap();

    // `cargo-sqlx` reports errors on stdout; read both so a change there can't hide a failure.
    let printed = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);

    (output.status.success(), printed)
}

#[test]
fn per_crate_rejects_all() {
    let (success, printed) = run_prepare(&["--per-crate", "--all"]);

    assert!(!success);
    assert!(
        printed.contains("`--per-crate` cannot be combined with `--all`"),
        "unexpected output: {printed}"
    );
}

#[test]
fn per_crate_is_accepted_alongside_workspace() {
    // `--workspace` is implied by `--per-crate`, but passing both is not an error.
    // Without a `Cargo.toml` this can only get as far as locating the manifest.
    let (success, printed) = run_prepare(&["--per-crate", "--workspace"]);

    assert!(!success);
    assert!(
        printed.contains("Failed to read `Cargo.toml`") || printed.contains("CARGO"),
        "unexpected output: {printed}"
    );
}
