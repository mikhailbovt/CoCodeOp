#!/bin/sh
# The official Postgres entrypoint may source this file, so keep settings local.
(
    set -eu
    COCODEOP_BOOTSTRAP_APP_PASSWORD=$(cat /run/secrets/cocodeop_database)
    export COCODEOP_BOOTSTRAP_APP_PASSWORD
    psql --no-psqlrc --set ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<'SQL'
\getenv app_password COCODEOP_BOOTSTRAP_APP_PASSWORD
CREATE ROLE cocodeop LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE PASSWORD :'app_password';
ALTER DATABASE cocodeop OWNER TO cocodeop;
REVOKE ALL ON DATABASE cocodeop FROM PUBLIC;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
SQL
)
