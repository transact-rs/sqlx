# PostgreSQL with the `oauth` HBA method configured, for the SASL OAUTHBEARER tests.
#
# The server hands token validation to a module it loads at authentication time and ships no
# default one, so the image builds the validator in `sqlx_oauth_validator.c` first. The
# compiler is left behind in the build stage.
ARG VERSION=18

FROM postgres:${VERSION} AS build
ARG VERSION

# `postgresql-server-dev-*` comes from the same PGDG repository this image is built from, so
# the module is compiled against exactly the server that will load it. The rest are headers
# that `libpq/oauth.h` reaches through `libpq-be.h`, which describes a server built with
# GSSAPI, OpenSSL and LDAP support.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        "postgresql-server-dev-${VERSION}" \
        libkrb5-dev \
        libldap2-dev \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY postgres/oauth/Makefile postgres/oauth/sqlx_oauth_validator.c /build/
RUN make -C /build install

FROM postgres:${VERSION}
ARG VERSION

COPY --from=build \
    "/usr/lib/postgresql/${VERSION}/lib/sqlx_oauth_validator.so" \
    "/usr/lib/postgresql/${VERSION}/lib/"
COPY postgres/oauth/pg_hba.conf /var/lib/postgresql/pg_hba.conf
