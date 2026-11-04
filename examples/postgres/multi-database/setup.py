from os import path

def setup(database_url, cwd, sqlx):
    accounts_url = f"{database_url}-accounts"
    payments_url = f"{database_url}-payments"

    sqlx("db reset -y", accounts_url, cwd=path.join(cwd, "accounts"))
    sqlx("db reset -y", payments_url, cwd=path.join(cwd, "payments"))

    return {"ACCOUNTS_DATABASE_URL": accounts_url, "PAYMENTS_DATABASE_URL": payments_url}
