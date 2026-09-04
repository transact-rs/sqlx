use std::borrow::Cow;
use std::env::var_os;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PGPassLineParseError {
    #[error("Unexpected end of line")]
    UnexpectedEOL,
}

/// Try to load a password from the various pgpass file locations.
///
/// Loading is attempted in the following order:
/// 1. Path given via the `PGPASSFILE` environment variable.
/// 2. Paths given via custom_paths.
/// 3. Default path (`~/.pgpass` on Linux and `%APPDATA%/postgres/pgpass.conf`
///    on Windows)
pub fn load_password(
    host: &str,
    port: u16,
    username: &str,
    database: Option<&str>,
    custom_paths: &[impl AsRef<Path>],
) -> Option<String> {
    let env_path = var_os("PGPASSFILE").map(PathBuf::from);
    let default_path = default_path();

    let path_iter = env_path
        .as_deref()
        .into_iter()
        .chain(custom_paths.iter().map(AsRef::as_ref))
        .chain(default_path.as_deref());

    path_iter
        .filter_map(|path| load_password_from_file(path, host, port, username, database))
        .next()
}

#[cfg(not(target_os = "windows"))]
fn default_path() -> Option<PathBuf> {
    // home_dir fixed in 1.85 (rust-lang/rust#132515) and un-deprecated in 1.87 (rust-lang/rust#137327)
    #[allow(deprecated)]
    std::env::home_dir().map(|path| path.join(".pgpass"))
}

#[cfg(target_os = "windows")]
fn default_path() -> Option<PathBuf> {
    use etcetera::BaseStrategy;

    etcetera::base_strategy::Windows::new()
        .ok()
        .map(|basedirs| basedirs.data_dir().join("postgresql").join("pgpass.conf"))
}

/// try to extract a password from a pgpass file
fn load_password_from_file(
    path: &Path,
    host: &str,
    port: u16,
    username: &str,
    database: Option<&str>,
) -> Option<String> {
    let file = File::open(path)
        .map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        path = %path.display(),
                        "`.pgpass` file not found",
                    );
                }
                _ => {
                    tracing::warn!(
                        path = %path.display(),
                        "Failed to open `.pgpass` file: {e:?}",
                    );
                }
            };
        })
        .ok()?;

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;

        // check file permissions on linux

        let metadata = file.metadata().ok()?;
        let permissions = metadata.permissions();
        let mode = permissions.mode();
        if mode & 0o77 != 0 {
            tracing::warn!(
                path = %path.display(),
                permissions = format!("{mode:o}"),
                "Ignoring path. Permissions are not strict enough",
            );
            return None;
        }
    }

    let reader = BufReader::new(file);
    load_password_from_reader(reader, host, port, username, database)
}

fn load_password_from_reader(
    mut reader: impl BufRead,
    host: &str,
    port: u16,
    username: &str,
    database: Option<&str>,
) -> Option<String> {
    let mut line = String::new();

    // https://stackoverflow.com/a/55041833
    fn trim_newline(s: &mut String) {
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
    }

    while let Ok(n) = reader.read_line(&mut line) {
        if n == 0 {
            break;
        }

        if line.starts_with('#') {
            // comment, do nothing
        } else {
            // try to load password from line
            trim_newline(&mut line);
            match load_password_from_line(&line, host, port, username, database) {
                Err(err) => {
                    tracing::warn!(line = line, "Malformed line in pgpass file: {err}");
                }
                Ok(Some(password)) => return Some(password),
                Ok(None) => (),
            }
        }

        line.clear();
    }

    None
}

/// try to check all fields & extract the password
fn load_password_from_line(
    mut line: &str,
    host: &str,
    port: u16,
    username: &str,
    database: Option<&str>,
) -> Result<Option<String>, PGPassLineParseError> {
    // Pgpass line ordering: hostname, port, database, username, password
    // See: https://www.postgresql.org/docs/9.3/libpq-pgpass.html

    if let None | Some('#') = line.trim_end().chars().next() {
        return Ok(None);
    }

    let line_matches = matches_next_field(&mut line, host)?
        && matches_next_field(&mut line, &port.to_string())?
        && matches_next_field(&mut line, database.unwrap_or_default())?
        && matches_next_field(&mut line, username)?;

    if !line_matches {
        return Ok(None);
    }

    Ok(Some(unescape_password(line)))
}

/// Unescape occurrences of `:` and `\` in the given password’s.
fn unescape_password(password_escaped: &str) -> String {
    let mut result = String::new();

    let mut it = password_escaped.chars();
    while let Some(char) = it.next() {
        if char != '\\' {
            result.push(char);
        } else if let Some(c) = it.next() {
            if c != ':' && c != '\\' {
                tracing::warn!("Superfluous escape in pgpass file");
            }
            result.push(c);
        } else {
            tracing::warn!("Superfluous escape at EOL in pgpass file");
        }
    }

    result
}

/// check if the next field matches the provided value
fn matches_next_field(line: &mut &str, value: &str) -> Result<bool, PGPassLineParseError> {
    let field = find_next_field(line)?;
    Ok(field == "*" || field == value)
}

/// extract the next value from a line in a pgpass file
///
/// `line` will get updated to point behind the field and delimiter
fn find_next_field<'a>(line: &mut &'a str) -> Result<Cow<'a, str>, PGPassLineParseError> {
    let mut escaping = false;
    let mut escaped_string = None;
    let mut last_added = 0;

    let char_indices = line.char_indices();
    for (idx, c) in char_indices {
        if c == ':' && !escaping {
            let (field, rest) = line.split_at(idx);
            *line = &rest[1..];

            if let Some(mut escaped_string) = escaped_string {
                escaped_string += &field[last_added..];
                return Ok(Cow::Owned(escaped_string));
            } else {
                return Ok(Cow::Borrowed(field));
            }
        } else if c == '\\' {
            let s = escaped_string.get_or_insert_with(String::new);

            if escaping {
                s.push('\\');
            } else {
                *s += &line[last_added..idx];
            }

            escaping = !escaping;
            last_added = idx + 1;
        } else {
            if escaping && c != '\\' && c != ':' {
                tracing::warn!("Superfluous escape in in pgpass file");
            }
            escaping = false;
        }
    }

    Err(PGPassLineParseError::UnexpectedEOL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn test_find_next_field() {
        fn test_case<'a>(
            mut input: &'a str,
            result: Result<Cow<'a, str>, PGPassLineParseError>,
            rest: &str,
        ) {
            assert_eq!(find_next_field(&mut input), result);
            assert_eq!(input, rest);
        }

        // normal field
        test_case("foo:bar:baz", Ok(Cow::Borrowed("foo")), "bar:baz");
        // \ escaped
        test_case(
            "foo\\\\:bar:baz",
            Ok(Cow::Owned("foo\\".to_owned())),
            "bar:baz",
        );
        // : escaped
        test_case(
            "foo\\::bar:baz",
            Ok(Cow::Owned("foo:".to_owned())),
            "bar:baz",
        );
        // unnecessary escape
        test_case(
            "foo\\a:bar:baz",
            Ok(Cow::Owned("fooa".to_owned())),
            "bar:baz",
        );
        // other text after escape
        test_case(
            "foo\\\\a:bar:baz",
            Ok(Cow::Owned("foo\\a".to_owned())),
            "bar:baz",
        );
        // double escape
        test_case(
            "foo\\\\\\\\a:bar:baz",
            Ok(Cow::Owned("foo\\\\a".to_owned())),
            "bar:baz",
        );
        // utf8 support
        test_case("🦀:bar:baz", Ok(Cow::Borrowed("🦀")), "bar:baz");

        // missing delimiter (eof)
        test_case("foo", Err(PGPassLineParseError::UnexpectedEOL), "foo");
        // missing delimiter after escape
        test_case("foo\\:", Err(PGPassLineParseError::UnexpectedEOL), "foo\\:");
        // missing delimiter after unused trailing escape
        test_case("foo\\", Err(PGPassLineParseError::UnexpectedEOL), "foo\\");
    }

    #[test]
    fn test_load_password_from_line() {
        // normal
        assert_eq!(
            load_password_from_line(
                "localhost:5432:bar:foo:baz",
                "localhost",
                5432,
                "foo",
                Some("bar"),
            ),
            Ok(Some("baz".to_owned()))
        );
        // wildcard
        assert_eq!(
            load_password_from_line("*:5432:bar:foo:baz", "localhost", 5432, "foo", Some("bar")),
            Ok(Some("baz".to_owned()))
        );
        // accept wildcard with missing db
        assert_eq!(
            load_password_from_line("localhost:5432:*:foo:baz", "localhost", 5432, "foo", None),
            Ok(Some("baz".to_owned()))
        );

        // doesn't match
        assert_eq!(
            load_password_from_line(
                "thishost:5432:bar:foo:baz",
                "thathost",
                5432,
                "foo",
                Some("bar")
            ),
            Ok(None)
        );
        // malformed entry
        assert_eq!(
            load_password_from_line(
                "localhost:5432:bar:foo",
                "localhost",
                5432,
                "foo",
                Some("bar")
            ),
            Err(PGPassLineParseError::UnexpectedEOL)
        );
        // Password with trailing whitespace
        assert_eq!(
            load_password_from_line("*:*:*:*:baz ", "localhost", 5432, "foo", Some("bar")),
            Ok(Some("baz ".to_owned()))
        );
        // Password with escaped colon
        assert_eq!(
            load_password_from_line("*:*:*:*:ba\\:z", "localhost", 5432, "foo", Some("bar")),
            Ok(Some("ba:z".to_owned()))
        );
        // Password with escaped backslash
        assert_eq!(
            load_password_from_line("*:*:*:*:ba\\\\z", "localhost", 5432, "foo", Some("bar")),
            Ok(Some("ba\\z".to_owned()))
        );
        // Password with superfluous escape
        assert_eq!(
            load_password_from_line("*:*:*:*:ba\\z", "localhost", 5432, "foo", Some("bar")),
            Ok(Some("baz".to_owned()))
        );
        // Password with trailing escape
        assert_eq!(
            load_password_from_line("*:*:*:*:baz\\", "localhost", 5432, "foo", Some("bar")),
            Ok(Some("baz".to_owned()))
        );
    }

    #[test]
    fn test_load_password_from_reader() {
        let file = b"\
            localhost:5432:bar:foo:baz\n\
            # mixed line endings (also a comment!)\n\
            *:5432:bar:foo:baz\r\n\
            # trailing space, comment with CRLF! \r\n\
            thishost:5432:bar:foo:baz \n\
            # malformed line \n\
            thathost:5432:foobar:foo\n\
            # missing trailing newline\n\
            localhost:5432:*:foo:baz
        ";

        // normal
        assert_eq!(
            load_password_from_reader(&mut &file[..], "localhost", 5432, "foo", Some("bar")),
            Some("baz".to_owned())
        );
        // wildcard
        assert_eq!(
            load_password_from_reader(&mut &file[..], "localhost", 5432, "foo", Some("foobar")),
            Some("baz".to_owned())
        );
        // accept wildcard with missing db
        assert_eq!(
            load_password_from_reader(&mut &file[..], "localhost", 5432, "foo", None),
            Some("baz".to_owned())
        );

        // doesn't match
        assert_eq!(
            load_password_from_reader(&mut &file[..], "thathost", 5432, "foo", Some("foobar")),
            None
        );
        // malformed entry
        assert_eq!(
            load_password_from_reader(&mut &file[..], "thathost", 5432, "foo", Some("foobar")),
            None
        );
    }
}
