use std::iter::{Extend, IntoIterator};

#[derive(Debug, Default)]
pub struct PgQueryResult {
    pub(super) rows_affected: u64,
}

impl PgQueryResult {
    pub fn rows_affected(&self) -> u64 {
        self.rows_affected
    }
}

impl Extend<PgQueryResult> for PgQueryResult {
    fn extend<T: IntoIterator<Item = PgQueryResult>>(&mut self, iter: T) {
        for elem in iter {
            self.rows_affected += elem.rows_affected;
        }
    }
}
