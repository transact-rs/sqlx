use crate::error::{BoxDynError, Error};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    #[error("while executing migrations: {0}")]
    Execute(#[from] Error),

    #[error("while executing migration {1}: {0}")]
    ExecuteMigration(#[source] Error, i64),

    #[error("while resolving migrations: {0}")]
    Source(#[source] BoxDynError),

    #[error("migration {0} was previously applied but is missing in the resolved migrations")]
    VersionMissing(i64),

    #[error("migration {0} was previously applied but has been modified")]
    VersionMismatch(i64),

    #[error("migration {0} is not present in the migration source")]
    VersionNotPresent(i64),

    #[error("migration {0} is older than the latest applied migration {1}")]
    VersionTooOld(i64, i64),

    #[error("migration {0} is newer than the latest applied migration {1}")]
    VersionTooNew(i64, i64),

    #[error("database driver does not support force-dropping a database (Only PostgreSQL)")]
    ForceNotSupported,

    #[deprecated = "migration types are now inferred"]
    #[error("cannot mix reversible migrations with simple migrations. All migrations should be reversible or simple migrations")]
    InvalidMixReversibleAndSimple,

    // NOTE: this will only happen with a database that does not have transactional DDL (.e.g, MySQL or Oracle)
    #[error(
        "migration {0} is partially applied; fix and remove row from `_sqlx_migrations` table"
    )]
    Dirty(i64),

    #[error("database driver does not support creation of schemas at migrate time: {0}")]
    CreateSchemasNotSupported(String),

    /// The migrations table exists, but not where an unqualified reference to it resolves.
    ///
    /// Currently only returned by the PostgreSQL driver, where an unqualified name is resolved
    /// through `search_path`.
    #[error(
        "cannot migrate: `{table_name}` does not exist in the default schema `{default_schema}`, \
         but does exist at: {}.\n\n\
         This suggests that the search path for the current user or database has changed since \
         the migrations table was created. This may happen explicitly, or implicitly if a schema \
         is created with the same name as the user \
         (https://www.postgresql.org/docs/current/ddl-schemas.html#DDL-SCHEMAS-PATH).\n\n\
         Since SQLx cannot know which of the existing `{table_name}` tables is correct for the \
         current context, migration cannot continue.\n\n\
         To resolve this ambiguity, either change the default search path for this database \
         (using `ALTER DATABASE`), or create `{default_schema}.{table_name}` with the correct \
         set of migrations.",
        .other_schemas
            .iter()
            .map(|schema| format!("`{schema}.{table_name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )]
    AmbiguousMigrationsTable {
        /// The unqualified name of the migrations table.
        table_name: String,
        /// The schema an unqualified reference to `table_name` currently resolves to.
        default_schema: String,
        /// The schemas in the search path that do contain `table_name`, in priority order.
        other_schemas: Vec<String>,
    },

    #[error("database driver does not support skipping migrations")]
    SkipNotSupported(),
}
