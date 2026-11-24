mod date;
mod datetime;
// Parent module is named after the `jiff` crate, this module is named after the `TIME` SQL type.
pub(crate) mod time;
mod timestamp;

use jiff::civil::{self, Date};
use jiff::{SignedDuration, Timestamp};

const PG_EPOCH: Date = civil::date(2000, 1, 1);
const PG_EPOCH_DATETIME: civil::DateTime = PG_EPOCH.at(0, 0, 0, 0);
// 2000-01-01T00:00:00Z
const PG_EPOCH_TIMESTAMP: Timestamp = Timestamp::constant(946_684_800, 0);

/// Convert a duration to whole microseconds, as used by the Postgres binary format.
///
/// Jiff has nanosecond precision while Postgres has microsecond precision,
/// so any sub-microsecond component is truncated towards negative infinity
/// (so the encoded value never lies _after_ the original value).
fn to_micros(duration: SignedDuration) -> Option<i64> {
    i64::try_from(duration.as_nanos().div_euclid(1000)).ok()
}
