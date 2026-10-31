set -ex

mkdir -p keys

# Create self-signed CA certificate
openssl req -batch -x509 -sha256 -days 7 -newkey ed25519 -keyout keys/ca.key -nodes -out ca.crt

# Generate client key + CSR
# Postgres expects the Common Name (`CN`) field of the certificate to match the database username:
# <https://www.postgresql.org/docs/current/auth-cert.html>
openssl req -batch -newkey ed25519 -keyout keys/client.key -nodes -new -out client.csr -subj "/CN=postgres"

# Sign client CSR
openssl x509 -req -CA ca.crt -CAkey keys/ca.key -in client.csr -out client.crt -days 7 -CAcreateserial

# Generate server key + CSR
# This adds a required x509 v3 extension:
# * `subjectAltName=DNS:sqlx.rs` supplies the Subject Alternative Name that RusTLS uses to verify the hostname.
#   * Only checked if using SSL mode `ssl_mode=verify_identity` (MySQL/MariaDB) or `sslmode=verify-full` (Postgres).
openssl req -batch -newkey ed25519 -keyout keys/server.key -nodes -new -out server.csr -addext subjectAltName=DNS:sqlx.test

# Sign server CSR (note: `-copy_extensions copyall` is required for `subjectAltName` to be set
openssl x509 -req -CA ca.crt -CAkey keys/ca.key -in server.csr -out server.crt -days 7 -CAcreateserial -copy_extensions copyall

rm keys/ca.key ca.srl client.csr server.csr