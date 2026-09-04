use sqlx::Sqlite;
use sqlx_test::test_type;

#[derive(Debug, PartialEq, sqlx::Type)]
#[repr(u32)]
enum Origin {
    Foo = 1,
    Bar = 2,
}

test_type!(origin_enum<Origin>(Sqlite,
    "1" == Origin::Foo,
    "2" == Origin::Bar,
));

#[derive(PartialEq, Eq, Debug, sqlx::Type)]
#[sqlx(transparent)]
struct TransparentTuple(i64);

#[derive(PartialEq, Eq, Debug, sqlx::Type)]
#[sqlx(transparent)]
struct TransparentNamed {
    field: i64,
}

test_type!(transparent_tuple<TransparentTuple>(Sqlite,
    "0" == TransparentTuple(0),
    "23523" == TransparentTuple(23523)
));

test_type!(transparent_named<TransparentNamed>(Sqlite,
    "0" == TransparentNamed { field: 0 },
    "23523" == TransparentNamed { field: 23523 },
));

#[derive(PartialEq, Eq, Debug, sqlx::Type)]
#[sqlx(try_from = "i64")]
struct TryFromI64(i64);

impl TryFrom<i64> for TryFromI64 {
    type Error = std::num::TryFromIntError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Ok(Self(i32::try_from(value)? as i64))
    }
}

test_type!(try_from_i64<TryFromI64>(Sqlite,
    "0" == TryFromI64(0),
    "23523" == TryFromI64(23523),
));
