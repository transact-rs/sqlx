use sha2::{Digest, Sha384};
use std::borrow::Cow;
use std::cmp::Ordering;

use crate::sql_str::SqlStr;

use super::MigrationType;

#[derive(Debug, Clone)]
pub struct Migration {
    pub version: i64,
    pub description: Cow<'static, str>,
    pub migration_type: MigrationType,
    pub sql: SqlStr,
    pub checksum: Cow<'static, [u8]>,
    pub no_tx: bool,
}

impl PartialEq for Migration {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version && self.migration_type == other.migration_type
    }
}

impl Eq for Migration {}

impl PartialOrd for Migration {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Migration {
    fn cmp(&self, other: &Self) -> Ordering {
        self.version
            .cmp(&other.version)
            .then_with(|| self.migration_type.cmp(&other.migration_type))
    }
}

impl Migration {
    pub fn new(
        version: i64,
        description: Cow<'static, str>,
        migration_type: MigrationType,
        sql: SqlStr,
        no_tx: bool,
    ) -> Self {
        let checksum = checksum(sql.as_str());

        Self::with_checksum(
            version,
            description,
            migration_type,
            sql,
            checksum.into(),
            no_tx,
        )
    }

    pub(crate) fn with_checksum(
        version: i64,
        description: Cow<'static, str>,
        migration_type: MigrationType,
        sql: SqlStr,
        checksum: Cow<'static, [u8]>,
        no_tx: bool,
    ) -> Self {
        Migration {
            version,
            description,
            migration_type,
            sql,
            checksum,
            no_tx,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: i64,
    pub checksum: Cow<'static, [u8]>,
}

pub fn checksum(sql: &str) -> Vec<u8> {
    Vec::from(Sha384::digest(sql).as_slice())
}

pub fn checksum_fragments<'a>(fragments: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut digest = Sha384::new();

    for fragment in fragments {
        digest.update(fragment);
    }

    digest.finalize().to_vec()
}

#[test]
fn fragments_checksum_equals_full_checksum() {
    // Copied from `examples/postgres/axum-social-with-tests/migrations/3_comment.sql`
    let sql = "\
        \u{FEFF}create table comment (\r\n\
            \tcomment_id uuid primary key default gen_random_uuid(),\r\n\
            \tpost_id uuid not null references post(post_id),\r\n\
            \tuser_id uuid not null references \"user\"(user_id),\r\n\
            \tcontent text not null,\r\n\
            \tcreated_at timestamptz not null default now()\r\n\
        );\r\n\
        \r\n\
        create index on comment(post_id, created_at);\r\n\
    ";

    // Should yield a string for each character
    let fragments_checksum = checksum_fragments(sql.split(""));
    let full_checksum = checksum(sql);

    assert_eq!(fragments_checksum, full_checksum);
}
