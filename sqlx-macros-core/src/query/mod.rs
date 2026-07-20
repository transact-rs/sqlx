use std::path::{Path, PathBuf};

use proc_macro2::TokenStream;
use syn::Type;

pub use input::QueryMacroInput;
use quote::{format_ident, quote};
use sqlx_core::database::Database;
use sqlx_core::{column::Column, describe::Describe, type_info::TypeInfo};

use crate::database::DatabaseExt;
use crate::query::data::{hash_string, DynQueryData, QueryData};
use crate::query::input::RecordType;
use crate::query::metadata::MacrosEnv;
use either::Either;
use metadata::Metadata;
use sqlx_core::config::Config;
use url::Url;

mod args;
mod cache;
mod data;
mod input;
mod metadata;
mod output;

#[derive(Copy, Clone)]
pub struct QueryDriver {
    db_name: &'static str,
    url_schemes: &'static [&'static str],
    expand:
        fn(&Config, QueryMacroInput, QueryDataSource, Option<&Path>) -> crate::Result<TokenStream>,
}

impl QueryDriver {
    pub const fn new<DB: DatabaseExt>() -> Self
    where
        Describe<DB>: serde::Serialize + serde::de::DeserializeOwned,
    {
        QueryDriver {
            db_name: DB::NAME,
            url_schemes: DB::URL_SCHEMES,
            expand: expand_with::<DB>,
        }
    }
}
pub enum QueryDataSource<'a> {
    Live {
        database_url: &'a str,
        database_url_parsed: Url,
    },
    Cached(DynQueryData),
}

impl<'a> QueryDataSource<'a> {
    pub fn live(database_url: &'a str) -> crate::Result<Self> {
        Ok(QueryDataSource::Live {
            database_url,
            database_url_parsed: database_url.parse()?,
        })
    }

    pub fn matches_driver(&self, driver: &QueryDriver) -> bool {
        match self {
            Self::Live {
                database_url_parsed,
                ..
            } => driver.url_schemes.contains(&database_url_parsed.scheme()),
            Self::Cached(dyn_data) => dyn_data.db_name == driver.db_name,
        }
    }
}
pub fn expand_input<'a>(
    input: QueryMacroInput,
    drivers: impl IntoIterator<Item = &'a QueryDriver>,
) -> crate::Result<TokenStream> {
    let metadata = metadata::try_for_crate()?;

    let metadata_env = metadata.env()?;

    let data_source = match &*metadata_env {
        MacrosEnv {
            offline: None | Some(false),
            database_url: Some(db_url),
            ..
        }
        // Allow `DATABASE_URL=''`
        if !db_url.is_empty() => QueryDataSource::live(db_url)?,
        MacrosEnv {
            offline,
            offline_dir,
            ..
        } => {
            // Try load the cached query metadata file.
            let filename = format!("query-{}.json", hash_string(&input.sql));

            // Check SQLX_OFFLINE_DIR, then local .sqlx, then workspace .sqlx.
            let dirs = [
                |_: &Metadata, offline_dir: Option<&Path>| offline_dir.map(PathBuf::from),
                |meta: &Metadata, _: Option<&Path>| Some(meta.manifest_dir.join(".sqlx")),
                |meta: &Metadata, _: Option<&Path>| Some(meta.workspace_root().join(".sqlx")),
            ];

            let Some(data_file_path) = dirs
                .iter()
                .filter_map(|path| path(&metadata, offline_dir.as_deref()))
                .map(|path| path.join(&filename))
                .find(|path| path.exists())
            else {
                return Err(
                    if offline.unwrap_or(false) {
                        "`SQLX_OFFLINE=true` but there is no cached data for this query, run `cargo sqlx prepare` to update the query cache or unset `SQLX_OFFLINE`"
                    } else {
                        "set `DATABASE_URL` to use query macros online, or run `cargo sqlx prepare` to update the query cache"
                    }.into()
                );
            };

            QueryDataSource::Cached(DynQueryData::from_data_file(&data_file_path, &input.sql)?)
        }
    };

    for driver in drivers {
        if data_source.matches_driver(driver) {
            return (driver.expand)(
                &metadata.config,
                input,
                data_source,
                metadata_env.offline_dir.as_deref(),
            );
        }
    }

    match data_source {
        QueryDataSource::Live {
            database_url_parsed,
            ..
        } => Err(format!(
            "no database driver found matching URL scheme {:?}; the corresponding Cargo feature may need to be enabled", 
            database_url_parsed.scheme()
        ).into()),
        QueryDataSource::Cached(data) => {
            Err(format!(
                "found cached data for database {:?} but no matching driver; the corresponding Cargo feature may need to be enabled",
                data.db_name
            ).into())
        }
    }
}

fn expand_with<DB: DatabaseExt>(
    config: &Config,
    input: QueryMacroInput,
    data_source: QueryDataSource,
    offline_dir: Option<&Path>,
) -> crate::Result<TokenStream>
where
    Describe<DB>: DescribeExt,
{
    let (query_data, save_dir): (QueryData<DB>, Option<&Path>) = match data_source {
        // If the build is offline, the cache is our input so it's pointless to also write data for it.
        QueryDataSource::Cached(dyn_data) => (QueryData::from_dyn_data(dyn_data)?, None),
        QueryDataSource::Live { database_url, .. } => {
            let describe = DB::describe_blocking(&input.sql, database_url, &config.drivers)?;
            (QueryData::from_describe(&input.sql, describe), offline_dir)
        }
    };

    expand_with_data(config, input, query_data, save_dir)
}

// marker trait for `Describe` that lets us conditionally require it to be `Serialize + Deserialize`
trait DescribeExt: serde::Serialize + serde::de::DeserializeOwned {}

impl<DB: Database> DescribeExt for Describe<DB> where
    Describe<DB>: serde::Serialize + serde::de::DeserializeOwned
{
}

#[derive(Default)]
struct Warnings {
    ambiguous_datetime: bool,
    ambiguous_numeric: bool,
}

fn expand_with_data<DB: DatabaseExt>(
    config: &Config,
    input: QueryMacroInput,
    data: QueryData<DB>,
    save_dir: Option<&Path>,
) -> crate::Result<TokenStream>
where
    Describe<DB>: DescribeExt,
{
    // validate at the minimum that our args match the query's input parameters
    let num_parameters = match data.describe.parameters() {
        Some(Either::Left(params)) => Some(params.len()),
        Some(Either::Right(num)) => Some(num),

        None => None,
    };

    if let Some(num) = num_parameters {
        if num != input.arg_exprs.len() {
            return Err(
                format!("expected {} parameters, got {}", num, input.arg_exprs.len()).into(),
            );
        }
    }

    let mut warnings = Warnings::default();

    let args_tokens = args::quote_args(&input, config, &mut warnings, &data.describe)?;

    let query_args = format_ident!("query_args");

    let output = if data
        .describe
        .columns()
        .iter()
        .all(|it| it.type_info().is_void())
    {
        let db_path = DB::db_path();
        let sql = &input.sql;

        quote! {
            ::sqlx::__query_with_result::<#db_path, _>(#sql, #query_args)
        }
    } else {
        match input.record_type {
            RecordType::Generated => {
                let columns = output::columns_to_rust::<DB>(&data.describe, config, &mut warnings)?;

                let record_name: Type = syn::parse_str("Record").unwrap();

                for rust_col in &columns {
                    if rust_col.type_.is_wildcard() {
                        return Err(
                            "wildcard overrides are only allowed with an explicit record type, \
                             e.g. `query_as!()` and its variants"
                                .into(),
                        );
                    }
                }

                let record_fields = columns
                    .iter()
                    .map(|output::RustColumn { ident, type_, .. }| quote!(#ident: #type_,));

                let mut record_tokens = quote! {
                    #[derive(Debug)]
                    #[allow(non_snake_case)]
                    struct #record_name {
                        #(#record_fields)*
                    }
                };

                record_tokens.extend(output::quote_query_as::<DB>(
                    &input,
                    &record_name,
                    &query_args,
                    &columns,
                ));

                record_tokens
            }
            RecordType::Given(ref out_ty) => {
                let columns = output::columns_to_rust::<DB>(&data.describe, config, &mut warnings)?;

                output::quote_query_as::<DB>(&input, out_ty, &query_args, &columns)
            }
            RecordType::Scalar => output::quote_query_scalar::<DB>(
                &input,
                config,
                &mut warnings,
                &query_args,
                &data.describe,
            )?,
        }
    };

    let mut warnings_out = TokenStream::new();

    if warnings.ambiguous_datetime {
        // Warns if the date-time crate is inferred but both `chrono` and `time` are enabled
        warnings_out.extend(quote! {
            ::sqlx::warn_on_ambiguous_inferred_date_time_crate();
        });
    }

    if warnings.ambiguous_numeric {
        // Warns if the numeric crate is inferred but both `bigdecimal` and `rust_decimal` are enabled
        warnings_out.extend(quote! {
            ::sqlx::warn_on_ambiguous_inferred_numeric_crate();
        });
    }

    let ret_tokens = quote! {
        {
            #[allow(clippy::all)]
            {
                use ::sqlx::Arguments as _;

                #warnings_out

                #args_tokens

                #output
            }
        }
    };

    if let Some(save_dir) = save_dir {
        data.save_in(save_dir)?;
    }

    Ok(ret_tokens)
}
