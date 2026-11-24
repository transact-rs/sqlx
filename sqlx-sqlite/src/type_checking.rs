use crate::Sqlite;
#[allow(unused_imports)]
use sqlx_core as sqlx;

// f32 is not included below as REAL represents a floating point value
// stored as an 8-byte IEEE floating point number (i.e. an f64)
// For more info see: https://www.sqlite.org/datatype3.html#storage_classes_and_datatypes
impl_type_checking!(
    Sqlite {
        // Note that since the macro checks `column_type_info == <T>::type_info()` first,
        // we can list `bool` without it being automatically picked for all integer types
        // due to its `TypeInfo::compatible()` impl.
        bool,
        // Since it returns `DataType::Int4` for `type_info()`,
        // `i32` should only be chosen IFF the column decltype is `INT4`
        i32,
        i64,
        f64,
        String,
        Vec<u8>,

        #[cfg(feature = "uuid")]
        sqlx::types::Uuid,
    },
    ParamChecking::Weak,
    // While there are type integrations that must be enabled via Cargo feature,
    // SQLite's type system doesn't actually have any type that we cannot decode by default.
    //
    // The type integrations simply allow the user to skip some intermediate representation,
    // which is usually TEXT.
    feature-types: _info => None,

    // The expansion of the macro automatically applies the correct feature name
    // and checks `[macros.preferred-crates]`
    datetime-types: {
        chrono: {
            sqlx::types::chrono::NaiveDate,

            sqlx::types::chrono::NaiveDateTime,

            sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>
                | sqlx::types::chrono::DateTime<_>,
        },
        time: {
            sqlx::types::time::OffsetDateTime,

            sqlx::types::time::PrimitiveDateTime,

            sqlx::types::time::Date,
        },
        jiff: {
            sqlx::types::jiff::Date,

            sqlx::types::jiff::DateTime,

            sqlx::types::jiff::Timestamp,
        },
    },
    numeric-types: {
        bigdecimal: { },
        rust_decimal: { },
    },
);
