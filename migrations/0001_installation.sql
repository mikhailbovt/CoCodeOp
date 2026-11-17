CREATE TABLE office_installation (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    installation_id uuid NOT NULL UNIQUE DEFAULT gen_random_uuid(),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp()
);

INSERT INTO office_installation DEFAULT VALUES;
