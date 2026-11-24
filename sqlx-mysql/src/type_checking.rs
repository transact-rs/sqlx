// Type mappings used by the macros and `Debug` impls.

#[allow(unused_imports)]
use sqlx_core as sqlx;

use crate::MySql;

impl_type_checking!(
    MySql {
        u8,
        u16,
        u32,
        u64,
        i8,
        i16,
        i32,
        i64,
        f32,
        f64,

        // ordering is important here as otherwise we might infer strings to be binary
        // CHAR, VAR_CHAR, TEXT
        String,

        // BINARY, VAR_BINARY, BLOB
        Vec<u8>,

        #[cfg(feature = "json")]
        sqlx::types::JsonValue,
    },
    ParamChecking::Weak,
    feature-types: info => info.__type_feature_gate(),
    // The expansion of the macro automatically applies the correct feature name
    // and checks `[macros.preferred-crates]`
    datetime-types: {
        chrono: {
            sqlx::types::chrono::NaiveTime,

            sqlx::types::chrono::NaiveDate,

            sqlx::types::chrono::NaiveDateTime,

            sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
        },
        time: {
            sqlx::types::time::Time,

            sqlx::types::time::Date,

            sqlx::types::time::PrimitiveDateTime,

            sqlx::types::time::OffsetDateTime,
        },
        jiff: {
            sqlx::types::jiff::Time,

            sqlx::types::jiff::Date,

            sqlx::types::jiff::DateTime,

            sqlx::types::jiff::Timestamp,
        },
    },
    numeric-types: {
        bigdecimal: {
            sqlx::types::BigDecimal,
        },
        rust_decimal: {
            sqlx::types::Decimal,
        },
    },
);
