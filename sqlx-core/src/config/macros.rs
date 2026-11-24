use std::collections::BTreeMap;

/// Configuration for the `query!()` family of macros.
///
/// See also [`common::Config`][crate::config::common::Config] for renaming `DATABASE_URL`.
#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case", deny_unknown_fields)
)]
pub struct Config {
    /// Specify which crates' types to use when types from multiple crates apply.
    ///
    /// See [`PreferredCrates`] for details.
    pub preferred_crates: PreferredCrates,

    /// Specify global overrides for mapping SQL type names to Rust type names.
    ///
    /// Default type mappings are defined by the database driver.
    /// Refer to the `sqlx::types` module for details.
    ///
    /// ## Note: Case-Sensitive
    /// Currently, the case of the type name MUST match the name SQLx knows it by.
    /// Built-in types are spelled in all-uppercase to match SQL convention.
    ///
    /// However, user-created types in Postgres are all-lowercase unless quoted.
    ///
    /// ## Note: Orthogonal to Nullability
    /// These overrides do not affect whether `query!()` decides to wrap a column in `Option<_>`
    /// or not. They only override the inner type used.
    ///
    /// ## Note: Schema Qualification (Postgres)
    /// Type names may be schema-qualified in Postgres. If so, the schema should be part
    /// of the type string, e.g. `'foo.bar'` to reference type `bar` in schema `foo`.
    ///
    /// The schema and/or type name may additionally be quoted in the string
    /// for a quoted identifier (see next section).
    ///
    /// Schema qualification should not be used for types in the search path.
    ///
    /// ## Note: Quoted Identifiers (Postgres)
    /// Type names using [quoted identifiers in Postgres] must also be specified with quotes here.
    ///
    /// Note, however, that the TOML format parses way the outer pair of quotes,
    /// so for quoted names in Postgres, double-quoting is necessary,
    /// e.g. `'"Foo"'` for SQL type `"Foo"`.
    ///
    /// To reference a schema-qualified type with a quoted name, use double-quotes after the
    /// dot, e.g. `'foo."Bar"'` to reference type `"Bar"` of schema `foo`, and vice versa for
    /// quoted schema names.
    ///
    /// We recommend wrapping all type names in single quotes, as shown below,
    /// to avoid confusion.
    ///
    /// MySQL/MariaDB and SQLite do not support custom types, so quoting type names should
    /// never be necessary.
    ///
    /// [quoted identifiers in Postgres]: https://www.postgresql.org/docs/current/sql-syntax-lexical.html#SQL-SYNTAX-IDENTIFIERS
    // Note: we wanted to be able to handle this intelligently,
    // but the `toml` crate authors weren't interested: https://github.com/toml-rs/toml/issues/761
    //
    // We decided to just encourage always quoting type names instead.
    /// Example: Custom Wrapper Types
    /// -------
    /// Does SQLx not support a type that you need? Do you want additional semantics not
    /// implemented on the built-in types? You can create a custom wrapper,
    /// or use an external crate.
    ///
    /// #### `sqlx.toml`
    /// ```toml
    /// [macros.type-overrides]
    /// # Override a built-in type
    /// 'UUID' = "crate::types::MyUuid"
    ///
    /// # Support an external or custom wrapper type (e.g. from the `isn` Postgres extension)
    /// # (NOTE: FOR DOCUMENTATION PURPOSES ONLY; THIS CRATE/TYPE DOES NOT EXIST AS OF WRITING)
    /// 'isbn13' = "isn_rs::sqlx::ISBN13"
    /// ```
    ///
    /// Example: Custom Types in Postgres
    /// -------
    /// If you have a custom type in Postgres that you want to map without needing to use
    /// the type override syntax in `sqlx::query!()` every time, you can specify a global
    /// override here.
    ///
    /// For example, a custom enum type `foo`:
    ///
    /// #### Migration or Setup SQL (e.g. `migrations/0_setup.sql`)
    /// ```sql
    /// CREATE TYPE foo AS ENUM ('Bar', 'Baz');
    /// ```
    ///
    /// #### `src/types.rs`
    /// ```rust,no_run
    /// #[derive(sqlx::Type)]
    /// pub enum Foo {
    ///     Bar,
    ///     Baz
    /// }
    /// ```
    ///
    /// If you're not using `PascalCase` in your enum variants then you'll want to use
    /// `#[sqlx(rename_all = "<strategy>")]` on your enum.
    /// See [`Type`][crate::type::Type] for details.
    ///
    /// #### `sqlx.toml`
    /// ```toml
    /// [macros.type-overrides]
    /// # Map SQL type `foo` to `crate::types::Foo`
    /// 'foo' = "crate::types::Foo"
    /// ```
    ///
    /// Example: Schema-Qualified Types
    /// -------
    /// (See `Note` section above for details.)
    ///
    /// ```toml
    /// [macros.type-overrides]
    /// # Map SQL type `foo.foo` to `crate::types::Foo`
    /// 'foo.foo' = "crate::types::Foo"
    /// ```
    ///
    /// Example: Quoted Identifiers
    /// -------
    /// If a type or schema uses quoted identifiers,
    /// it must be wrapped in quotes _twice_ for SQLx to know the difference:
    ///
    /// ```toml
    /// [macros.type-overrides]
    /// # `"Foo"` in SQLx
    /// '"Foo"' = "crate::types::Foo"
    /// # **NOT** `"Foo"` in SQLx (parses as just `Foo`)
    /// "Foo" = "crate::types::Foo"
    ///
    /// # Schema-qualified
    /// '"foo".foo' = "crate::types::Foo"
    /// 'foo."Foo"' = "crate::types::Foo"
    /// '"foo"."Foo"' = "crate::types::Foo"
    /// ```
    ///
    /// (See `Note` section above for details.)
    // TODO: allow specifying different types for input vs output
    // e.g. to accept `&[T]` on input but output `Vec<T>`
    pub type_overrides: BTreeMap<SqlType, RustType>,

    /// Specify per-table and per-column overrides for mapping SQL types to Rust types.
    ///
    /// Default type mappings are defined by the database driver.
    /// Refer to the `sqlx::types` module for details.
    ///
    /// The supported syntax is similar to [`type_overrides`][Self::type_overrides],
    /// (with the same caveat for quoted names!) but column names must be qualified
    /// by a separately quoted table name, which may optionally be schema-qualified.
    ///
    /// Multiple columns for the same SQL table may be written in the same table in TOML
    /// (see examples below).
    ///
    /// ## Note: Orthogonal to Nullability
    /// These overrides do not affect whether `query!()` decides to wrap a column in `Option<_>`
    /// or not. They only override the inner type used.
    ///
    /// ## Note: Schema Qualification
    /// Table names may be schema-qualified. If so, the schema should be part
    /// of the table name string, e.g. `'foo.bar'` to reference table `bar` in schema `foo`.
    ///
    /// The schema and/or type name may additionally be quoted in the string
    /// for a quoted identifier (see next section).
    ///
    /// Postgres users: schema qualification should not be used for tables in the search path.
    ///
    /// ## Note: Quoted Identifiers
    /// Schema, table, or column names using quoted identifiers ([MySQL], [Postgres], [SQLite])
    /// in SQL must also be specified with quotes here.
    ///
    /// Postgres and SQLite use double-quotes (`"Foo"`) while MySQL uses backticks (`\`Foo\`).
    ///
    /// Note, however, that the TOML format parses way the outer pair of quotes,
    /// so for quoted names in Postgres, double-quoting is necessary,
    /// e.g. `'"Foo"'` for SQL name `"Foo"`.
    ///
    /// To reference a schema-qualified table with a quoted name, use the appropriate quotation
    /// characters after the dot, e.g. `'foo."Bar"'` to reference table `"Bar"` of schema `foo`,
    /// and vice versa for quoted schema names.
    ///
    /// We recommend wrapping all table and column names in single quotes, as shown below,
    /// to avoid confusion.
    ///
    /// [MySQL]: https://dev.mysql.com/doc/refman/8.4/en/identifiers.html
    /// [Postgres]: https://www.postgresql.org/docs/current/sql-syntax-lexical.html#SQL-SYNTAX-IDENTIFIERS
    /// [SQLite]: https://sqlite.org/lang_keywords.html
    // Note: we wanted to be able to handle this intelligently,
    // but the `toml` crate authors weren't interested: https://github.com/toml-rs/toml/issues/761
    //
    // We decided to just encourage always quoting type names instead.
    ///
    /// Example
    /// -------
    ///
    /// #### `sqlx.toml`
    /// ```toml
    /// [macros.table-overrides.'foo']
    /// # Map column `bar` of table `foo` to Rust type `crate::types::Foo`:
    /// 'bar' = "crate::types::Bar"
    ///
    /// # Quoted column name
    /// # Note: same quoting requirements as `macros.type_overrides`
    /// '"Bar"' = "crate::types::Bar"
    ///
    /// # Note: will NOT work (parses as `Bar`)
    /// # "Bar" = "crate::types::Bar"
    ///
    /// # Table name may be quoted (note the wrapping single-quotes)
    /// [macros.table-overrides.'"Foo"']
    /// 'bar' = "crate::types::Bar"
    /// '"Bar"' = "crate::types::Bar"
    ///
    /// # Table name may also be schema-qualified.
    /// # Note how the dot is inside the quotes.
    /// [macros.table-overrides.'my_schema.my_table']
    /// 'my_column' = "crate::types::MyType"
    ///
    /// # Quoted schema, table, and column names
    /// [macros.table-overrides.'"My Schema"."My Table"']
    /// '"My Column"' = "crate::types::MyType"
    /// ```
    pub table_overrides: BTreeMap<TableName, BTreeMap<ColumnName, RustType>>,
}

#[derive(Debug, Default)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(default, rename_all = "kebab-case")
)]
pub struct PreferredCrates {
    /// Specify the crate to use for mapping date/time types to Rust.
    ///
    /// The default behavior is to use whatever crate is enabled,
    /// in order of precedence: [`time`], [`chrono`], then [`jiff`].
    ///
    /// [`chrono`]: crate::types::chrono
    /// [`time`]: crate::types::time
    /// [`jiff`]: crate::types::jiff
    ///
    /// Example: Always Use Chrono
    /// -------
    /// Thanks to Cargo's [feature unification], a crate in the dependency graph may enable
    /// the `time` feature of SQLx which will force it on for all crates using SQLx,
    /// which will result in problems if your crate wants to use types from [`chrono`].
    ///
    /// You can use the type override syntax (see `sqlx::query!` for details),
    /// or you can force an override globally by setting this option.
    ///
    /// #### `sqlx.toml`
    /// ```toml
    /// [macros.preferred-crates]
    /// date-time = "chrono"
    /// ```
    ///
    /// [feature unification]: https://doc.rust-lang.org/cargo/reference/features.html#feature-unification
    pub date_time: DateTimeCrate,

    /// Specify the crate to use for mapping `NUMERIC` types to Rust.
    ///
    /// The default behavior is to use whatever crate is enabled,
    /// [`bigdecimal`] or [`rust_decimal`] (the latter takes precedent).
    ///
    /// [`bigdecimal`]: crate::types::bigdecimal
    /// [`rust_decimal`]: crate::types::rust_decimal
    ///
    /// Example: Always Use `bigdecimal`
    /// -------
    /// Thanks to Cargo's [feature unification], a crate in the dependency graph may enable
    /// the `rust_decimal` feature of SQLx which will force it on for all crates using SQLx,
    /// which will result in problems if your crate wants to use types from [`bigdecimal`].
    ///
    /// You can use the type override syntax (see `sqlx::query!` for details),
    /// or you can force an override globally by setting this option.
    ///
    /// #### `sqlx.toml`
    /// ```toml
    /// [macros.preferred-crates]
    /// numeric = "bigdecimal"
    /// ```
    ///
    /// [feature unification]: https://doc.rust-lang.org/cargo/reference/features.html#feature-unification
    pub numeric: NumericCrate,
}

/// The preferred crate to use for mapping date/time types to Rust.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum DateTimeCrate {
    /// Use whichever crate is enabled (`time`, then `chrono`, then `jiff`).
    #[default]
    Inferred,

    /// Always use types from [`chrono`][crate::types::chrono].
    ///
    /// ```toml
    /// [macros.preferred-crates]
    /// date-time = "chrono"
    /// ```
    Chrono,

    /// Always use types from [`time`][crate::types::time].
    ///
    /// ```toml
    /// [macros.preferred-crates]
    /// date-time = "time"
    /// ```
    Time,

    /// Always use types from [`jiff`][crate::types::jiff].
    ///
    /// ```toml
    /// [macros.preferred-crates]
    /// date-time = "jiff"
    /// ```
    Jiff,
}

/// The preferred crate to use for mapping `NUMERIC` types to Rust.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "sqlx-toml",
    derive(serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum NumericCrate {
    /// Use whichever crate is enabled (`rust_decimal` then `bigdecimal`).
    #[default]
    Inferred,

    /// Always use types from [`bigdecimal`][crate::types::bigdecimal].
    ///
    /// ```toml
    /// [macros.preferred-crates]
    /// numeric = "bigdecimal"
    /// ```
    #[cfg_attr(feature = "sqlx-toml", serde(rename = "bigdecimal"))]
    BigDecimal,

    /// Always use types from [`rust_decimal`][crate::types::rust_decimal].
    ///
    /// ```toml
    /// [macros.preferred-crates]
    /// numeric = "rust_decimal"
    /// ```
    RustDecimal,
}

/// A SQL type name; may optionally be schema-qualified.
///
/// See [`macros.type-overrides`][Config::type_overrides] for usages.
pub type SqlType = Box<str>;

/// A SQL table name; may optionally be schema-qualified.
///
/// See [`macros.table-overrides`][Config::table_overrides] for usages.
pub type TableName = Box<str>;

/// A column in a SQL table.
///
/// See [`macros.table-overrides`][Config::table_overrides] for usages.
pub type ColumnName = Box<str>;

/// A Rust type name or path.
///
/// Should be a global path (not relative).
pub type RustType = Box<str>;

/// Internal getter methods.
impl Config {
    /// Get the override for a given type name (optionally schema-qualified).
    pub fn type_override(&self, type_name: &str) -> Option<&str> {
        // TODO: make this case-insensitive
        self.type_overrides.get(type_name).map(|s| &**s)
    }

    /// Get the override for a given column and table name (optionally schema-qualified).
    pub fn column_override(&self, table: &str, column: &str) -> Option<&str> {
        self.table_overrides
            .get(table)
            .and_then(|by_column| by_column.get(column))
            .map(|s| &**s)
    }
}

impl DateTimeCrate {
    /// Returns `self == Self::Inferred`
    #[inline(always)]
    pub fn is_inferred(&self) -> bool {
        *self == Self::Inferred
    }

    #[inline(always)]
    pub fn crate_name(&self) -> Option<&str> {
        match self {
            Self::Inferred => None,
            Self::Chrono => Some("chrono"),
            Self::Time => Some("time"),
            Self::Jiff => Some("jiff"),
        }
    }
}

impl NumericCrate {
    /// Returns `self == Self::Inferred`
    #[inline(always)]
    pub fn is_inferred(&self) -> bool {
        *self == Self::Inferred
    }

    #[inline(always)]
    pub fn crate_name(&self) -> Option<&str> {
        match self {
            Self::Inferred => None,
            Self::BigDecimal => Some("bigdecimal"),
            Self::RustDecimal => Some("rust_decimal"),
        }
    }
}
