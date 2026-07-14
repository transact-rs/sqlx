use crate::encode::{Encode, IsNull};
use crate::error::Error;
use crate::statement::StatementHandle;
use crate::Sqlite;
use atoi::atoi;
use libsqlite3_sys::SQLITE_OK;
use std::sync::Arc;

pub(crate) use sqlx_core::arguments::*;
use sqlx_core::error::BoxDynError;

#[derive(Debug, Clone)]
pub enum SqliteArgumentValue {
    Null,
    Text(Arc<String>),
    TextSlice(Arc<str>),
    Blob(Arc<Vec<u8>>),
    Double(f64),
    Int(i32),
    Int64(i64),
}

#[derive(Default, Debug, Clone)]
pub struct SqliteArguments {
    pub(crate) values: SqliteArgumentsBuffer,
}

#[derive(Default, Debug, Clone)]
pub struct SqliteArgumentsBuffer(Vec<SqliteArgumentValue>);

impl SqliteArguments {
    pub(crate) fn add<'t, T>(&mut self, value: T) -> Result<(), BoxDynError>
    where
        T: Encode<'t, Sqlite>,
    {
        let value_length_before_encoding = self.values.0.len();

        match value.encode(&mut self.values) {
            Ok(IsNull::Yes) => self.values.0.push(SqliteArgumentValue::Null),
            Ok(IsNull::No) => {}
            Err(error) => {
                // reset the value buffer to its previous value if encoding failed so we don't leave a half-encoded value behind
                self.values.0.truncate(value_length_before_encoding);
                return Err(error);
            }
        };

        Ok(())
    }
}

impl Arguments for SqliteArguments {
    type Database = Sqlite;

    fn reserve(&mut self, len: usize, _size_hint: usize) {
        self.values.0.reserve(len);
    }

    fn add<'t, T>(&mut self, value: T) -> Result<(), BoxDynError>
    where
        T: Encode<'t, Self::Database>,
    {
        self.add(value)
    }

    fn len(&self) -> usize {
        self.values.0.len()
    }
}

impl SqliteArguments {
    pub(super) fn bind(&self, handle: &mut StatementHandle, offset: usize) -> Result<usize, Error> {
        let mut arg_i = offset;
        // for handle in &statement.handles {

        let cnt = handle.bind_parameter_count();

        for param_i in 1..=cnt {
            // figure out the index of this bind parameter into our argument tuple
            let n: usize = if let Some(name) = handle.bind_parameter_name(param_i) {
                if let Some(name) = name.strip_prefix('?') {
                    // parameter should have the form ?NNN
                    atoi(name.as_bytes()).expect("parameter of the form ?NNN")
                } else if let Some(name) = name.strip_prefix('$') {
                    // parameter should have the form $NNN
                    atoi(name.as_bytes()).ok_or_else(|| {
                        err_protocol!(
                            "parameters with non-integer names are not currently supported: {}",
                            name
                        )
                    })?
                } else {
                    return Err(err_protocol!("unsupported SQL parameter format: {}", name));
                }
            } else {
                arg_i += 1;
                arg_i
            };

            if n > self.values.0.len() {
                // SQLite treats unbound variables as NULL
                // we reproduce this here
                // If you are reading this and think this should be an error, open an issue and we can
                // discuss configuring this somehow
                // Note that the query macros have a different way of enforcing
                // argument arity
                break;
            }

            self.values.0[n - 1].bind(handle, param_i)?;
        }

        Ok(arg_i - offset)
    }
}

impl SqliteArgumentsBuffer {
    #[allow(dead_code)] // clippy incorrectly reports this as unused
    pub(crate) fn new(values: Vec<SqliteArgumentValue>) -> SqliteArgumentsBuffer {
        Self(values)
    }

    pub(crate) fn push(&mut self, value: SqliteArgumentValue) {
        self.0.push(value);
    }
}

impl SqliteArgumentValue {
    fn bind(&self, handle: &mut StatementHandle, i: usize) -> Result<(), Error> {
        use SqliteArgumentValue::*;

        let status = match self {
            Text(v) => handle.bind_text(i, v),
            TextSlice(v) => handle.bind_text(i, v),
            Blob(v) => handle.bind_blob(i, v),
            Int(v) => handle.bind_int(i, *v),
            Int64(v) => handle.bind_int64(i, *v),
            Double(v) => handle.bind_double(i, *v),
            Null => handle.bind_null(i),
        };

        if status != SQLITE_OK {
            return Err(handle.last_error().into());
        }

        Ok(())
    }
}
