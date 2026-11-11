use sqlx::migrate::{Migrate, MigrateError, Migration, MigrationType, Migrator};
use sqlx::SqlSafeStr;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::instrument::WithSubscriber;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

#[derive(Debug, Default)]
struct RecordedEvent {
    fields: BTreeMap<String, String>,
    level: Option<Level>,
}

impl Visit for RecordedEvent {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().into(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().into(), value.into());
    }
}

#[derive(Clone, Default)]
struct MigrationEvents(Arc<Mutex<Vec<RecordedEvent>>>);

impl<S: Subscriber> Layer<S> for MigrationEvents {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "sqlx::migrate" {
            let mut recorded = RecordedEvent {
                level: Some(*event.metadata().level()),
                ..RecordedEvent::default()
            };
            event.record(&mut recorded);
            self.0.lock().unwrap().push(recorded);
        }
    }
}

impl MigrationEvents {
    async fn record<F: Future>(&self, future: F) -> F::Output {
        future
            .with_subscriber(tracing_subscriber::registry().with(self.clone()))
            .await
    }

    fn assert_migration(&self, version: i64, direction: &str, success: bool) {
        let events = std::mem::take(&mut *self.0.lock().unwrap());
        assert_eq!(events.len(), 2, "{events:?}");
        for event in &events {
            assert_eq!(event.fields["version"], version.to_string());
            assert_eq!(event.fields["description"], "logging test");
            assert_eq!(event.fields["direction"], direction);
            assert!(!event.fields.contains_key("sql"));
        }
        assert_eq!(events[0].level, Some(Level::INFO));
        assert_eq!(events[0].fields["message"], "starting migration");
        if success {
            assert_eq!(events[1].level, Some(Level::INFO));
            assert_eq!(events[1].fields["message"], "migration completed");
            assert!(events[1].fields["elapsed_secs"].parse::<f64>().unwrap() >= 0.0);
            assert!(events[1].fields.contains_key("elapsed"));
        } else {
            assert_eq!(events[1].level, Some(Level::ERROR));
            assert_eq!(events[1].fields["message"], "migration failed");
            assert!(!events[1].fields["error"].is_empty());
        }
    }

    fn assert_empty(&self) {
        assert!(self.0.lock().unwrap().is_empty());
    }
}

fn migration(
    version: i64,
    migration_type: MigrationType,
    sql: &'static str,
    no_tx: bool,
) -> Migration {
    Migration::new(
        version,
        "logging test".into(),
        migration_type,
        sql.into_sql_str(),
        no_tx,
    )
}

pub async fn migrations_are_logged<C>(conn: &mut C, no_tx: bool) -> anyhow::Result<()>
where
    C: Migrate + sqlx::Connection,
    for<'a> &'a mut C: sqlx::Acquire<'a, Connection = &'a mut C>,
{
    let events = MigrationEvents::default();
    let migrator = Migrator::with_migrations(vec![
        migration(
            1,
            MigrationType::ReversibleUp,
            "CREATE TABLE migration_logging_test (id BIGINT)",
            no_tx,
        ),
        migration(
            1,
            MigrationType::ReversibleDown,
            "DROP TABLE migration_logging_test",
            no_tx,
        ),
        migration(
            2,
            MigrationType::ReversibleUp,
            "INSERT INTO migration_logging_test VALUES (2)",
            no_tx,
        ),
        migration(
            2,
            MigrationType::ReversibleDown,
            "DELETE FROM migration_logging_test WHERE id = 2",
            no_tx,
        ),
    ]);

    events.record(migrator.run_to(1, &mut *conn)).await?;
    events.assert_migration(1, "up", true);
    events.record(migrator.run_to(1, &mut *conn)).await?;
    events.assert_empty();

    events.record(migrator.run(&mut *conn)).await?;
    events.assert_migration(2, "up", true);
    events.record(migrator.run(&mut *conn)).await?;
    events.assert_empty();

    events.record(migrator.undo(&mut *conn, 2)).await?;
    events.assert_empty();
    events.record(migrator.undo(&mut *conn, 1)).await?;
    events.assert_migration(2, "down", true);
    events.record(migrator.undo(&mut *conn, 0)).await?;
    events.assert_migration(1, "down", true);

    events.record(migrator.skip(&mut *conn, Some(1))).await?;
    events.assert_empty();

    // Validation failures do not claim that a migration is being executed.
    let mismatched =
        Migrator::with_migrations(vec![migration(1, MigrationType::Simple, "SELECT 1", no_tx)]);
    assert!(matches!(
        events.record(mismatched.run(&mut *conn)).await,
        Err(MigrateError::VersionMismatch(1))
    ));
    events.assert_empty();

    // Direct driver calls also log failures, including through AnyConnection in the CLI.
    let invalid = migration(
        3,
        MigrationType::Simple,
        "SELECT * FROM missing_migration_logging_table",
        no_tx,
    );
    assert!(events
        .record(conn.apply("_sqlx_migrations", &invalid))
        .await
        .is_err());
    events.assert_migration(3, "up", false);

    let invalid = migration(
        1,
        MigrationType::ReversibleDown,
        "SELECT * FROM missing_migration_logging_table",
        no_tx,
    );
    assert!(events
        .record(conn.revert("_sqlx_migrations", &invalid))
        .await
        .is_err());
    events.assert_migration(1, "down", false);

    Ok(())
}

#[sqlx::test]
async fn start_is_logged_before_execution_is_polled() {
    let events = MigrationEvents::default();
    let migration = migration(1, MigrationType::Simple, "SELECT 1", false);
    events
        .record(sqlx_core::logger::log_migration(&migration, async {
            let recorded = events.0.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].fields["message"], "starting migration");
            Ok(std::time::Duration::ZERO)
        }))
        .await
        .unwrap();
    events.assert_migration(1, "up", true);
}
