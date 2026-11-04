# SQLx Test TLS Certificates

This directory contains the following files for testing TLS connections.

* `ca.crt`: Self-signed Certificate Authority for `client.crt`
* `client.crt`: Client certificate signed by `ca.crt`
* `server.crt`: Server certificate signed by `ca.crt`

These certificates are **not** to be used outside of testing with SQLx.
The private keys are publicly available in the `keys` directory.

These certificates should be valid until the year 2035.

RusTLS requires TLS certificates to be x509v3. 
OpenSSL 3.2 and up create v3 certificates by default.

### MySQL 5.7 (RSA)

The default test certificates in this directory use Ed25519, which MySQL 5.7
cannot load. We keep a separate RSA CA/client/server set under
`tests/certs/rsa` and use it only for the MySQL 5.7 client-SSL targets.

## (Re)generating

When generating certificates, OpenSSL prompts for a number of fields:

```
You are about to be asked to enter information that will be incorporated
into your certificate request.
What you are about to enter is what is called a Distinguished Name or a DN.
There are quite a few fields but you can leave some blank
For some fields there will be a default value,
If you enter '.', the field will be left blank.
-----
Country Name (2 letter code) [AU]:
State or Province Name (full name) [Some-State]:
Locality Name (eg, city) []:
Organization Name (eg, company) [Internet Widgits Pty Ltd]:
Organizational Unit Name (eg, section) []:
Common Name (e.g. server FQDN or YOUR name) []:
Email Address []:
```

These are purely informational and can _generally_ be left blank or filled with arbitrary values 
(except for `Common Name` with client certificates; see below for details).

### CA Certificate

Create a self-signed root certificate:

```shell
openssl req -x509 -sha256 -days 3650 -key keys/ca.key -out ca.crt
```

This is passed as a trust root when testing certificate authentication.

### Client Certificate

**Note**: Postgres expects the Common Name (`CN`) field of the certificate to match the database username: 
<https://www.postgresql.org/docs/current/auth-cert.html>

At the prompt `Common Name (e.g. server FQDN or YOUR name) []:`, enter `postgres`.

Create a certificate signing request (CSR) to "submit" to our fake certificate authority:

```
openssl req -key keys/client.key -new -out client.csr
```

Create a signed certificate using our CA key and the CSR:

```
openssl x509 -req -CA ca.crt -CAkey keys/ca.key -in client.csr -out client.crt -days 3650 -CAcreateserial
```

### Server Certificate


Create a certificate signing request (CSR) to "submit" to our fake certificate authority:

```
openssl req -key keys/server.key -new -out server.csr -addext subjectAltName=DNS:sqlx.rs
```

This adds a required x509 v3 extension:
* `subjectAltName=DNS:sqlx.rs` supplies the Subject Alternative Name that RusTLS uses to verify the hostname.
  * Only checked if using SSL mode `ssl_mode=verify_identity` (MySQL/MariaDB) or `sslmode=verify-full` (Postgres).

Create a signed certificate using our CA key and the CSR:

```
openssl x509 -req -CA ca.crt -CAkey keys/ca.key -in server.csr -out server.crt -days 3650 -CAcreateserial
```
