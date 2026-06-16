# SQLx on `wasm32-wasip2`

These examples show SQLx running inside a [WebAssembly Component] on the
`wasm32-wasip2` target, using a `sqlx::Pool` to connect to a database over the
host network via [`wasi:sockets`]. They run in any WASIP2 runtime; the
commands below use [Wasmtime].

| Example                  | Backend  |
| ------------------------ | -------- |
| [`postgres`](./postgres) | Postgres |
| [`mysql`](./mysql)       | MySQL    |

## How it works

The examples use the standard **`runtime-tokio`** feature with async I/O.
Tokio's `net` driver works on `wasm32-wasip2` (via `mio`'s WASI support).

Two target-specific requirements:

- **`--cfg tokio_unstable`**: Tokio's `net` support on `wasm32` is gated behind
  this cfg. It's set for the `wasm32-wasip2` target in
  [`.cargo/config.toml`](./.cargo/config.toml), so a plain
  `cargo build --target wasm32-wasip2` works (no `RUSTFLAGS` needed).
- **Current-thread runtime**: WASIP2 does not have threads, so each example uses
  `#[tokio::main(flavor = "current_thread")]`.

Each example opens a `Pool`, creates a table, inserts rows in a transaction, then
reads them back and aggregates — a small round-trip exercising DDL, bind
parameters, `fetch_all`, and `begin`/`commit`. `LISTEN`/`NOTIFY` and TLS
(`tls-rustls-*`) also work on this target; see the
[top-level README](../../README.md#webassembly-wasm32-wasip2).

## Prerequisites

```sh
rustup target add wasm32-wasip2
# Wasmtime: https://wasmtime.dev/
```

## Run

### Postgres

```sh
# A database to connect to:
docker run -d --name sqlx-wasip2-pg \
    -e POSTGRES_PASSWORD=password -e POSTGRES_DB=sqlx \
    -p 5432:5432 postgres:17

cd postgres
cargo build --target wasm32-wasip2 --release
wasmtime run -S inherit-network \
    --env DATABASE_URL="postgres://postgres:password@127.0.0.1:5432/sqlx" \
    target/wasm32-wasip2/release/sqlx-wasip2-postgres.wasm
```

Expected output:

```text
alice: 5
carol: 4
bob: 3
total votes: 12
connected via pool to: PostgreSQL 17.x ...
```

### MySQL

```sh
docker run -d --name sqlx-wasip2-mysql \
    -e MYSQL_ROOT_PASSWORD=password -e MYSQL_DATABASE=sqlx \
    -p 3306:3306 mysql:8

cd mysql
cargo build --target wasm32-wasip2 --release
wasmtime run -S inherit-network \
    --env DATABASE_URL="mysql://root:password@127.0.0.1:3306/sqlx" \
    target/wasm32-wasip2/release/sqlx-wasip2-mysql.wasm
```

Expected output:

```text
alice: 5
carol: 4
bob: 3
total votes: 12
connected via pool to: 8.x.x
```

> `wasmtime run -S inherit-network` grants the component access to the host
> network; it is required for outbound TCP. These examples connect by IP literal
> (`127.0.0.1`), so that flag alone is sufficient. Connecting by **hostname**
> additionally needs `--allow-ip-name-lookup` to permit WASI DNS resolution.

[WebAssembly Component]: https://component-model.bytecodealliance.org/
[`wasi:sockets`]: https://github.com/WebAssembly/wasi-sockets
[Wasmtime]: https://wasmtime.dev/
