use assert_cmd::{assert::Assert, Command};

use sqlx::_unstable::config::Config;
use sqlx::{migrate::Migrate, Connection, SqliteConnection};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub struct TestDatabase {
    file_path: PathBuf,
    migrations_path: PathBuf,
    pub config_path: Option<PathBuf>,
}

pub enum MigrateCommand {
    Run,
    Revert,
    Skip,
}

impl AsRef<str> for MigrateCommand {
    fn as_ref(&self) -> &str {
        match self {
            MigrateCommand::Run => "run",
            MigrateCommand::Revert => "revert",
            MigrateCommand::Skip => "override skip",
        }
    }
}

impl TestDatabase {
    pub fn new(name: &str, migrations: &str) -> Self {
        // Note: only set when _building_
        let temp_dir = option_env!("CARGO_TARGET_TMPDIR").map_or_else(env::temp_dir, PathBuf::from);

        let test_dir = temp_dir.join("migrate");

        fs::create_dir_all(&test_dir)
            .unwrap_or_else(|e| panic!("error creating directory: {test_dir:?}: {e}"));

        let file_path = test_dir.join(format!("test-{name}.db"));

        if file_path.exists() {
            fs::remove_file(&file_path)
                .unwrap_or_else(|e| panic!("error deleting test database {file_path:?}: {e}"));
        }

        let this = Self {
            file_path,
            migrations_path: Path::new("tests").join(migrations),
            config_path: None,
        };

        Command::cargo_bin("cargo-sqlx")
            .unwrap()
            .args([
                "sqlx",
                "database",
                "create",
                "--database-url",
                &this.connection_string(),
            ])
            .assert()
            .success();
        this
    }

    pub fn set_migrations(&mut self, migrations: &str) {
        self.migrations_path = Path::new("tests").join(migrations);
    }

    pub fn connection_string(&self) -> String {
        format!("sqlite://{}", self.file_path.display())
    }

    pub fn run_migration(
        &self,
        migrate_command: MigrateCommand,
        version: Option<i64>,
        dry_run: bool,
    ) -> Assert {
        let mut command = Command::cargo_bin("sqlx").unwrap();
        command
            .arg("migrate")
            .args(migrate_command.as_ref().split_whitespace())
            .args(["--database-url", &self.connection_string(), "--source"])
            .arg(&self.migrations_path);

        if let Some(config_path) = &self.config_path {
            command.arg("--config").arg(config_path);
        }

        if let Some(version) = version {
            command.arg("--target-version").arg(version.to_string());
        }

        if dry_run {
            command.arg("--dry-run");
        }

        command.assert()
    }

    pub async fn applied_migrations(&self) -> Vec<i64> {
        let mut conn = SqliteConnection::connect(&self.connection_string())
            .await
            .unwrap();

        let config = Config::default();

        conn.list_applied_migrations(config.migrate.table_name())
            .await
            .unwrap()
            .iter()
            .map(|m| m.version)
            .collect()
    }

    pub fn migrate_info(&self) -> Assert {
        let mut command = Command::cargo_bin("sqlx").unwrap();
        command
            .args([
                "migrate",
                "info",
                "--database-url",
                &self.connection_string(),
                "--source",
            ])
            .arg(&self.migrations_path);

        if let Some(config_path) = &self.config_path {
            command.arg("--config").arg(config_path);
        }

        command.assert()
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Only remove the database if there isn't a failure.
        if !std::thread::panicking() {
            fs::remove_file(&self.file_path).unwrap_or_else(|e| {
                panic!("error deleting test database {:?}: {e}", self.file_path)
            });
        }
    }
}
