use std::collections::{BTreeSet, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::metadata::{manifest_dir, Metadata};
use crate::opt::ConnectOpts;
use crate::Config;
use anyhow::{bail, Context};
use console::style;
use sqlx::Connection;

pub struct PrepareCtx<'a> {
    pub config: &'a Config,
    pub workspace: bool,
    pub per_crate: bool,
    pub all: bool,
    pub cargo: OsString,
    pub cargo_args: Vec<String>,
    pub metadata: Metadata,
    pub connect_opts: ConnectOpts,
}

impl PrepareCtx<'_> {
    /// Path to the directory where cached queries should be placed.
    ///
    /// Only meaningful when not preparing per-crate; see [`CrateTarget`] for that case.
    fn prepare_dir(&self) -> anyhow::Result<PathBuf> {
        if self.workspace {
            Ok(self.metadata.workspace_root().join(".sqlx"))
        } else {
            Ok(manifest_dir(&self.cargo)?.join(".sqlx"))
        }
    }
}

/// A workspace crate and the two directories its query data passes through when preparing
/// with `--per-crate`.
#[derive(Debug, PartialEq, Eq)]
struct CrateTarget {
    /// The Cargo package name, which is how `sqlx-macros` keys the staging directory.
    name: String,
    /// Scratch directory under `target/` that `sqlx-macros` writes this crate's query data to.
    staging_dir: PathBuf,
    /// The crate's own `.sqlx` directory, next to its `Cargo.toml`.
    sqlx_dir: PathBuf,
}

pub async fn run(
    config: &Config,
    check: bool,
    all: bool,
    workspace: bool,
    per_crate: bool,
    connect_opts: ConnectOpts,
    cargo_args: Vec<String>,
) -> anyhow::Result<()> {
    if per_crate && all {
        bail!(
            "`--per-crate` cannot be combined with `--all`: \
             query data for crates outside the workspace has no crate directory to go in"
        );
    }

    // A per-crate layout is only meaningful for a whole workspace; for a single crate,
    // `cargo sqlx prepare` already writes to that crate's `.sqlx`.
    let workspace = workspace || per_crate;

    let cargo = env::var_os("CARGO")
        .context("failed to get value of `CARGO`; `prepare` subcommand may only be invoked as `cargo sqlx prepare`")?;

    anyhow::ensure!(
        Path::new("Cargo.toml").exists(),
        r#"Failed to read `Cargo.toml`.
hint: This command only works in the manifest directory of a Cargo package or workspace."#
    );

    let metadata: Metadata = Metadata::from_current_directory(&cargo)?;
    let ctx = PrepareCtx {
        config,
        workspace,
        per_crate,
        all,
        cargo,
        cargo_args,
        metadata,
        connect_opts,
    };

    if check {
        prepare_check(&ctx).await
    } else {
        prepare(&ctx).await
    }
}

async fn prepare(ctx: &PrepareCtx<'_>) -> anyhow::Result<()> {
    if ctx.connect_opts.database_url.is_some() {
        check_backend(ctx.config, &ctx.connect_opts).await?;
    }

    if ctx.per_crate {
        return prepare_per_crate(ctx);
    }

    let prepare_dir = ctx.prepare_dir()?;
    run_prepare_step(ctx, &prepare_dir)?;

    // Warn if no queries were generated. Glob since the directory may contain unrelated files.
    if glob_query_files(prepare_dir)?.is_empty() {
        println!("{} no queries found", style("warning:").yellow());
        return Ok(());
    }

    if ctx.workspace {
        println!(
            "query data written to .sqlx in the workspace root; \
             please check this into version control"
        );
    } else {
        println!(
            "query data written to .sqlx in the current directory; \
             please check this into version control"
        );
    }
    Ok(())
}

/// Run `prepare` with a `.sqlx` directory per workspace crate.
///
/// The macros can't write straight into each crate's `.sqlx` because `--check` needs the same
/// layout without touching the checked-in data. Instead they write to a staging tree under
/// `target/`, keyed by package name, which we then move into place.
fn prepare_per_crate(ctx: &PrepareCtx<'_>) -> anyhow::Result<()> {
    let staging_root = ctx.metadata.target_directory().join("sqlx-prepare");
    let targets = reset_staging_dirs(&ctx.metadata, &staging_root)?;

    run_prepare_step(ctx, &staging_root)?;

    let mut prepared_crates = 0;
    let mut queries = 0;
    for target in &targets {
        let installed = install_crate_query_data(target)?;
        if installed > 0 {
            prepared_crates += 1;
            queries += installed;
        }
    }

    if queries == 0 {
        println!("{} no queries found", style("warning:").yellow());
        return Ok(());
    }

    println!(
        "query data for {} {} written to .sqlx in {} workspace {}; \
         please check this into version control",
        queries,
        if queries == 1 { "query" } else { "queries" },
        prepared_crates,
        if prepared_crates == 1 {
            "crate"
        } else {
            "crates"
        },
    );

    warn_about_stale_workspace_query_data(&ctx.metadata, &targets)?;

    Ok(())
}

/// Point out query data at the workspace root left behind by an earlier `--workspace` run.
///
/// The macros still fall back to it, so a query that only lives there will keep compiling even
/// though `--per-crate` no longer generates or checks it.
fn warn_about_stale_workspace_query_data(
    metadata: &Metadata,
    targets: &[CrateTarget],
) -> anyhow::Result<()> {
    if let Some(dir) = stale_workspace_query_data(metadata.workspace_root(), targets)? {
        println!(
            "{} {} still holds query data from `cargo sqlx prepare --workspace`; \
             it is no longer generated or checked, and can be deleted",
            style("warning:").yellow(),
            dir.display(),
        );
    }

    Ok(())
}

/// The workspace-root `.sqlx` directory, if it holds query data that no crate owns.
fn stale_workspace_query_data(
    workspace_root: &Path,
    targets: &[CrateTarget],
) -> anyhow::Result<Option<PathBuf>> {
    let workspace_dir = workspace_root.join(".sqlx");

    // In a workspace whose root is itself a package, this *is* that crate's own directory.
    if targets
        .iter()
        .any(|target| target.sqlx_dir == workspace_dir)
    {
        return Ok(None);
    }

    if glob_query_files(&workspace_dir)?.is_empty() {
        return Ok(None);
    }

    Ok(Some(workspace_dir))
}

async fn prepare_check(ctx: &PrepareCtx<'_>) -> anyhow::Result<()> {
    if ctx.connect_opts.database_url.is_some() {
        check_backend(ctx.config, &ctx.connect_opts).await?;
    }

    // Re-generate and store the queries in a separate directory from both the prepared
    // queries and the ones generated by `cargo check`, to avoid conflicts.
    let cache_dir = ctx.metadata.target_directory().join("sqlx-prepare-check");

    if ctx.per_crate {
        let targets = reset_staging_dirs(&ctx.metadata, &cache_dir)?;
        run_prepare_step(ctx, &cache_dir)?;

        let failures: Vec<String> = targets
            .iter()
            .filter_map(|target| {
                compare_query_data(&target.sqlx_dir, &target.staging_dir, Some(&target.name)).err()
            })
            .map(|err| err.to_string())
            .collect();

        if !failures.is_empty() {
            bail!("{}", failures.join("\n"));
        }

        return Ok(());
    }

    let prepare_dir = ctx.prepare_dir()?;
    run_prepare_step(ctx, &cache_dir)?;

    compare_query_data(&prepare_dir, &cache_dir, None)
}

/// Compare the checked-in query data in `prepare_dir` against freshly generated data in
/// `cache_dir`, erroring if the former is out of date.
fn compare_query_data(
    prepare_dir: &Path,
    cache_dir: &Path,
    crate_name: Option<&str>,
) -> anyhow::Result<()> {
    // Only used to attribute failures when checking a whole workspace crate by crate.
    let in_crate = crate_name
        .map(|name| format!(" in {name}"))
        .unwrap_or_default();

    // Compare .sqlx to cache.
    let prepare_filenames: HashSet<String> = glob_query_files(prepare_dir)?
        .into_iter()
        .filter_map(|path| path.file_name().map(|f| f.to_string_lossy().into_owned()))
        .collect();
    let cache_filenames: HashSet<String> = glob_query_files(cache_dir)?
        .into_iter()
        .filter_map(|path| path.file_name().map(|f| f.to_string_lossy().into_owned()))
        .collect();

    // Error: files in cache but not .sqlx.
    if cache_filenames
        .difference(&prepare_filenames)
        .next()
        .is_some()
    {
        bail!("prepare check failed: .sqlx{in_crate} is missing one or more queries; you should re-run sqlx prepare");
    }
    // Warn: files in .sqlx but not cache.
    if prepare_filenames
        .difference(&cache_filenames)
        .next()
        .is_some()
    {
        println!(
            "{} potentially unused queries found in .sqlx{in_crate}; you may want to re-run sqlx prepare",
            style("warning:").yellow()
        );
    }

    // Compare file contents as JSON to ignore superficial differences.
    // Everything in cache checked to be in .sqlx already.
    for filename in cache_filenames {
        let prepare_json = load_json_file(prepare_dir.join(&filename))?;
        let cache_json = load_json_file(cache_dir.join(&filename))?;
        if prepare_json != cache_json {
            bail!("prepare check failed: one or more query files differ{in_crate} ({filename}); you should re-run sqlx prepare");
        }
    }

    Ok(())
}

/// Enumerate the workspace crates and give each one an empty staging directory under
/// `staging_root`, discarding anything left over from a previous run.
fn reset_staging_dirs(
    metadata: &Metadata,
    staging_root: &Path,
) -> anyhow::Result<Vec<CrateTarget>> {
    match fs::remove_dir_all(staging_root) {
        Ok(()) => (),
        Err(err) if err.kind() == io::ErrorKind::NotFound => (),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to clear query staging directory: {}",
                    staging_root.display()
                )
            })
        }
    }

    let targets = crate_targets(metadata, staging_root);

    for target in &targets {
        fs::create_dir_all(&target.staging_dir).with_context(|| {
            format!(
                "failed to create query staging directory: {}",
                target.staging_dir.display()
            )
        })?;
    }

    Ok(targets)
}

/// Map each workspace member to its staging and `.sqlx` directories.
fn crate_targets(metadata: &Metadata, staging_root: &Path) -> Vec<CrateTarget> {
    metadata
        .workspace_members()
        .iter()
        .filter_map(|id| metadata.package(id))
        .map(|package| CrateTarget {
            name: package.name().to_owned(),
            staging_dir: staging_root.join(package.name()),
            sqlx_dir: package.manifest_dir().join(".sqlx"),
        })
        .collect()
}

/// Replace a crate's `.sqlx` contents with the query data staged for it, returning how many
/// queries were installed.
///
/// Only `query-*.json` files are touched, so anything else the user keeps in `.sqlx` survives.
fn install_crate_query_data(target: &CrateTarget) -> anyhow::Result<usize> {
    let staged = glob_query_files(&target.staging_dir)?;

    for stale in glob_query_files(&target.sqlx_dir)? {
        fs::remove_file(&stale)
            .with_context(|| format!("failed to delete query file: {}", stale.display()))?;
    }

    if staged.is_empty() {
        // The crate has no queries (anymore); don't leave an empty directory behind.
        if let Ok(mut entries) = fs::read_dir(&target.sqlx_dir) {
            if entries.next().is_none() {
                let _ = fs::remove_dir(&target.sqlx_dir);
            }
        }
        return Ok(0);
    }

    fs::create_dir_all(&target.sqlx_dir).with_context(|| {
        format!(
            "failed to create query cache directory: {}",
            target.sqlx_dir.display()
        )
    })?;

    for file in &staged {
        let file_name = file
            .file_name()
            .context("BUG: globbed query file has no file name")?;
        let dest = target.sqlx_dir.join(file_name);

        // `target/` may be on a different filesystem than the source tree, where rename fails.
        if fs::rename(file, &dest).is_err() {
            fs::copy(file, &dest).with_context(|| {
                format!(
                    "failed to move query file {} to {}",
                    file.display(),
                    dest.display()
                )
            })?;
        }
    }

    Ok(staged.len())
}

fn run_prepare_step(ctx: &PrepareCtx, cache_dir: &Path) -> anyhow::Result<()> {
    // Create and/or clean the directory.
    fs::create_dir_all(cache_dir).context(format!(
        "Failed to create query cache directory: {:?}",
        cache_dir
    ))?;

    // Create directory to hold temporary query files before they get persisted to SQLX_OFFLINE_DIR
    let tmp_dir = ctx.metadata.target_directory().join("sqlx-tmp");
    fs::create_dir_all(&tmp_dir).context(format!(
        "Failed to create temporary query cache directory: {:?}",
        tmp_dir
    ))?;

    // Only delete sqlx-*.json files to avoid accidentally deleting any user data.
    for query_file in glob_query_files(cache_dir).context("Failed to read query cache files")? {
        fs::remove_file(&query_file)
            .with_context(|| format!("Failed to delete query file: {}", query_file.display()))?;
    }

    // Try only triggering a recompile on crates that use `sqlx-macros` falling back to a full
    // clean on error
    setup_minimal_project_recompile(&ctx.cargo, &ctx.metadata, ctx.all, ctx.workspace)?;

    // Compile the queries.
    let check_status = {
        let mut check_command = Command::new(&ctx.cargo);
        check_command
            .arg("check")
            .args(&ctx.cargo_args)
            .env("SQLX_TMP", tmp_dir)
            .env("SQLX_OFFLINE", "false")
            .env("SQLX_OFFLINE_DIR", cache_dir);

        if ctx.per_crate {
            // Tells the macros to write into `<cache_dir>/<package name>` instead of `cache_dir`.
            check_command.env("SQLX_OFFLINE_PER_CRATE", "true");
        }

        if let Some(database_url) = &ctx.connect_opts.database_url {
            check_command.env("DATABASE_URL", database_url);
        }

        // `cargo check` recompiles on changed rust flags which can be set either via the env var
        // or through the `rustflags` field in `$CARGO_HOME/config` when the env var isn't set.
        // Because of this we only pass in `$RUSTFLAGS` when present.
        if let Ok(rustflags) = env::var("RUSTFLAGS") {
            check_command.env("RUSTFLAGS", rustflags);
        }

        check_command.status()?
    };
    if !check_status.success() {
        bail!("`cargo check` failed with status: {}", check_status);
    }

    Ok(())
}

#[derive(Debug, PartialEq)]
struct ProjectRecompileAction {
    // The names of the packages
    clean_packages: Vec<String>,
    touch_paths: Vec<PathBuf>,
}

/// Sets up recompiling only crates that depend on `sqlx-macros`
///
/// This gets a listing of all crates that depend on `sqlx-macros` (direct and transitive). The
/// crates within the current workspace have their source file's mtimes updated while crates
/// outside the workspace are selectively `cargo clean -p`ed. In this way we can trigger a
/// recompile of crates that may be using compile-time macros without forcing a full recompile.
///
/// If `workspace` is false, only the current package will have its files' mtimes updated.
fn setup_minimal_project_recompile(
    cargo: impl AsRef<OsStr>,
    metadata: &Metadata,
    all: bool,
    workspace: bool,
) -> anyhow::Result<()> {
    let recompile_action: ProjectRecompileAction = if workspace {
        minimal_project_recompile_action(metadata, all)
    } else {
        // Only touch the current crate.
        ProjectRecompileAction {
            clean_packages: Vec::new(),
            touch_paths: metadata.current_package()
                .context("failed to get package in current working directory, pass `--workspace` if running from a workspace root")?
                .src_paths()
                .to_vec(),
        }
    };

    if let Err(err) = minimal_project_clean(&cargo, recompile_action) {
        println!(
            "Failed minimal recompile setup. Cleaning entire project. Err: {}",
            err
        );
        let clean_status = Command::new(&cargo).arg("clean").status()?;
        if !clean_status.success() {
            bail!("`cargo clean` failed with status: {}", clean_status);
        }
    }

    Ok(())
}

fn minimal_project_clean(
    cargo: impl AsRef<OsStr>,
    action: ProjectRecompileAction,
) -> anyhow::Result<()> {
    let ProjectRecompileAction {
        clean_packages,
        touch_paths,
    } = action;

    // Update the modified timestamp of package files to force a selective recompilation.
    for file in touch_paths {
        let now = filetime::FileTime::now();
        filetime::set_file_times(&file, now, now)
            .with_context(|| format!("Failed to update mtime for {file:?}"))?;
    }

    // Clean entire packages.
    for pkg_id in &clean_packages {
        let clean_status = Command::new(&cargo)
            .args(["clean", "-p", pkg_id])
            .status()?;

        if !clean_status.success() {
            bail!("`cargo clean -p {}` failed", pkg_id);
        }
    }

    Ok(())
}

fn minimal_project_recompile_action(metadata: &Metadata, all: bool) -> ProjectRecompileAction {
    // Get all the packages that depend on `sqlx-macros`
    let mut sqlx_macros_dependents = BTreeSet::new();
    let sqlx_macros_ids: BTreeSet<_> = metadata
        .entries()
        // We match just by name instead of name and url because some people may have it installed
        // through different means like vendoring
        .filter(|(_, package)| package.name() == "sqlx-macros")
        .map(|(id, _)| id)
        .collect();
    for sqlx_macros_id in sqlx_macros_ids {
        sqlx_macros_dependents.extend(metadata.all_dependents_of(sqlx_macros_id));
    }

    // Figure out which `sqlx-macros` dependents are in the workspace vs out
    let mut in_workspace_dependents = Vec::new();
    let mut out_of_workspace_dependents = Vec::new();
    for dependent in sqlx_macros_dependents {
        if metadata.workspace_members().contains(dependent) {
            in_workspace_dependents.push(dependent);
        } else {
            out_of_workspace_dependents.push(dependent);
        }
    }

    // In-workspace dependents have their source file's mtime updated.
    let files_to_touch: Vec<_> = in_workspace_dependents
        .iter()
        .filter_map(|id| {
            metadata
                .package(id)
                .map(|package| package.src_paths().to_owned())
        })
        .flatten()
        .collect();

    // Out-of-workspace get `cargo clean -p <PKGID>`ed, only if --all is set.
    let packages_to_clean: Vec<_> = if all {
        out_of_workspace_dependents
            .iter()
            .filter_map(|id| {
                metadata
                    .package(id)
                    .map(|package| package.name().to_owned())
            })
            // Do not clean sqlx, it depends on sqlx-macros but has no queries to prepare itself.
            .filter(|name| name != "sqlx")
            .collect()
    } else {
        Vec::new()
    };

    ProjectRecompileAction {
        clean_packages: packages_to_clean,
        touch_paths: files_to_touch,
    }
}

/// Find all `query-*.json` files in a directory.
fn glob_query_files(path: impl AsRef<Path>) -> anyhow::Result<Vec<PathBuf>> {
    let path = path.as_ref();
    let pattern = path.join("query-*.json");
    glob::glob(
        pattern
            .to_str()
            .context("query cache path is invalid UTF-8")?,
    )
    .with_context(|| format!("failed to read query cache path: {}", path.display()))?
    .collect::<Result<Vec<_>, _>>()
    .context("glob failed")
}

/// Load the JSON contents of a query data file.
fn load_json_file(path: impl AsRef<Path>) -> anyhow::Result<serde_json::Value> {
    let path = path.as_ref();
    let file_bytes =
        fs::read(path).with_context(|| format!("failed to load file: {}", path.display()))?;
    Ok(serde_json::from_slice(&file_bytes)?)
}

async fn check_backend(config: &Config, opts: &ConnectOpts) -> anyhow::Result<()> {
    crate::connect(config, opts).await?.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_eq;
    use tempfile::TempDir;

    fn sample_metadata() -> anyhow::Result<Metadata> {
        let sample_metadata_path = Path::new("tests")
            .join("assets")
            .join("sample_metadata.json");
        let sample_metadata = std::fs::read_to_string(sample_metadata_path)?;
        sample_metadata.parse()
    }

    /// Write a query data file with the given hash, creating `dir` if needed.
    fn write_query_file(dir: &Path, hash: &str, contents: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("query-{hash}.json")), contents).unwrap();
    }

    fn query_file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = glob_query_files(dir)
            .unwrap()
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A target whose staging and `.sqlx` directories both live under `root`.
    fn crate_target_in(root: &Path) -> CrateTarget {
        CrateTarget {
            name: "my-api".into(),
            staging_dir: root.join("staging").join("my-api"),
            sqlx_dir: root.join("my-api").join(".sqlx"),
        }
    }

    #[test]
    fn minimal_project_recompile_action_works() -> anyhow::Result<()> {
        let metadata = sample_metadata()?;

        let action = minimal_project_recompile_action(&metadata, false);
        assert_eq!(
            action,
            ProjectRecompileAction {
                clean_packages: vec![],
                touch_paths: vec![
                    "/home/user/problematic/workspace/b_in_workspace_lib/src/lib.rs".into(),
                    "/home/user/problematic/workspace/c_in_workspace_bin/src/main.rs".into(),
                ],
            }
        );

        Ok(())
    }

    #[test]
    fn crate_targets_cover_every_workspace_member() -> anyhow::Result<()> {
        let metadata = sample_metadata()?;
        let staging_root = Path::new("/home/user/problematic/workspace/target/sqlx-prepare");

        let targets = crate_targets(&metadata, staging_root);

        assert_eq!(
            targets,
            vec![
                CrateTarget {
                    name: "b_in_workspace_lib".into(),
                    staging_dir: staging_root.join("b_in_workspace_lib"),
                    sqlx_dir: "/home/user/problematic/workspace/b_in_workspace_lib/.sqlx".into(),
                },
                CrateTarget {
                    name: "c_in_workspace_bin".into(),
                    staging_dir: staging_root.join("c_in_workspace_bin"),
                    sqlx_dir: "/home/user/problematic/workspace/c_in_workspace_bin/.sqlx".into(),
                },
            ]
        );

        Ok(())
    }

    #[test]
    fn reset_staging_dirs_starts_from_a_clean_tree() -> anyhow::Result<()> {
        let metadata = sample_metadata()?;
        let tempdir = TempDir::new()?;
        let staging_root = tempdir.path().join("sqlx-prepare");

        // Left over from a previous run, for a crate that no longer exists.
        write_query_file(&staging_root.join("deleted_crate"), "stale", "{}");

        let targets = reset_staging_dirs(&metadata, &staging_root)?;

        assert!(!staging_root.join("deleted_crate").exists());
        for target in &targets {
            assert!(
                target.staging_dir.is_dir(),
                "{} was not created",
                target.staging_dir.display()
            );
        }

        Ok(())
    }

    #[test]
    fn install_crate_query_data_replaces_stale_queries() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let target = crate_target_in(tempdir.path());

        write_query_file(&target.staging_dir, "aaa", r#"{"query":"SELECT 1"}"#);
        write_query_file(&target.staging_dir, "bbb", r#"{"query":"SELECT 2"}"#);

        write_query_file(&target.sqlx_dir, "ccc", r#"{"query":"SELECT 3"}"#);
        // Anything that isn't query data is the user's, and must be left alone.
        fs::write(target.sqlx_dir.join("README.md"), "hello")?;

        assert_eq!(install_crate_query_data(&target)?, 2);

        assert_eq!(
            query_file_names(&target.sqlx_dir),
            ["query-aaa.json", "query-bbb.json"]
        );
        assert_eq!(
            fs::read_to_string(target.sqlx_dir.join("query-aaa.json"))?,
            r#"{"query":"SELECT 1"}"#
        );
        assert_eq!(
            fs::read_to_string(target.sqlx_dir.join("README.md"))?,
            "hello"
        );

        Ok(())
    }

    #[test]
    fn install_crate_query_data_creates_missing_sqlx_dir() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let target = crate_target_in(tempdir.path());

        write_query_file(&target.staging_dir, "aaa", r#"{"query":"SELECT 1"}"#);
        assert!(!target.sqlx_dir.exists());

        assert_eq!(install_crate_query_data(&target)?, 1);
        assert_eq!(query_file_names(&target.sqlx_dir), ["query-aaa.json"]);

        Ok(())
    }

    #[test]
    fn install_crate_query_data_removes_emptied_sqlx_dir() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let target = crate_target_in(tempdir.path());

        fs::create_dir_all(&target.staging_dir)?;
        // The crate's last query was deleted since the previous `prepare`.
        write_query_file(&target.sqlx_dir, "ccc", r#"{"query":"SELECT 3"}"#);

        assert_eq!(install_crate_query_data(&target)?, 0);
        assert!(!target.sqlx_dir.exists());

        Ok(())
    }

    #[test]
    fn install_crate_query_data_keeps_sqlx_dir_holding_other_files() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let target = crate_target_in(tempdir.path());

        fs::create_dir_all(&target.staging_dir)?;
        write_query_file(&target.sqlx_dir, "ccc", r#"{"query":"SELECT 3"}"#);
        fs::write(target.sqlx_dir.join("README.md"), "hello")?;

        assert_eq!(install_crate_query_data(&target)?, 0);
        assert!(query_file_names(&target.sqlx_dir).is_empty());
        assert!(target.sqlx_dir.join("README.md").exists());

        Ok(())
    }

    #[test]
    fn stale_workspace_query_data_is_reported() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let target = crate_target_in(tempdir.path());

        // Left behind by a previous `cargo sqlx prepare --workspace`.
        write_query_file(
            &tempdir.path().join(".sqlx"),
            "aaa",
            r#"{"query":"SELECT 1"}"#,
        );

        assert_eq!(
            stale_workspace_query_data(tempdir.path(), &[target])?,
            Some(tempdir.path().join(".sqlx"))
        );

        Ok(())
    }

    #[test]
    fn workspace_query_data_owned_by_the_root_crate_is_not_stale() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        // A workspace whose root directory is itself a package.
        let target = CrateTarget {
            name: "my-api".into(),
            staging_dir: tempdir.path().join("staging").join("my-api"),
            sqlx_dir: tempdir.path().join(".sqlx"),
        };

        write_query_file(&target.sqlx_dir, "aaa", r#"{"query":"SELECT 1"}"#);

        assert_eq!(stale_workspace_query_data(tempdir.path(), &[target])?, None);

        Ok(())
    }

    #[test]
    fn empty_workspace_query_data_is_not_reported() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let target = crate_target_in(tempdir.path());

        assert_eq!(stale_workspace_query_data(tempdir.path(), &[target])?, None);

        Ok(())
    }

    #[test]
    fn compare_query_data_accepts_up_to_date_data() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let (prepare_dir, cache_dir) = (tempdir.path().join(".sqlx"), tempdir.path().join("cache"));

        write_query_file(&prepare_dir, "aaa", r#"{"query": "SELECT 1"}"#);
        // Formatting differences are ignored; the JSON is what matters.
        write_query_file(&cache_dir, "aaa", r#"{"query":"SELECT 1"}"#);

        compare_query_data(&prepare_dir, &cache_dir, None)?;

        Ok(())
    }

    #[test]
    fn compare_query_data_tolerates_unused_queries() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let (prepare_dir, cache_dir) = (tempdir.path().join(".sqlx"), tempdir.path().join("cache"));

        write_query_file(&prepare_dir, "aaa", r#"{"query":"SELECT 1"}"#);
        write_query_file(&prepare_dir, "bbb", r#"{"query":"SELECT 2"}"#);
        write_query_file(&cache_dir, "aaa", r#"{"query":"SELECT 1"}"#);

        // Only a warning: the query may still be used by another crate or feature combination.
        compare_query_data(&prepare_dir, &cache_dir, None)?;

        Ok(())
    }

    #[test]
    fn compare_query_data_rejects_missing_queries() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let (prepare_dir, cache_dir) = (tempdir.path().join(".sqlx"), tempdir.path().join("cache"));

        write_query_file(&cache_dir, "aaa", r#"{"query":"SELECT 1"}"#);

        let err = compare_query_data(&prepare_dir, &cache_dir, Some("my-api"))
            .expect_err("missing query data should fail the check");
        assert!(
            err.to_string().contains(".sqlx in my-api is missing"),
            "{err}"
        );

        Ok(())
    }

    #[test]
    fn compare_query_data_rejects_differing_queries() -> anyhow::Result<()> {
        let tempdir = TempDir::new()?;
        let (prepare_dir, cache_dir) = (tempdir.path().join(".sqlx"), tempdir.path().join("cache"));

        write_query_file(&prepare_dir, "aaa", r#"{"query":"SELECT 1"}"#);
        write_query_file(&cache_dir, "aaa", r#"{"query":"SELECT 2"}"#);

        let err = compare_query_data(&prepare_dir, &cache_dir, Some("my-api"))
            .expect_err("outdated query data should fail the check");
        assert!(
            err.to_string()
                .contains("query files differ in my-api (query-aaa.json)"),
            "{err}"
        );

        Ok(())
    }
}
