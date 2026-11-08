set -ex

mkdir -p keys

# Create self-signed CA certificate
openssl req -batch -x509 -sha256 -days 7 -newkey ed25519 -keyout keys/ca.key -nodes -out ca.crt -subj "/C=US/ST=CA/O=transact-rs/CN=ca.sql.test"

# Generate client key + CSR
# Postgres expects the Common Name (`CN`) field of the certificate to match the database username:
# <https://www.postgresql.org/docs/current/auth-cert.html>
#
# `-addext basicConstraints=ca:FALSE` is only added here to force the generation of an x509v3 certificate,
# which is not the default before OpenSSL 3.2 (GitHub's `ubuntu-24.04` runner uses OpenSSL 3.0)
openssl req -batch -newkey ed25519 -keyout keys/client.key -nodes -new -out client.csr -subj "/C=US/ST=CA/O=transact-rs/CN=postgres" -addext basicConstraints=CA:FALSE

# Sign client CSR (note: `-copy_extensions copyall` is required for v3 certificate gen)
openssl x509 -req -CA ca.crt -CAkey keys/ca.key -in client.csr -out client.crt -days 7 -CAcreateserial -copy_extensions copyall

# Generate server key + CSR
# This adds a required x509 v3 extension:
# * `subjectAltName=DNS:sqlx.test` supplies the Subject Alternative Name that RusTLS uses to verify the hostname.
#   * Only checked if using SSL mode `ssl_mode=verify_identity` (MySQL/MariaDB) or `sslmode=verify-full` (Postgres).
openssl req -batch -newkey ed25519 -keyout keys/server.key -nodes -new -out server.csr -subj "/C=US/ST=CA/O=transact-rs/CN=sqlx.test" -addext subjectAltName=DNS:sqlx.test

# Sign server CSR (note: `-copy_extensions copyall` is required for `subjectAltName` to be set)
openssl x509 -req -CA ca.crt -CAkey keys/ca.key -in server.csr -out server.crt -days 7 -CAcreateserial -copy_extensions copyall

rm keys/ca.key ca.srl client.csr server.csr

echo '================ CA Certificate ================'
openssl x509 -text -in ca.crt

echo '================ Server Certificate ================'
openssl x509 -text -in server.crt

echo '================ Client Certificate ================'
openssl x509 -text -in client.crt
