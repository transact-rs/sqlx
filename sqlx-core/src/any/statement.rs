use crate::any::{Any, AnyArguments, AnyColumn, AnyTypeInfo};
use crate::column::ColumnIndex;
use crate::database::Database;
use crate::error::Error;
use crate::ext::ustr::UStr;
use crate::sql_str::SqlStr;
use crate::statement::Statement;
use crate::HashMap;
use either::Either;
use std::sync::Arc;

#[derive(Clone)]
pub struct AnyStatement {
    #[doc(hidden)]
    pub sql: SqlStr,
    #[doc(hidden)]
    pub parameters: Option<Either<Vec<AnyTypeInfo>, usize>>,
    #[doc(hidden)]
    pub column_names: Arc<HashMap<UStr, usize>>,
    #[doc(hidden)]
    pub columns: Vec<AnyColumn>,
}

impl Statement for AnyStatement {
    type Database = Any;

    fn into_sql(self) -> SqlStr {
        self.sql
    }

    fn sql(&self) -> &SqlStr {
        &self.sql
    }

    fn parameters(&self) -> Option<Either<&[AnyTypeInfo], usize>> {
        match &self.parameters {
            Some(Either::Left(types)) => Some(Either::Left(types)),
            Some(Either::Right(count)) => Some(Either::Right(*count)),
            None => None,
        }
    }

    fn columns(&self) -> &[AnyColumn] {
        &self.columns
    }

    impl_statement_query!(AnyArguments);
}

impl ColumnIndex<AnyStatement> for &'_ str {
    fn index(&self, statement: &AnyStatement) -> Result<usize, Error> {
        statement
            .column_names
            .get(*self)
            .ok_or_else(|| Error::ColumnNotFound((*self).into()))
            .copied()
    }
}

impl AnyStatement {
    #[doc(hidden)]
    pub fn try_from_statement<S>(
        statement: S,
        column_names: Arc<HashMap<UStr, usize>>,
    ) -> crate::Result<Self>
    where
        S: Statement,
        AnyTypeInfo: for<'a> TryFrom<&'a <S::Database as Database>::TypeInfo, Error = Error>,
        AnyColumn: for<'a> TryFrom<&'a <S::Database as Database>::Column, Error = Error>,
    {
        let parameters = match statement.parameters() {
            Some(Either::Left(parameters)) => Some(Either::Left(
                parameters
                    .iter()
                    .map(AnyTypeInfo::try_from)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            Some(Either::Right(count)) => Some(Either::Right(count)),
            None => None,
        };

        let columns = statement
            .columns()
            .iter()
            .map(AnyColumn::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            sql: statement.into_sql(),
            columns,
            column_names,
            parameters,
        })
    }
}
