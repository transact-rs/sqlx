from os import path

def setup(database_url, cwd, sqlx):
    sqlx("mig run", database_url, cwd=path.join(cwd, "accounts"))
    sqlx("mig run", database_url, cwd=path.join(cwd, "payments"))
