use std::rc::Weak;
use crate::database::Database;
use crate::pool::Pool;
use crate::sync::AsyncOnceCell;

pub struct TestMasterPool<DB: Database> {
    inner: AsyncOnceCell<Inner<DB>>,
}

struct Inner<DB: Database> {
    pool: Pool<DB>,

    #[cfg(feature = "_rt-tokio")]

}


impl<DB: Database> TestMasterPool<DB> {

}
