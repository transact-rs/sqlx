#![cfg(feature = "_sqlite")]

use assert_cmd::{cargo_bin_cmd, Command};
use std::fs;
use tempfile::TempDir;

struct TestCli {
    dir: TempDir,
    cargo: bool,
}

impl TestCli {
    fn new(cargo: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("migrations")).unwrap();
        fs::write(
            dir.path().join("migrations/1_logging_test.up.sql"),
            "CREATE TABLE logging_test (id BIGINT);",
        )
        .unwrap();
        fs::write(
            dir.path().join("migrations/1_logging_test.down.sql"),
            "DROP TABLE logging_test;",
        )
        .unwrap();
        Self { dir, cargo }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = if self.cargo {
            let mut cmd = cargo_bin_cmd!("cargo-sqlx");
            cmd.arg("sqlx");
            cmd
        } else {
            cargo_bin_cmd!("sqlx")
        };
        cmd.current_dir(self.dir.path())
            .env_remove("RUST_LOG")
            .env_remove("DATABASE_URL")
            .args(args)
            .arg("--database-url")
            .arg(format!(
                "sqlite://{}?mode=rwc",
                self.dir.path().join("test.db").display()
            ));
        cmd
    }
}

#[test]
fn migration_logging_in_both_binaries() {
    for cargo in [false, true] {
        let cli = TestCli::new(cargo);
        let output = cli
            .command(&["migrate", "run"])
            .env("RUST_LOG", "sqlx::migrate=info")
            .assert()
            .success()
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("starting migration"), "{stderr}");
        assert!(stderr.contains("migration completed"), "{stderr}");
        assert!(stderr.contains("version=1"), "{stderr}");
        assert!(stderr.contains("description=logging test"), "{stderr}");
        assert!(stderr.contains("direction=\"up\""), "{stderr}");
        assert!(stderr.contains("elapsed_secs="), "{stderr}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Applied"), "{stdout}");
        assert!(!stdout.contains("starting migration"), "{stdout}");

        // Already applied migrations and dry runs do not log execution.
        for args in [
            vec!["migrate", "run"],
            vec!["migrate", "revert", "--dry-run"],
        ] {
            cli.command(&args)
                .env("RUST_LOG", "sqlx::migrate=info")
                .assert()
                .success()
                .stderr("");
        }

        let output = cli
            .command(&["migrate", "revert"])
            .env("RUST_LOG", "sqlx::migrate=info")
            .assert()
            .success()
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("starting migration"), "{stderr}");
        assert!(stderr.contains("migration completed"), "{stderr}");
        assert!(stderr.contains("direction=\"down\""), "{stderr}");

        for args in [
            vec!["migrate", "run", "--dry-run"],
            vec!["migrate", "override", "skip"],
        ] {
            cli.command(&args)
                .env("RUST_LOG", "sqlx::migrate=info")
                .assert()
                .success()
                .stderr("");
        }
    }
}

#[test]
fn default_filter_and_dotenv() {
    for cargo in [false, true] {
        let cli = TestCli::new(cargo);
        cli.command(&["migrate", "run"])
            .assert()
            .success()
            .stderr("");

        fs::write(cli.dir.path().join(".env"), "RUST_LOG=sqlx::migrate=info\n").unwrap();
        let output = cli
            .command(&["migrate", "revert"])
            .assert()
            .success()
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("starting migration"), "{stderr}");

        cli.command(&["migrate", "run", "--no-dotenv"])
            .assert()
            .success()
            .stderr("");
        cli.command(&["migrate", "revert"])
            .env("RUST_LOG", "off")
            .assert()
            .success()
            .stderr("");
    }
}

#[test]
fn failed_migrations_are_logged_by_default() {
    for cargo in [false, true] {
        let cli = TestCli::new(cargo);
        fs::write(
            cli.dir.path().join("migrations/1_logging_test.up.sql"),
            "SELECT * FROM missing_migration_logging_table;",
        )
        .unwrap();
        let output = cli
            .command(&["migrate", "run"])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("migration failed"), "{stderr}");
        assert!(stderr.contains("version=1"), "{stderr}");
        assert!(
            stderr.contains("missing_migration_logging_table"),
            "{stderr}"
        );
        assert!(!stderr.contains("starting migration"), "{stderr}");
        assert!(!stderr.contains("migration completed"), "{stderr}");
    }
}
