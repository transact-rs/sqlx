use sqlx::{Error, MySql};
use sqlx_mysql::MySqlDatabaseError;
use std::io;

use sqlx_test::new;

// https://rustsec.org/advisories/RUSTSEC-2024-0363.html
//
// During the audit the MySQL driver was found to be *unlikely* to be vulnerable to the exploit,
// so this just serves as a sanity check.
#[sqlx::test]
async fn rustsec_2024_0363() -> anyhow::Result<()> {
    let overflow_len = 4 * 1024 * 1024 * 1024; // 4 GiB

    let padding = " ".repeat(overflow_len);

    let payload = "UPDATE injection_target SET message = 'you''ve been pwned!' WHERE id = 1";

    let mut injected_value = String::with_capacity(overflow_len + payload.len());

    injected_value.push_str(&padding);
    injected_value.push_str(payload);

    // Since this is so large, keeping it around until the end *can* lead to getting OOM-killed.
    drop(padding);

    let mut conn = new::<MySql>().await?;

    sqlx::raw_sql(
        "CREATE TEMPORARY TABLE injection_target(id INTEGER PRIMARY KEY AUTO_INCREMENT, message TEXT);\n\
         INSERT INTO injection_target(message) VALUES ('existing message');",
    )
        .execute(&mut conn)
        .await?;

    // We can't concatenate a query string together like the other tests
    // because it would just demonstrate a regular old SQL injection.
    let res = sqlx::query("INSERT INTO injection_target(message) VALUES (?)")
        .bind(&injected_value)
        .execute(&mut conn)
        .await;

    if let Err(e) = res {
        // Connection rejected the query; we're happy.
        //
        // If a packet exceeds `max_allowed_packet`, MySQL returns ER_NET_PACKET_TOO_LARGE
        // and closes the connection. Depending on timing, the client may instead observe
        // "Lost connection to MySQL server during query" or a local "Broken pipe" error.
        //
        // See: https://dev.mysql.com/doc/refman/8.4/en/packet-too-large.html
        match e {
            Error::Database(ref dbe) => {
                let err_net_packet_too_large = 1153;
                return match dbe.try_downcast_ref::<MySqlDatabaseError>() {
                    Some(error) if error.number() == err_net_packet_too_large => Ok(()),
                    _ => panic!("unexpected error: {e:?}"),
                };
            }
            Error::Io(ref ioe) if ioe.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            _ => panic!("unexpected error: {e:?}"),
        }
    }

    let messages: Vec<String> =
        sqlx::query_scalar("SELECT message FROM injection_target ORDER BY id")
            .fetch_all(&mut conn)
            .await?;

    assert_eq!(messages[0], "existing_message");
    assert_eq!(messages[1].len(), injected_value.len());

    // Injection didn't affect our database; we're happy.
    Ok(())
}
