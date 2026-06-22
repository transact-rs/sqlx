create table uuids (uuid text);

-- The `uuid4` function is provided by the
-- [uuid](https://github.com/nalgeon/sqlean/blob/main/docs/uuid.md)
-- sqlite extension, and so this migration can not run if that
-- extension is not loaded.
insert into uuids (uuid) values
  (uuid4()),
  (uuid4()),
  (uuid4());
