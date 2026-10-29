# Built with PGXS inside tests/postgres/oauth/Dockerfile, which supplies pg_config.
MODULES = sqlx_oauth_validator

PG_CONFIG = pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)
