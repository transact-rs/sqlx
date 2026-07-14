use proc_macro2::Span;
use std::path::{Path, PathBuf};

pub(crate) fn resolve_path(path: impl AsRef<Path>, err_span: Span) -> syn::Result<PathBuf> {
    let path = path.as_ref();

    if path.is_absolute() {
        return Err(syn::Error::new(
            err_span,
            "absolute paths will only work on the current machine",
        ));
    }

    // requires `proc_macro::SourceFile::path()` to be stable
    // https://github.com/rust-lang/rust/issues/54725
    if path.is_relative()
        && path
            .parent()
            .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return Err(syn::Error::new(
            err_span,
            "paths relative to the current file's directory are not currently supported",
        ));
    }

    let mut out_path = crate::manifest_dir().map_err(|e| syn::Error::new(err_span, e))?;

    out_path.push(path);

    Ok(out_path)
}
