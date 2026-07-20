-- Create extra user to be used on runner
CREATE USER runner WITH SUPERUSER PASSWORD 'runner-password';

-- https://www.postgresql.org/docs/current/ltree.html
CREATE EXTENSION IF NOT EXISTS ltree;

-- https://www.postgresql.org/docs/current/cube.html
CREATE EXTENSION IF NOT EXISTS cube;

-- https://www.postgresql.org/docs/current/citext.html
CREATE EXTENSION IF NOT EXISTS citext;

-- https://www.postgresql.org/docs/current/hstore.html
CREATE EXTENSION IF NOT EXISTS hstore;

-- https://www.postgresql.org/docs/current/sql-createtype.html
CREATE TYPE status AS ENUM ('new', 'open', 'closed');

-- https://www.postgresql.org/docs/current/rowtypes.html#ROWTYPES-DECLARING
CREATE TYPE inventory_item AS
(
    name        TEXT,
    supplier_id INT,
    price       BIGINT
);

-- https://github.com/prisma/database-schema-examples/tree/master/postgres/basic-twitter#basic-twitter
CREATE TABLE tweet
(
    id         BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    text       TEXT        NOT NULL,
    owner_id   BIGINT
);

CREATE TABLE tweet_reply
(
    id         BIGSERIAL PRIMARY KEY,
    tweet_id   BIGINT      NOT NULL REFERENCES tweet(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    text       TEXT        NOT NULL,
    owner_id   BIGINT
);

CREATE TYPE float_range AS RANGE
(
    subtype = float8,
    subtype_diff = float8mi
);

CREATE TABLE products (
    product_no INTEGER,
    name TEXT,
    price NUMERIC CHECK (price > 0)
);

CREATE OR REPLACE PROCEDURE forty_two(INOUT forty_two INT = NULL)
    LANGUAGE plpgsql AS 'begin forty_two := 42; end;';

CREATE TABLE test_citext (
    foo CITEXT NOT NULL
);

CREATE SCHEMA IF NOT EXISTS foo;

CREATE TYPE foo."Foo" as ENUM ('Bar', 'Baz');

CREATE TABLE mytable(f HSTORE);

CREATE TABLE circles (
    c circle,
    EXCLUDE USING gist (c WITH &&)
);

CREATE DOMAIN positive_int AS integer CHECK (VALUE > 0);
CREATE DOMAIN percentage AS positive_int CHECK (VALUE <= 100);

CREATE TYPE person as (
    id int,
    age positive_int,
    percent percentage
);

CREATE TYPE leaf_composite AS (prim integer);
CREATE DOMAIN domain AS leaf_composite;
CREATE TYPE root_composite AS (domain domain);
