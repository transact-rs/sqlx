ARG IMAGE
FROM ${IMAGE}

# Allow using alternate cert sets for older server images (e.g. MySQL 5.7).
ARG SSL_CERT_DIR=certs
ARG SSL_KEY_DIR=certs/keys

# Copy SSL certificate (and key)
COPY ${SSL_CERT_DIR}/server.crt /etc/mysql/ssl/server.crt
COPY ${SSL_CERT_DIR}/ca.crt /etc/mysql/ssl/ca.crt
COPY ${SSL_KEY_DIR}/server.key /etc/mysql/ssl/server.key
COPY mysql/my.cnf /etc/mysql/conf.d/sqlx-ssl.cnf

# Fix permissions
RUN chown mysql:mysql /etc/mysql/ssl/server.crt /etc/mysql/ssl/server.key
RUN chmod 0600 /etc/mysql/ssl/server.crt /etc/mysql/ssl/server.key

# Create dir for secure-file-priv
RUN mkdir -p /var/lib/mysql-files
