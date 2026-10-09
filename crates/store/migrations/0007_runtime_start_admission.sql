-- Admission is durable metadata, not permission to dispatch or proof of readiness.
CREATE TABLE runtime_controls (
    organization TEXT NOT NULL,
    computer_id TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    active_request TEXT,
    PRIMARY KEY (organization,computer_id),
    FOREIGN KEY (organization,computer_id) REFERENCES resource_definitions
);

CREATE TABLE runtime_start_requests (
    organization TEXT NOT NULL,
    request_id TEXT NOT NULL,
    computer_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    credential_id TEXT NOT NULL REFERENCES service_credentials,
    generation BIGINT NOT NULL CHECK (generation > 0),
    candidate_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    volume_id TEXT NOT NULL,
    cpu_millis BIGINT NOT NULL CHECK (cpu_millis >= 0),
    memory_mib BIGINT NOT NULL CHECK (memory_mib >= 0),
    storage_bytes BIGINT NOT NULL CHECK (storage_bytes > 0),
    max_runtime_seconds INTEGER NOT NULL CHECK (max_runtime_seconds BETWEEN 1 AND 86400),
    queue_deadline_at_ms BIGINT NOT NULL CHECK (queue_deadline_at_ms > 0),
    snapshot JSONB NOT NULL,
    snapshot_digest TEXT NOT NULL CHECK (snapshot_digest ~ '^sha256:[0-9a-f]{64}$'),
    receipt JSONB NOT NULL,
    state TEXT NOT NULL DEFAULT 'Queued' CHECK (state IN ('Queued','Preparing','Cancelled')),
    PRIMARY KEY (organization,request_id),
    UNIQUE (organization,computer_id,generation),
    UNIQUE (organization,candidate_id),
    UNIQUE (organization,computer_id,request_id),
    FOREIGN KEY (organization,computer_id) REFERENCES runtime_controls,
    FOREIGN KEY (organization,workspace_id) REFERENCES resource_definitions,
    FOREIGN KEY (organization,volume_id) REFERENCES resource_definitions,
    FOREIGN KEY (organization,principal) REFERENCES principals
);
ALTER TABLE runtime_controls ADD CONSTRAINT runtime_active_request
    FOREIGN KEY (organization,computer_id,active_request)
    REFERENCES runtime_start_requests (organization,computer_id,request_id);

-- Queued requests reserve capacity too. An expired deadline or credential is
-- not evidence that a dispatched writer has stopped, so it cannot free capacity.
CREATE UNIQUE INDEX runtime_one_active_computer
    ON runtime_start_requests (organization,computer_id) WHERE state <> 'Cancelled';
CREATE UNIQUE INDEX runtime_one_active_workspace
    ON runtime_start_requests (organization,workspace_id) WHERE state <> 'Cancelled';
CREATE INDEX runtime_reserved_capacity
    ON runtime_start_requests (organization,principal,volume_id) WHERE state <> 'Cancelled';

CREATE FUNCTION guard_runtime_start_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runtime start history is immutable';
    END IF;
    IF (to_jsonb(NEW) - 'state') IS DISTINCT FROM (to_jsonb(OLD) - 'state')
       OR (NEW.state <> OLD.state AND NOT (OLD.state = 'Queued' AND NEW.state IN ('Preparing','Cancelled'))) THEN
        RAISE EXCEPTION 'invalid runtime start mutation';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER immutable_runtime_start BEFORE UPDATE OR DELETE ON runtime_start_requests
    FOR EACH ROW EXECUTE FUNCTION guard_runtime_start_mutation();
