use accounts::AccountsManager;
use color_eyre::eyre;
use color_eyre::eyre::{Context, OptionExt};
use rand::distr::{Alphanumeric, SampleString};
use sqlx::Connection;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();

    let mut conn = sqlx::PgConnection::connect(
        // `env::var()` doesn't include the variable name in the error.
        &dotenvy::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?,
    )
    .await
    .wrap_err("could not connect to database")?;

    // Runs migration for `accounts` internally.
    let accounts = AccountsManager::setup(&mut conn, 1)
        .await
        .wrap_err("error initializing AccountsManager")?;

    payments::migrate(&mut conn)
        .await
        .wrap_err("error running payments migrations")?;

    // For simplicity's sake, imagine each of these might be invoked by different request routes
    // in a web application.

    // POST /account
    let user_email = format!("user{}@example.com", rand::random::<u32>());
    let user_password = Alphanumeric.sample_string(&mut rand::rng(), 16);

    // Requires an externally managed transaction in case any application-specific records
    // should be created after the actual account record.
    let mut txn = conn.begin().await?;

    let account_id = accounts
        // Takes ownership of the password string because it's sent to another thread for hashing.
        .create(&mut txn, &user_email, user_password.clone())
        .await
        .wrap_err("error creating account")?;

    txn.commit().await?;

    println!(
        "created account ID: {}, email: {user_email:?}, password: {user_password:?}",
        account_id.0
    );

    // POST /session
    // Log the user in.
    let session = accounts
        .create_session(&mut conn, &user_email, user_password.clone())
        .await
        .wrap_err("error creating session")?;

    // After this, session.session_token should then be returned to the client,
    // either in the response body or a `Set-Cookie` header.
    println!("created session token: {}", session.session_token.0);

    // POST /purchase
    // The client would then pass the session token to authenticated routes.
    // In this route, they're making some kind of purchase.

    // First, we need to ensure the session is valid.
    // `session.session_token` would be passed by the client in whatever way is appropriate.
    //
    // For a pure REST API, consider an `Authorization: Bearer` header instead of the request body.
    // With Axum, you can create a reusable extractor that reads the header and validates the session
    // by implementing `FromRequestParts`.
    //
    // For APIs where the browser is intended to be the primary client, using a session cookie
    // may be easier for the frontend. By setting the cookie with `HttpOnly: true`,
    // it's impossible for malicious Javascript on the client to access and steal the session token.
    let account_id = accounts
        .auth_session(&mut conn, &session.session_token.0)
        .await
        .wrap_err("error authenticating session")?
        .ok_or_eyre("session does not exist")?;

    let purchase_amount: rust_decimal::Decimal = "12.34".parse().unwrap();

    // Then, because the user is making a purchase, we record a payment.
    let payment = payments::create(&mut conn, account_id, "USD", purchase_amount)
        .await
        .wrap_err("error creating payment")?;

    println!("created payment: {payment:?}");

    let purchase_id = sqlx::query_scalar!(
        "insert into purchase(account_id, payment_id, amount) values ($1, $2, $3) returning purchase_id",
        account_id.0,
        payment.payment_id.0,
        purchase_amount
    )
    .fetch_one(&mut conn)
    .await
    .wrap_err("error creating purchase")?;

    println!("created purchase: {purchase_id}");

    conn.close().await?;

    Ok(())
}
