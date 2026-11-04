

### Running Tests
SQLx uses docker to run many compatible database systems for integration testing. You'll need to [install docker](https://docs.docker.com/engine/) to run the full suite. You can validate your docker installation with:
    
    $ docker run hello-world

Start the databases with `docker compose` (or `docker-compose`) before running tests:

    $ docker compose up -d

Run clippy for the check matrix:

    $ ./x.py --clippy

This runs only the check/clippy matrix and skips unit and integration tests.

For the full test matrix, run all tests against all supported databases using:

    $ ./x.py

### Limiting the Matrix

The full matrix (runtimes, TLS backends, and DB versions) is large. Use the filters in `x.py` to keep runs small.

List all targets (tags):

    $ ./x.py --list-targets

Run by prefix (uses `tag.startswith`):

    $ ./x.py --target sqlite_tokio
    $ ./x.py --target postgres_17_tokio
    $ ./x.py --target mysql_8
    $ ./x.py --target mariadb_10_11

Note: integration tags do not include TLS, so a target like `postgres_17_tokio`
still runs all `TLS_VARIANTS`. To limit TLS locally, edit `TLS_VARIANTS` (and
`CHECK_TLS` for the check phase).

Run exactly one target:

    $ ./x.py --target-exact mysql_8_client_ssl_no_password_tokio

Run only one integration test binary:

    $ ./x.py --test sqlite
    $ ./x.py --test any

Pass extra args to cargo:

    $ ./x.py -- --nocapture

To shrink the matrix globally, edit the lists at the top of `tests/x.py`:
`CHECK_TLS`, `TLS_VARIANTS`, `POSTGRES_VERSIONS`, `MYSQL_VERSIONS`, `MARIADB_VERSIONS`.

If you see test failures, or want to run a more specific set of tests against a specific database, you can specify both the features to be tests and the DATABASE_URL. e.g.

    $ DATABASE_URL=mysql://root:password@127.0.0.1:49183/sqlx cargo test --no-default-features --features macros,offline,any,all-types,mysql,runtime-async-std-native-tls