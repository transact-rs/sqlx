/*
 * The smallest possible OAuth token validator, for testing PostgreSQL's `oauth`
 * authentication method (SASL OAUTHBEARER) against SQLx.
 *
 * PostgreSQL 18 ships no built-in validator and refuses to run an OAuth exchange without
 * one, because deciding whether a bearer token is good is the identity provider's business
 * and not the server's. A real validator checks a signature or calls an introspection
 * endpoint; this one compares the token against a constant, which is all a driver test
 * needs to tell an accepted token from a rejected one.
 *
 * See tests/postgres/oauth/Dockerfile.
 */

#include "postgres.h"

#include "fmgr.h"
#include "libpq/oauth.h"

PG_MODULE_MAGIC;

/* The only token this server accepts; `tests/postgres/oauth.rs` presents it. */
#define VALID_TOKEN "sqlx-test-token"

static bool validate_token(const ValidatorModuleState *state, const char *token,
						   const char *role, ValidatorModuleResult *result);

static const OAuthValidatorCallbacks validator_callbacks = {
	PG_OAUTH_VALIDATOR_MAGIC,

	.validate_cb = validate_token
};

const OAuthValidatorCallbacks *
_PG_oauth_validator_module_init(void)
{
	return &validator_callbacks;
}

static bool
validate_token(const ValidatorModuleState *state, const char *token,
			   const char *role, ValidatorModuleResult *result)
{
	result->authorized = (strcmp(token, VALID_TOKEN) == 0);

	/*
	 * The authenticated identity is the role from the startup packet, so that an accepted
	 * token logs in as whoever asked for it and pg_hba.conf needs no user map.
	 */
	result->authn_id = pstrdup(role);

	/* The token was examined; whether it passed is `result->authorized`. */
	return true;
}
