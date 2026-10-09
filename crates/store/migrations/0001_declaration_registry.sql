-- A row lock on this counter is held until the entire write commits. PostgreSQL
-- sequences would allocate values before commit and allow replay to miss writes.
CREATE TABLE organization_streams (
    organization TEXT PRIMARY KEY,
    last_sequence BIGINT NOT NULL DEFAULT 0 CHECK (last_sequence >= 0),
    replay_floor BIGINT NOT NULL DEFAULT 0 CHECK (replay_floor >= 0 AND replay_floor <= last_sequence)
);

CREATE TABLE declaration_heads (
    organization TEXT NOT NULL REFERENCES organization_streams,
    name TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    PRIMARY KEY (organization, name)
);

CREATE TABLE declaration_versions (
    organization TEXT NOT NULL,
    name TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    digest TEXT NOT NULL CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    canonical BYTEA NOT NULL CHECK (octet_length(canonical) > 0),
    PRIMARY KEY (organization, name, revision),
    FOREIGN KEY (organization, name) REFERENCES declaration_heads DEFERRABLE INITIALLY DEFERRED
);

ALTER TABLE declaration_heads ADD CONSTRAINT head_version
    FOREIGN KEY (organization, name, revision)
    REFERENCES declaration_versions DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION reject_declaration_version_mutation() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'declaration versions are immutable';
END;
$$;
CREATE TRIGGER immutable_declaration_version BEFORE UPDATE OR DELETE ON declaration_versions
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE TABLE request_records (
    organization TEXT NOT NULL REFERENCES organization_streams,
    principal TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_key TEXT NOT NULL,
    input_digest BYTEA NOT NULL CHECK (octet_length(input_digest) = 32),
    response JSONB,
    retired BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (organization, principal, operation, request_key),
    CHECK ((retired AND response IS NULL) OR (NOT retired AND response IS NOT NULL))
);

CREATE TABLE events (
    organization TEXT NOT NULL REFERENCES organization_streams,
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT transaction_timestamp(),
    PRIMARY KEY (organization, sequence)
);

CREATE TABLE outbox (
    organization TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    acknowledged BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (organization, sequence),
    FOREIGN KEY (organization, sequence) REFERENCES events
);
CREATE INDEX pending_outbox ON outbox (organization, sequence) WHERE NOT acknowledged;
