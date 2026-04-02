# TODOs Example

## Setup

1. Declare the database URL

    ```
    export DATABASE_URL="mysql://root:password@localhost/todos"
    ```

2. Create the database.

    ```
    $ sqlx db create
    ```

3. Run sql migrations

    ```
    $ sqlx migrate run
    ```

## Usage

Add a todo 

```
cargo run -- add "todo description"
```

Complete a todo.

```
cargo run -- done <todo id>
```

List all todos

```
cargo run
```
