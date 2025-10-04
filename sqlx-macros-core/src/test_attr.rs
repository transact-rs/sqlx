use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::Parser;

#[cfg(feature = "migrate")]
struct Args {
    fixtures: Vec<(FixturesType, Vec<syn::LitStr>)>,
    migrations: MigrationsOpt,
    database_url_var: DatabaseUrlOpt,
}

#[cfg(feature = "migrate")]
enum FixturesType {
    None,
    RelativePath,
    CustomRelativePath(syn::LitStr),
    ExplicitPath,
}

#[cfg(feature = "migrate")]
enum MigrationsOpt {
    InferredPath,
    ExplicitPath(syn::LitStr),
    ExplicitMigrator(syn::Path),
    Disabled,
}

#[cfg(feature = "migrate")]
enum DatabaseUrlOpt {
    EnvironmentVariable(syn::LitStr),
    ExplicitVariable(syn::Path),
}

#[cfg(feature = "migrate")]
struct ParsedArgs {
    fixtures: Vec<TokenStream>,
    migrations: TokenStream,
    database_url_var: TokenStream,
}

type AttributeArgs = syn::punctuated::Punctuated<syn::Meta, syn::Token![,]>;

pub fn expand(args: TokenStream, input: syn::ItemFn) -> crate::Result<TokenStream> {
    let parser = AttributeArgs::parse_terminated;
    let args = parser.parse2(args)?;

    if input.sig.inputs.is_empty() {
        if !args.is_empty() {
            if cfg!(not(feature = "migrate")) {
                return Err(syn::Error::new_spanned(
                    args.first().unwrap(),
                    "control attributes are not allowed unless \
                        the `migrate` feature is enabled and \
                        automatic test DB management is used; see docs",
                )
                .into());
            }

            return Err(syn::Error::new_spanned(
                args.first().unwrap(),
                "control attributes are not allowed unless \
                    automatic test DB management is used; see docs",
            )
            .into());
        }

        return Ok(expand_simple(input));
    }

    #[cfg(feature = "migrate")]
    return expand_advanced(args, input);

    #[cfg(not(feature = "migrate"))]
    return Err(syn::Error::new_spanned(input, "`migrate` feature required").into());
}

fn expand_simple(input: syn::ItemFn) -> TokenStream {
    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;

    quote! {
        #[::core::prelude::v1::test]
        #(#attrs)*
        fn #name() #ret {
            ::sqlx::test_block_on(async { #body })
        }
    }
}

#[cfg(feature = "migrate")]
fn expand_advanced(args: AttributeArgs, input: syn::ItemFn) -> crate::Result<TokenStream> {
    let config = sqlx_core::config::Config::try_from_crate_or_default()?;

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let inputs = &input.sig.inputs;
    let body = &input.block;
    let attrs = &input.attrs;

    let parsed_args_list = parse_attr_args(&args, &input, &config)?;

    let fn_arg_types = inputs.iter().map(|_| quote! { _ });

    match parsed_args_list.len() {
        0 => {
            return Ok(quote! {
                #(#attrs)*
                #[::core::prelude::v1::test]
                fn #name() #ret {
                    async fn #name(#inputs) #ret {
                        #body
                    }

                    let mut args = ::sqlx::testing::TestArgs::new(concat!(module_path!(), "::", stringify!(#name)));

                    // We need to give a coercion site or else we get "unimplemented trait" errors.
                    let f: fn(#(#fn_arg_types),*) -> _ = #name;

                    ::sqlx::testing::TestFn::run_test(f, args)
                }
            });
        }
        args_num @ 1..=4 => {
            use proc_macro2::Span;
            use syn::Ident;

            let mut run_fn_args = Vec::with_capacity(args_num);
            for i in 1..=args_num {
                run_fn_args.push(Ident::new(&format!("args{i}"), Span::call_site()));
            }
            let run_fn = if args_num == 1 {
                quote! { ::sqlx::testing::TestFn::run_test(f, #(#run_fn_args,)*) }
            } else {
                let test_fn = Ident::new(&format!("TestFn{args_num}"), Span::call_site());
                quote! { ::sqlx::testing::#test_fn::run_test(f, #(#run_fn_args,)*) }
            };

            let mut args = Vec::new();
            for (i, parsed_args) in parsed_args_list.iter().enumerate() {
                let args_name = Ident::new(&format!("args{}", 1 + i), Span::call_site());
                let database_url_var = &parsed_args.database_url_var;
                let migrations = &parsed_args.migrations;
                let fixtures = parsed_args.fixtures.as_slice();

                args.push(quote! {
                    let mut #args_name = ::sqlx::testing::TestArgs::new(concat!(module_path!(), "::", stringify!(#name), "#{", #i, "}"));
                    #args_name.#migrations
                    #args_name.fixtures(&[#(#fixtures),*]);
                    #args_name.database_url_var(#database_url_var);
                });
            }

            return Ok(quote! {
                #(#attrs)*
                #[::core::prelude::v1::test]
                fn #name() #ret {
                    async fn #name(#inputs) #ret {
                        #body
                    }

                    #(#args)*

                    // We need to give a coercion site or else we get "unimplemented trait" errors.
                    let f: fn(#(#fn_arg_types),*) -> _ = #name;

                    #run_fn
                }
            });
        }
        _ => {
            return Err(Box::new(syn::Error::new_spanned(
                args.first().unwrap(),
                "expect to no more than 4 env attributes.",
            )));
        }
    }
}

#[cfg(feature = "migrate")]
fn parse_attr_args(
    args: &AttributeArgs,
    input: &syn::ItemFn,
    config: &sqlx_core::config::Config,
) -> crate::Result<Vec<ParsedArgs>> {
    let parser = AttributeArgs::parse_terminated;

    let mut parsed_args: Vec<ParsedArgs> = Vec::new();

    for arg in args {
        match arg {
            syn::Meta::List(list) if list.path.is_ident("env") => {
                let args = parser.parse2(list.tokens.clone())?;
                let parsed = parse_one_attr_args(args, input, config)?;
                parsed_args.push(parsed);
            }
            _ => {
                let parsed = parse_one_attr_args(args.clone(), input, config)?;
                return Ok(vec![parsed]);
            }
        }
    }

    Ok(parsed_args)
}

#[cfg(feature = "migrate")]
fn parse_one_attr_args(
    args: AttributeArgs,
    input: &syn::ItemFn,
    config: &sqlx_core::config::Config,
) -> crate::Result<ParsedArgs> {
    let inputs = &input.sig.inputs;

    let args = parse_args(args)?;

    let mut fixtures = Vec::new();

    for (fixture_type, fixtures_local) in args.fixtures {
        let mut res = match fixture_type {
            FixturesType::None => vec![],
            FixturesType::RelativePath => fixtures_local
                .into_iter()
                .map(|fixture| {
                    let mut fixture_str = fixture.value();
                    add_sql_extension_if_missing(&mut fixture_str);

                    let path = format!("fixtures/{}", fixture_str);

                    quote! {
                        ::sqlx::testing::TestFixture {
                            path: #path,
                            contents: include_str!(#path),
                        }
                    }
                })
                .collect(),
            FixturesType::CustomRelativePath(path) => fixtures_local
                .into_iter()
                .map(|fixture| {
                    let mut fixture_str = fixture.value();
                    add_sql_extension_if_missing(&mut fixture_str);

                    let path = format!("{}/{}", path.value(), fixture_str);

                    quote! {
                        ::sqlx::testing::TestFixture {
                            path: #path,
                            contents: include_str!(#path),
                        }
                    }
                })
                .collect(),
            FixturesType::ExplicitPath => fixtures_local
                .into_iter()
                .map(|fixture| {
                    let path = fixture.value();

                    quote! {
                        ::sqlx::testing::TestFixture {
                            path: #path,
                            contents: include_str!(#path),
                        }
                    }
                })
                .collect(),
        };
        fixtures.append(&mut res)
    }

    let migrations = match args.migrations {
        MigrationsOpt::ExplicitPath(path) => {
            let migrator = crate::migrate::expand(Some(path))?;
            quote! { migrator(&#migrator); }
        }
        MigrationsOpt::InferredPath if !inputs.is_empty() => {
            let path = crate::migrate::default_path(&config);

            let resolved_path = crate::common::resolve_path(path, proc_macro2::Span::call_site())?;

            if resolved_path.is_dir() {
                let migrator = crate::migrate::expand_with_path(&config, &resolved_path)?;
                quote! { migrator(&#migrator); }
            } else {
                quote! { no_migrator(); }
            }
        }
        MigrationsOpt::ExplicitMigrator(path) => {
            quote! { migrator(&#path); }
        }
        _ => quote! { no_migrator(); },
    };

    let database_url_var = match args.database_url_var {
        DatabaseUrlOpt::EnvironmentVariable(name) => {
            quote! { #name }
        }
        DatabaseUrlOpt::ExplicitVariable(path) => quote! { #path },
    };

    Ok(ParsedArgs {
        fixtures,
        migrations,
        database_url_var,
    })
}

#[cfg(feature = "migrate")]
fn parse_args(attr_args: AttributeArgs) -> syn::Result<Args> {
    use proc_macro2::Span;
    use syn::{
        parenthesized, parse::Parse, punctuated::Punctuated, token::Comma, Expr, Lit, LitStr, Meta,
        MetaNameValue, Token,
    };

    let mut fixtures = Vec::new();
    let mut migrations = MigrationsOpt::InferredPath;
    let mut database_url_var =
        DatabaseUrlOpt::EnvironmentVariable(LitStr::new("DATABASE_URL", Span::call_site()));

    for arg in attr_args {
        let path = arg.path().clone();

        match arg {
            syn::Meta::List(list) if list.path.is_ident("fixtures") => {
                let mut fixtures_local = vec![];
                let mut fixtures_type = FixturesType::None;

                let parse_nested = list.parse_nested_meta(|meta| {
                    if meta.path.is_ident("path") {
                        //  fixtures(path = "<path>", scripts("<file_1>","<file_2>")) checking `path` argument
                        meta.input.parse::<Token![=]>()?;
                        let val: LitStr = meta.input.parse()?;
                        parse_fixtures_path_args(&mut fixtures_type, val)?;
                    } else if meta.path.is_ident("scripts") {
                        //  fixtures(path = "<path>", scripts("<file_1>","<file_2>")) checking `scripts` argument
                        let content;
                        parenthesized!(content in meta.input);
                        let list = content.parse_terminated(<LitStr as Parse>::parse, Comma)?;
                        parse_fixtures_scripts_args(&mut fixtures_type, list, &mut fixtures_local)?;
                    } else {
                        return Err(syn::Error::new_spanned(
                            meta.path,
                            "unexpected fixture meta",
                        ));
                    }

                    Ok(())
                });

                if parse_nested.is_err() {
                    // fixtures("<file_1>","<file_2>") or fixtures("<path/file_1.sql>","<path/file_2.sql>")
                    let args =
                        list.parse_args_with(<Punctuated<LitStr, Token![,]>>::parse_terminated)?;
                    for arg in args {
                        parse_fixtures_args(&mut fixtures_type, arg, &mut fixtures_local)?;
                    }
                }

                fixtures.push((fixtures_type, fixtures_local));
            }
            syn::Meta::NameValue(value) if value.path.is_ident("migrations") => {
                if !matches!(migrations, MigrationsOpt::InferredPath) {
                    return Err(syn::Error::new_spanned(
                        value,
                        "cannot have more than one `migrations` or `migrator` arg",
                    ));
                }

                fn recurse_lit_lookup(expr: Expr) -> Option<Lit> {
                    match expr {
                        Expr::Lit(syn::ExprLit { lit, .. }) => Some(lit),
                        Expr::Group(syn::ExprGroup { expr, .. }) => recurse_lit_lookup(*expr),
                        _ => None,
                    }
                }

                let Some(lit) = recurse_lit_lookup(value.value) else {
                    return Err(syn::Error::new_spanned(path, "expected string or `false`"));
                };

                migrations = match lit {
                    // migrations = false
                    Lit::Bool(b) if !b.value => MigrationsOpt::Disabled,
                    // migrations = true
                    Lit::Bool(b) => {
                        return Err(syn::Error::new_spanned(
                            b,
                            "`migrations = true` is redundant",
                        ));
                    }
                    // migrations = "path"
                    Lit::Str(s) => MigrationsOpt::ExplicitPath(s),
                    lit => return Err(syn::Error::new_spanned(lit, "expected string or `false`")),
                };
            }
            // migrator = "<path>"
            Meta::NameValue(MetaNameValue { value, .. }) if path.is_ident("migrator") => {
                if !matches!(migrations, MigrationsOpt::InferredPath) {
                    return Err(syn::Error::new_spanned(
                        path,
                        "cannot have more than one `migrations` or `migrator` arg",
                    ));
                }

                let Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(lit), ..
                }) = value
                else {
                    return Err(syn::Error::new_spanned(path, "expected string"));
                };

                migrations = MigrationsOpt::ExplicitMigrator(lit.parse()?);
            }
            // var = "<path>"
            Meta::NameValue(MetaNameValue { value, .. }) if path.is_ident("var") => {
                let Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(lit), ..
                }) = value
                else {
                    return Err(syn::Error::new_spanned(path, "expected string"));
                };

                database_url_var = DatabaseUrlOpt::ExplicitVariable(lit.parse()?);
            }
            // var("<environment variable name>")
            Meta::List(list) if path.is_ident("var") => {
                let s: LitStr = list.parse_args()?;
                database_url_var = DatabaseUrlOpt::EnvironmentVariable(s);
            }
            arg => {
                return Err(syn::Error::new_spanned(
                    arg,
                    r#"expected `fixtures("<filename>", ...)` or `migrations = "<path>" | false` or `migrator = "<rust path>"` or `var("DATABASE_URL")` or `var = "<rust path>"`"#,
                ));
            }
        }
    }

    Ok(Args {
        fixtures,
        migrations,
        database_url_var,
    })
}

#[cfg(feature = "migrate")]
fn parse_fixtures_args(
    fixtures_type: &mut FixturesType,
    litstr: syn::LitStr,
    fixtures_local: &mut Vec<syn::LitStr>,
) -> syn::Result<()> {
    //  fixtures(path = "<path>", scripts("<file_1>","<file_2>")) checking `path` argument
    let path_str = litstr.value();
    let path = std::path::Path::new(&path_str);
    // This will be `true` if there's at least one path separator (`/` or `\`)
    // It's also true for all absolute paths, even e.g. `/foo.sql` as the root directory is counted as a component.
    let is_explicit_path = path.components().count() > 1;
    match fixtures_type {
        FixturesType::None => {
            if is_explicit_path {
                *fixtures_type = FixturesType::ExplicitPath;
            } else {
                *fixtures_type = FixturesType::RelativePath;
            }
        }
        FixturesType::RelativePath => {
            if is_explicit_path {
                return Err(syn::Error::new_spanned(
                    litstr,
                    "expected only relative path fixtures",
                ));
            }
        }
        FixturesType::ExplicitPath => {
            if !is_explicit_path {
                return Err(syn::Error::new_spanned(
                    litstr,
                    "expected only explicit path fixtures",
                ));
            }
        }
        FixturesType::CustomRelativePath(_) => {
            return Err(syn::Error::new_spanned(
                litstr,
                "custom relative path fixtures must be defined in `scripts` argument",
            ))
        }
    }
    if (matches!(fixtures_type, FixturesType::ExplicitPath) && !is_explicit_path) {
        return Err(syn::Error::new_spanned(
            litstr,
            "expected explicit path fixtures to have `.sql` extension",
        ));
    }
    fixtures_local.push(litstr);
    Ok(())
}

#[cfg(feature = "migrate")]
fn parse_fixtures_path_args(
    fixtures_type: &mut FixturesType,
    namevalue: syn::LitStr,
) -> syn::Result<()> {
    if !matches!(fixtures_type, FixturesType::None) {
        return Err(syn::Error::new_spanned(
            namevalue,
            "`path` must be the first argument of `fixtures`",
        ));
    }
    *fixtures_type = FixturesType::CustomRelativePath(namevalue);
    Ok(())
}

#[cfg(feature = "migrate")]
fn parse_fixtures_scripts_args(
    fixtures_type: &mut FixturesType,
    list: syn::punctuated::Punctuated<syn::LitStr, syn::Token![,]>,
    fixtures_local: &mut Vec<syn::LitStr>,
) -> syn::Result<()> {
    //  fixtures(path = "<path>", scripts("<file_1>","<file_2>")) checking `scripts` argument

    if !matches!(fixtures_type, FixturesType::CustomRelativePath(_)) {
        return Err(syn::Error::new_spanned(
            list,
            "`scripts` must be the second argument of `fixtures` and used together with `path`",
        ));
    }

    fixtures_local.extend(list);
    Ok(())
}

#[cfg(feature = "migrate")]
fn add_sql_extension_if_missing(fixture: &mut String) {
    let has_extension = std::path::Path::new(&fixture).extension().is_some();
    if !has_extension {
        fixture.push_str(".sql")
    }
}
