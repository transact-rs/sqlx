use crate::value::{MySqlValue, MySqlValueRef};
use crate::{
    MySqlArguments, MySqlColumn, MySqlConnection, MySqlQueryResult, MySqlRow, MySqlStatement,
    MySqlTransactionManager, MySqlTypeInfo,
};
pub(crate) use sqlx_core::database::{Database, HasStatementCache};

/// MySQL database driver.
#[derive(Debug)]
pub struct MySql;

impl Database for MySql {
    type Connection = MySqlConnection;

    type TransactionManager = MySqlTransactionManager;

    type Row = MySqlRow;

    type QueryResult = MySqlQueryResult;

    type Column = MySqlColumn;

    type TypeInfo = MySqlTypeInfo;

    type Value = MySqlValue;
    type ValueRef<'r> = MySqlValueRef<'r>;

    type Arguments = MySqlArguments;
    type ArgumentBuffer = Vec<u8>;

    type Statement = MySqlStatement;

    const NAME: &'static str = "MySQL";

    const URL_SCHEMES: &'static [&'static str] = &["mysql", "mariadb"];
}

impl HasStatementCache for MySql {}
