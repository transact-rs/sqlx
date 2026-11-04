use crate::config::macros::PreferredCrates;
use crate::database::Database;
use crate::decode::Decode;
use crate::type_info::TypeInfo;
use crate::value::Value;
use std::any::Any;
use std::fmt;
use std::fmt::{Debug, Formatter};

/// The type of query parameter checking done by a SQL database.
#[derive(PartialEq, Eq)]
pub enum ParamChecking {
    /// Parameter checking is weak or nonexistent (uses coercion or allows mismatches).
    Weak,
    /// Parameter checking is strong (types must match exactly).
    Strong,
}

/// Type-checking extensions for the `Database` trait.
///
/// Mostly supporting code for the macros, and for `Debug` impls.
pub trait TypeChecking: Database {
    /// Describes how the database in question typechecks query parameters.
    const PARAM_CHECKING: ParamChecking;

    /// Get the full path of the Rust type that corresponds to the given `TypeInfo`, if applicable.
    ///
    /// If the type has a borrowed equivalent suitable for query parameters,
    /// this is that borrowed type.
    fn param_type_for_id(
        id: &Self::TypeInfo,
        preferred_crates: &PreferredCrates,
    ) -> Result<&'static str, Error>;

    /// Get the full path of the Rust type that corresponds to the given `TypeInfo`, if applicable.
    ///
    /// Always returns the owned version of the type, suitable for decoding from `Row`.
    fn return_type_for_id(
        id: &Self::TypeInfo,
        preferred_crates: &PreferredCrates,
    ) -> Result<&'static str, Error>;

    /// Get the name of the Cargo feature gate that must be enabled to process the given `TypeInfo`,
    /// if applicable.
    fn get_feature_gate(info: &Self::TypeInfo) -> Option<&'static str>;

    /// If `value` is a well-known type, decode and format it using `Debug`.
    ///
    /// If `value` is not a well-known type or could not be decoded, the reason is printed instead.
    fn fmt_value_debug(value: &<Self as Database>::Value) -> FmtValue<'_, Self>;
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no built-in mapping found for SQL type; a type override may be required")]
    NoMappingFound,
    #[error("Cargo feature for configured `macros.preferred-crates.date-time` not enabled")]
    DateTimeCrateFeatureNotEnabled,
    #[error("Cargo feature for configured `macros.preferred-crates.numeric` not enabled")]
    NumericCrateFeatureNotEnabled,
    #[error("multiple date-time types are possible; falling back to `{fallback}`")]
    AmbiguousDateTimeType { fallback: &'static str },
    #[error("multiple numeric types are possible; falling back to `{fallback}`")]
    AmbiguousNumericType { fallback: &'static str },
}

/// An adapter for [`Value`] which attempts to decode the value and format it when printed using [`Debug`].
pub struct FmtValue<'v, DB>
where
    DB: Database,
{
    value: &'v <DB as Database>::Value,
    fmt: fn(&'v <DB as Database>::Value, &mut Formatter<'_>) -> fmt::Result,
}

impl<'v, DB> FmtValue<'v, DB>
where
    DB: Database,
{
    // This API can't take `ValueRef` directly as it would need to pass it to `Decode` by-value,
    // which means taking ownership of it. We cannot rely on a `Clone` impl because `SqliteValueRef` doesn't have one.
    /// When printed with [`Debug`], attempt to decode `value` as the given type `T` and format it using [`Debug`].
    ///
    /// If `value` could not be decoded as `T`, the reason is printed instead.
    pub fn debug<T>(value: &'v <DB as Database>::Value) -> Self
    where
        T: Decode<'v, DB> + Debug + Any,
    {
        Self {
            value,
            fmt: |value, f| {
                let info = value.type_info();

                match T::decode(value.as_ref()) {
                    Ok(value) => Debug::fmt(&value, f),
                    Err(e) => {
                        if e.is::<crate::error::UnexpectedNullError>() {
                            f.write_str("NULL")
                        } else {
                            f.write_fmt(format_args!(
                                "(error decoding SQL type {} as {}: {e:?})",
                                info.name(),
                                std::any::type_name::<T>()
                            ))
                        }
                    }
                }
            },
        }
    }

    /// If the type to be decoded is not known or not supported, print the SQL type instead,
    /// as well as any applicable SQLx feature that needs to be enabled.
    pub fn unknown(value: &'v <DB as Database>::Value) -> Self
    where
        DB: TypeChecking,
    {
        Self {
            value,
            fmt: |value, f| {
                let info = value.type_info();

                if let Some(feature_gate) = <DB as TypeChecking>::get_feature_gate(&info) {
                    return f.write_fmt(format_args!(
                        "(unknown SQL type {}: SQLx feature {feature_gate} not enabled)",
                        info.name()
                    ));
                }

                f.write_fmt(format_args!("(unknown SQL type {})", info.name()))
            },
        }
    }
}

impl<DB> Debug for FmtValue<'_, DB>
where
    DB: Database,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        (self.fmt)(self.value, f)
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! select_input_type {
    ($ty:ty, $input:ty) => {
        stringify!($input)
    };
    ($ty:ty) => {
        stringify!($ty)
    };
}

#[macro_export]
macro_rules! impl_type_checking {
    (
        $database:path {
            $($(#[$meta:meta])? $ty:ty $(| $input:ty)?),*$(,)?
        },
        ParamChecking::$param_checking:ident,
        feature-types: $ty_info:ident => $get_gate:expr,
        datetime-types: {
            chrono: {
                $($chrono_ty:ty $(| $chrono_input:ty)?),*$(,)?
            },
            time: {
                $($time_ty:ty $(| $time_input:ty)?),*$(,)?
            },
        },
        numeric-types: {
            bigdecimal: {
                $($bigdecimal_ty:ty $(| $bigdecimal_input:ty)?),*$(,)?
            },
            rust_decimal: {
                $($rust_decimal_ty:ty $(| $rust_decimal_input:ty)?),*$(,)?
            },
        },
    ) => {
        impl $crate::type_checking::TypeChecking for $database {
            const PARAM_CHECKING: $crate::type_checking::ParamChecking = $crate::type_checking::ParamChecking::$param_checking;

            fn param_type_for_id(
                info: &Self::TypeInfo,
                preferred_crates: &$crate::config::macros::PreferredCrates,
            ) -> Result<&'static str, $crate::type_checking::Error> {
                use $crate::config::macros::{DateTimeCrate, NumericCrate};
                use $crate::type_checking::Error;

                // Check non-special types
                // ---------------------
                $(
                    $(#[$meta])?
                    if <$ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                        return Ok($crate::select_input_type!($ty $(, $input)?));
                    }
                )*

                $(
                    $(#[$meta])?
                    if <$ty as sqlx_core::types::Type<$database>>::compatible(info) {
                        return Ok($crate::select_input_type!($ty $(, $input)?));
                    }
                )*

                // Check `macros.preferred-crates.date-time`
                //
                // Due to legacy reasons, `time` takes precedent over `chrono` if both are enabled.
                // Any crates added later should be _lower_ priority than `chrono` to avoid breakages.
                // ----------------------------------------
                #[cfg(feature = "time")]
                if matches!(preferred_crates.date_time, DateTimeCrate::Time | DateTimeCrate::Inferred) {
                    $(
                        if <$time_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            if preferred_crates.date_time == DateTimeCrate::Inferred && cfg!(feature = "chrono") {
                                return Err($crate::type_checking::Error::AmbiguousDateTimeType {
                                    fallback: $crate::select_input_type!($time_ty $(, $time_input)?),
                                });
                            }

                            return Ok($crate::select_input_type!($time_ty $(, $time_input)?));
                        }
                    )*

                    $(
                        if <$time_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            if preferred_crates.date_time == DateTimeCrate::Inferred && cfg!(feature = "chrono") {
                                return Err($crate::type_checking::Error::AmbiguousDateTimeType {
                                    fallback: $crate::select_input_type!($time_ty $(, $time_input)?),
                                });
                            }

                            return Ok($crate::select_input_type!($time_ty $(, $time_input)?));
                        }
                    )*
                }

                #[cfg(not(feature = "time"))]
                if preferred_crates.date_time == DateTimeCrate::Time {
                    return Err(Error::DateTimeCrateFeatureNotEnabled);
                }

                #[cfg(feature = "chrono")]
                if matches!(preferred_crates.date_time, DateTimeCrate::Chrono | DateTimeCrate::Inferred) {
                    $(
                        if <$chrono_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            return Ok($crate::select_input_type!($chrono_ty $(, $chrono_input)?));
                        }
                    )*

                    $(
                        if <$chrono_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            return Ok($crate::select_input_type!($chrono_ty $(, $chrono_input)?));
                        }
                    )*
                }

                #[cfg(not(feature = "chrono"))]
                if preferred_crates.date_time == DateTimeCrate::Chrono {
                    return Err(Error::DateTimeCrateFeatureNotEnabled);
                }

                // Check `macros.preferred-crates.numeric`
                //
                // Due to legacy reasons, `bigdecimal` takes precedent over `rust_decimal` if
                // both are enabled.
                // ----------------------------------------
                #[cfg(feature = "bigdecimal")]
                if matches!(preferred_crates.numeric, NumericCrate::BigDecimal | NumericCrate::Inferred) {
                    $(
                        if <$bigdecimal_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            if preferred_crates.numeric == NumericCrate::Inferred && cfg!(feature = "rust_decimal") {
                                return Err($crate::type_checking::Error::AmbiguousNumericType {
                                    fallback: $crate::select_input_type!($bigdecimal_ty $(, $bigdecimal_input)?),
                                });
                            }

                            return Ok($crate::select_input_type!($bigdecimal_ty $(, $bigdecimal_input)?));
                        }
                    )*

                    $(
                        if <$bigdecimal_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            if preferred_crates.numeric == NumericCrate::Inferred && cfg!(feature = "rust_decimal") {
                                return Err($crate::type_checking::Error::AmbiguousNumericType {
                                    fallback: $crate::select_input_type!($bigdecimal_ty $(, $bigdecimal_input)?),
                                });
                            }

                            return Ok($crate::select_input_type!($bigdecimal_ty $(, $bigdecimal_input)?));
                        }
                    )*
                }

                #[cfg(not(feature = "bigdecimal"))]
                if preferred_crates.numeric == NumericCrate::BigDecimal {
                    return Err(Error::NumericCrateFeatureNotEnabled);
                }

                #[cfg(feature = "rust_decimal")]
                if matches!(preferred_crates.numeric, NumericCrate::RustDecimal | NumericCrate::Inferred) {
                    $(
                        if <$rust_decimal_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            return Ok($crate::select_input_type!($rust_decimal_ty $(, $rust_decimal_input)?));
                        }
                    )*

                    $(
                        if <$rust_decimal_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            return Ok($crate::select_input_type!($rust_decimal_ty $(, $rust_decimal_input)?));
                        }
                    )*
                }

                #[cfg(not(feature = "rust_decimal"))]
                if preferred_crates.numeric == NumericCrate::RustDecimal {
                    return Err(Error::NumericCrateFeatureNotEnabled);
                }

                Err(Error::NoMappingFound)
            }

            fn return_type_for_id(
                info: &Self::TypeInfo,
                preferred_crates: &$crate::config::macros::PreferredCrates,
            ) -> Result<&'static str, $crate::type_checking::Error> {
                use $crate::config::macros::{DateTimeCrate, NumericCrate};
                use $crate::type_checking::Error;

                // Check non-special types
                // ---------------------
                $(
                    $(#[$meta])?
                    if <$ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                        return Ok(stringify!($ty));
                    }
                )*

                $(
                    $(#[$meta])?
                    if <$ty as sqlx_core::types::Type<$database>>::compatible(info) {
                        return Ok(stringify!($ty));
                    }
                )*

                // Check `macros.preferred-crates.date-time`
                //
                // Due to legacy reasons, `time` takes precedent over `chrono` if both are enabled.
                // Any crates added later should be _lower_ priority than `chrono` to avoid breakages.
                // ----------------------------------------
                #[cfg(feature = "time")]
                if matches!(preferred_crates.date_time, DateTimeCrate::Time | DateTimeCrate::Inferred) {
                    $(
                        if <$time_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            if preferred_crates.date_time == DateTimeCrate::Inferred && cfg!(feature = "chrono") {
                                return Err($crate::type_checking::Error::AmbiguousDateTimeType {
                                    fallback: stringify!($time_ty),
                                });
                            }

                            return Ok(stringify!($time_ty));
                        }
                    )*

                    $(
                        if <$time_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            if preferred_crates.date_time == DateTimeCrate::Inferred && cfg!(feature = "chrono") {
                                return Err($crate::type_checking::Error::AmbiguousDateTimeType {
                                    fallback: stringify!($time_ty),
                                });
                            }

                            return Ok(stringify!($time_ty));
                        }
                    )*
                }

                #[cfg(not(feature = "time"))]
                if preferred_crates.date_time == DateTimeCrate::Time {
                    return Err(Error::DateTimeCrateFeatureNotEnabled);
                }

                #[cfg(feature = "chrono")]
                if matches!(preferred_crates.date_time, DateTimeCrate::Chrono | DateTimeCrate::Inferred) {
                    $(
                        if <$chrono_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            return Ok(stringify!($chrono_ty));
                        }
                    )*

                    $(
                        if <$chrono_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            return Ok(stringify!($chrono_ty));
                        }
                    )*
                }

                #[cfg(not(feature = "chrono"))]
                if preferred_crates.date_time == DateTimeCrate::Chrono {
                    return Err(Error::DateTimeCrateFeatureNotEnabled);
                }

                // Check `macros.preferred-crates.numeric`
                //
                // Due to legacy reasons, `bigdecimal` takes precedent over `rust_decimal` if
                // both are enabled.
                // ----------------------------------------
                #[cfg(feature = "bigdecimal")]
                if matches!(preferred_crates.numeric, NumericCrate::BigDecimal | NumericCrate::Inferred) {
                    $(
                        if <$bigdecimal_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            if preferred_crates.numeric == NumericCrate::Inferred && cfg!(feature = "rust_decimal") {
                                return Err($crate::type_checking::Error::AmbiguousNumericType {
                                    fallback: stringify!($bigdecimal_ty),
                                });
                            }

                            return Ok(stringify!($bigdecimal_ty));
                        }
                    )*

                    $(
                        if <$bigdecimal_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            if preferred_crates.numeric == NumericCrate::Inferred && cfg!(feature = "rust_decimal") {
                                return Err($crate::type_checking::Error::AmbiguousNumericType {
                                    fallback: stringify!($bigdecimal_ty),
                                });
                            }

                            return Ok(stringify!($bigdecimal_ty));
                        }
                    )*
                }

                #[cfg(not(feature = "bigdecimal"))]
                if preferred_crates.numeric == NumericCrate::BigDecimal {
                    return Err(Error::NumericCrateFeatureNotEnabled);
                }

                #[cfg(feature = "rust_decimal")]
                if matches!(preferred_crates.numeric, NumericCrate::RustDecimal | NumericCrate::Inferred) {
                    $(
                        if <$rust_decimal_ty as sqlx_core::types::Type<$database>>::type_info() == *info {
                            return Ok($crate::select_input_type!($rust_decimal_ty $(, $rust_decimal_input)?));
                        }
                    )*

                    $(
                        if <$rust_decimal_ty as sqlx_core::types::Type<$database>>::compatible(info) {
                            return Ok($crate::select_input_type!($rust_decimal_ty $(, $rust_decimal_input)?));
                        }
                    )*
                }

                #[cfg(not(feature = "rust_decimal"))]
                if preferred_crates.numeric == NumericCrate::RustDecimal {
                    return Err(Error::NumericCrateFeatureNotEnabled);
                }

                Err(Error::NoMappingFound)
            }

            fn get_feature_gate($ty_info: &Self::TypeInfo) -> Option<&'static str> {
                $get_gate
            }

            fn fmt_value_debug(value: &Self::Value) -> $crate::type_checking::FmtValue<Self> {
                use $crate::value::Value;

                let info = value.type_info();

                #[cfg(feature = "time")]
                {
                    $(
                        if <$time_ty as sqlx_core::types::Type<$database>>::compatible(&info) {
                            return $crate::type_checking::FmtValue::debug::<$time_ty>(value);
                        }
                    )*
                }

                #[cfg(feature = "chrono")]
                {
                    $(
                        if <$chrono_ty as sqlx_core::types::Type<$database>>::compatible(&info) {
                            return $crate::type_checking::FmtValue::debug::<$chrono_ty>(value);
                        }
                    )*
                }

                #[cfg(feature = "bigdecimal")]
                {
                    $(
                        if <$bigdecimal_ty as sqlx_core::types::Type<$database>>::compatible(&info) {
                            return $crate::type_checking::FmtValue::debug::<$bigdecimal_ty>(value);
                        }
                    )*
                }

                #[cfg(feature = "rust_decimal")]
                {
                    $(
                        if <$rust_decimal_ty as sqlx_core::types::Type<$database>>::compatible(&info) {
                            return $crate::type_checking::FmtValue::debug::<$rust_decimal_ty>(value);
                        }
                    )*
                }

                $(
                    $(#[$meta])?
                    if <$ty as sqlx_core::types::Type<$database>>::compatible(&info) {
                        return $crate::type_checking::FmtValue::debug::<$ty>(value);
                    }
                )*

                $crate::type_checking::FmtValue::unknown(value)
            }
        }
    };
}
