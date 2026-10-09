CREATE TABLE principals (
    organization TEXT NOT NULL REFERENCES organization_streams,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    PRIMARY KEY (organization, principal)
);

CREATE TABLE service_credentials (
    credential_id TEXT PRIMARY KEY CHECK (credential_id ~ '^[0-9a-f]{32}$'),
    organization TEXT NOT NULL,
    principal TEXT NOT NULL,
    secret_hash BYTEA NOT NULL CHECK (octet_length(secret_hash) = 32),
    scopes TEXT[] NOT NULL CHECK (
        cardinality(scopes) BETWEEN 1 AND 2
        AND scopes <@ ARRAY['definitions.validate', 'definitions.manage']::TEXT[]
    ),
    issued_at TIMESTAMPTZ NOT NULL DEFAULT statement_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL CHECK (expires_at > issued_at),
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY (organization, principal) REFERENCES principals
);
CREATE INDEX principal_credentials ON service_credentials (organization, principal);
