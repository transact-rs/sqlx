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

// A `repr` without an integer type does not make a weak enum
#[derive(Debug, PartialEq, sqlx::Type)]
#[repr(C)]
enum ReprC {
    Foo,
    Bar,
}

test_type!(repr_c_enum<ReprC>(Sqlite,
    "'Foo'" == ReprC::Foo,
    "'Bar'" == ReprC::Bar,
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
#[sqlx(transparent)]
#[repr(transparent)]
struct TransparentRepr(i64);

test_type!(transparent_repr<TransparentRepr>(Sqlite,
    "0" == TransparentRepr(0),
    "23523" == TransparentRepr(23523)
));
