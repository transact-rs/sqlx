# SQLx CLI

SQLx's associated command-line utility for managing databases, migrations, and enabling "offline"
mode with `sqlx::query!()` and friends.

## Install

### With Rust toolchain

```bash
# supports all databases supported by SQLx
$ cargo install sqlx-cli

# only for postgres
$ cargo install sqlx-cli --no-default-features --features native-tls,postgres

# use vendored OpenSSL (build from source)
$ cargo install sqlx-cli --features openssl-vendored

# use Rustls rather than OpenSSL (be sure to add the features for the databases you intend to use!)
$ cargo install sqlx-cli --no-default-features --features rustls

# only for sqlite and use the system sqlite library
$ cargo install sqlx-cli --no-default-features --features sqlite-unbundled

# if you connect to MySQL/MariaDB without TLS and the server requires RSA auth
$ cargo install sqlx-cli --features mysql-rsa
```

Add `mysql-rsa` only for non-TLS MySQL/MariaDB connections that use
`caching_sha2_password` or `sha256_password`. If you use TLS, it is not needed.

## Usage

All commands require that a database url is provided. This can be done either with the `--database-url` command line option or by setting `DATABASE_URL`, either in the environment or in a `.env` file
in the current working directory.

For more details, run `sqlx <command> --help`.

```dotenv
# Postgres
DATABASE_URL=postgres://postgres@localhost/my_database
```

### Create/drop the database at `DATABASE_URL`

```bash
sqlx database create
sqlx database drop
```

---

### Create and run migrations

```bash
sqlx migrate add <name>
```

Creates a new file in `migrations/<timestamp>-<name>.sql`. Add your database schema changes to
this new file.

---

```bash
sqlx migrate run
```

Compares the migration history of the running database against the `migrations/` folder and runs
any scripts that are still pending.

---

Users can provide the directory for the migration scripts to `sqlx migrate` subcommands with the `--source` flag.

```bash
sqlx migrate info --source ../relative/migrations
```

---

### Reverting Migrations

If you would like to create _reversible_ migrations with corresponding "up" and "down" scripts, you use the `-r` flag when creating the first migration:

```bash
$ sqlx migrate add -r <name>
Creating migrations/20211001154420_<name>.up.sql
Creating migrations/20211001154420_<name>.down.sql
```

After that, you can run these as above:

```bash
$ sqlx migrate run
Applied migrations/20211001154420 <name> (32.517835ms)
```

And reverts work as well:

```bash
$ sqlx migrate revert
Applied 20211001154420/revert <name>
```

**Note**: All the subsequent migrations will be reversible as well.

```bash
$ sqlx migrate add <name1>
Creating migrations/20211001154420_<name>.up.sql
Creating migrations/20211001154420_<name>.down.sql
```

### Enable building in "offline mode" with `query!()`

There are 2 steps to building with "offline mode":

1. Save query metadata for offline usage
    - `cargo sqlx prepare`
2. Build

Note: Saving query metadata must be run as `cargo sqlx`.

```bash
cargo sqlx prepare
```

Invoking `prepare` saves query metadata to `.sqlx` in the current directory.
For workspaces where several crates are using query macros, pass the `--workspace` flag
to generate a single `.sqlx` directory at the root of the workspace.

```bash
cargo sqlx prepare --workspace
```

Check this directory into version control and an active database connection will 
no longer be needed to build your project.

---

```bash
cargo sqlx prepare --check
# OR
cargo sqlx prepare --check --workspace
```

Exits with a nonzero exit status if the data in `.sqlx` is out of date with the current
database schema or queries in the project. Intended for use in Continuous Integration.

### Force building in offline mode

The presence of a `DATABASE_URL` environment variable will take precedence over the presence of `.sqlx`, meaning SQLx will default to building against a database if it can. To make sure an accidentally-present `DATABASE_URL` environment variable or `.env` file does not
result in `cargo build` (trying to) access the database, you can set the `SQLX_OFFLINE` environment
variable to `true`.

If you want to make this the default, just add it to your `.env` file. `cargo sqlx prepare` will
still do the right thing and connect to the database.

### Include queries behind feature flags (such as queries inside of tests)

In order for sqlx to be able to find queries behind certain feature flags or in tests, you need to turn them
on by passing arguments to `cargo`.

This is how you would turn all targets and features on.

```bash
cargo sqlx prepare -- --all-targets --all-features
```
