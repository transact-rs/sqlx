use crate::database::Database;
use crate::error::Error;

use std::fmt::Debug;
use std::sync::Arc;

pub trait Column: 'static + Send + Sync + Debug {
    type Database: Database<Column = Self>;

    /// Gets the column ordinal.
    ///
    /// This can be used to unambiguously refer to this column within a row in case more than
    /// one column have the same name
    fn ordinal(&self) -> usize;

    /// Gets the column name or alias.
    ///
    /// The column name is unreliable (and can change between database minor versions) if this
    /// column is an expression that has not been aliased.
    fn name(&self) -> &str;

    /// Gets the type information for the column.
    fn type_info(&self) -> &<Self::Database as Database>::TypeInfo;

    /// If this column comes from a table, return the table and original column name.
    ///
    /// Returns [`ColumnOrigin::Expression`] if the column is the result of an expression
    /// or else the source table could not be determined.
    ///
    /// Returns [`ColumnOrigin::Unknown`] if the database driver does not have that information,
    /// or has not overridden this method.
    // This method returns an owned value instead of a reference,
    // to give the implementor more flexibility.
    fn origin(&self) -> ColumnOrigin {
        ColumnOrigin::Unknown
    }
}

/// A [`Column`] that originates from a table.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "offline", derive(serde::Serialize, serde::Deserialize))]
pub struct TableColumn {
    /// The name of the table (optionally schema-qualified) that the column comes from.
    pub table: Arc<str>,
    /// The original name of the column.
    pub name: Arc<str>,
}

/// The possible statuses for our knowledge of the origin of a [`Column`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "offline", derive(serde::Serialize, serde::Deserialize))]
pub enum ColumnOrigin {
    /// The column is known to originate from a table.
    ///
    /// Included is the table name and original column name.
    Table(TableColumn),
    /// The column originates from an expression, or else its origin could not be determined.
    Expression,
    /// The database driver does not know the column origin at this time.
    ///
    /// This may happen if:
    /// * The connection is in the middle of executing a query,
    ///   and cannot query the catalog to fetch this information.
    /// * The connection does not have access to the database catalog.
    /// * The implementation of [`Column`] did not override [`Column::origin()`].
    #[default]
    Unknown,
}

impl ColumnOrigin {
    /// Returns the true column origin, if known.
    pub fn table_column(&self) -> Option<&TableColumn> {
        if let Self::Table(table_column) = self {
            Some(table_column)
        } else {
            None
        }
    }
}

/// A type that can be used to index into a [`Row`] or [`Statement`].
///
/// The [`get`] and [`try_get`] methods of [`Row`] accept any type that implements `ColumnIndex`.
/// This trait is implemented for strings which are used to look up a column by name, and for
/// `usize` which is used as a positional index into the row.
///
/// *Caution*: The column index may differ between a [`Statement`] and a [`Row`] returned by the
/// statement. This can happen with some databases if, for example, the schema changes between
/// prepare and execute or if the database does not provide column information when the statement
/// is prepared.
///
/// [`Row`]: crate::row::Row
/// [`Statement`]: crate::statement::Statement
/// [`get`]: crate::row::Row::get
/// [`try_get`]: crate::row::Row::try_get
///
pub trait ColumnIndex<T: ?Sized>: Debug {
    /// Returns a valid positional index into the row or statement, [`ColumnIndexOutOfBounds`], or,
    /// [`ColumnNotFound`].
    ///
    /// [`ColumnNotFound`]: Error::ColumnNotFound
    /// [`ColumnIndexOutOfBounds`]: Error::ColumnIndexOutOfBounds
    fn index(&self, container: &T) -> Result<usize, Error>;
}

impl<T: ?Sized, I: ColumnIndex<T> + ?Sized> ColumnIndex<T> for &'_ I {
    #[inline]
    fn index(&self, row: &T) -> Result<usize, Error> {
        (**self).index(row)
    }
}

#[macro_export]
macro_rules! impl_column_index_for_row {
    ($R:ident) => {
        impl $crate::column::ColumnIndex<$R> for usize {
            fn index(&self, row: &$R) -> Result<usize, $crate::error::Error> {
                let len = $crate::row::Row::len(row);

                if *self >= len {
                    return Err($crate::error::Error::ColumnIndexOutOfBounds { len, index: *self });
                }

                Ok(*self)
            }
        }
    };
}

#[macro_export]
macro_rules! impl_column_index_for_statement {
    ($S:ident) => {
        impl $crate::column::ColumnIndex<$S> for usize {
            fn index(&self, statement: &$S) -> Result<usize, $crate::error::Error> {
                let len = $crate::statement::Statement::columns(statement).len();

                if *self >= len {
                    return Err($crate::error::Error::ColumnIndexOutOfBounds { len, index: *self });
                }

                Ok(*self)
            }
        }
    };
}
