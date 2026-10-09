-- A missing input is not an empty Workspace. Only new Workspace creation or an
-- explicit trusted initialization inserts a genesis input; no legacy backfill.
CREATE TABLE workspace_input_versions (
    organization TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    manifest JSONB NOT NULL,
    digest TEXT NOT NULL CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    origin TEXT NOT NULL CHECK (origin IN ('initial_empty')),
    PRIMARY KEY (organization,workspace_id,revision),
    FOREIGN KEY (organization,workspace_id) REFERENCES resource_definitions
);
CREATE TRIGGER immutable_workspace_input BEFORE UPDATE OR DELETE ON workspace_input_versions
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TABLE workspace_input_heads (
    organization TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    PRIMARY KEY (organization,workspace_id),
    FOREIGN KEY (organization,workspace_id,revision) REFERENCES workspace_input_versions
);
CREATE TABLE runtime_start_inputs (
    organization TEXT NOT NULL,
    request_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    PRIMARY KEY (organization,request_id),
    FOREIGN KEY (organization,request_id) REFERENCES runtime_start_requests,
    FOREIGN KEY (organization,workspace_id,revision) REFERENCES workspace_input_versions
);
CREATE TRIGGER immutable_start_input BEFORE UPDATE OR DELETE ON runtime_start_inputs
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

ALTER TABLE runtime_start_requests DROP CONSTRAINT runtime_start_requests_state_check;
ALTER TABLE runtime_start_requests ADD CONSTRAINT runtime_start_requests_state_check
    CHECK (state IN ('Queued','Preparing','Prepared','Cancelled'));
CREATE OR REPLACE FUNCTION guard_runtime_start_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'runtime start history is immutable'; END IF;
    IF (to_jsonb(NEW) - 'state') IS DISTINCT FROM (to_jsonb(OLD) - 'state')
       OR (NEW.state <> OLD.state AND NOT (
           (OLD.state = 'Queued' AND NEW.state IN ('Preparing','Cancelled'))
           OR (OLD.state = 'Preparing' AND NEW.state = 'Prepared'))) THEN
        RAISE EXCEPTION 'invalid runtime start mutation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TABLE candidate_preparations (
    organization TEXT NOT NULL,
    request_id TEXT NOT NULL,
    binding JSONB NOT NULL,
    request JSONB NOT NULL,
    request_digest TEXT NOT NULL,
    lease_epoch BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0),
    lease_owner TEXT,
    lease_until_ms BIGINT,
    dispatch_started BOOLEAN NOT NULL DEFAULT FALSE,
    receipt JSONB,
    reason TEXT,
    event_sequence BIGINT NOT NULL CHECK (event_sequence > 0),
    PRIMARY KEY (organization,request_id),
    FOREIGN KEY (organization,request_id) REFERENCES runtime_start_inputs,
    CHECK ((lease_owner IS NULL) = (lease_until_ms IS NULL)),
    CHECK (receipt IS NULL OR dispatch_started)
);
CREATE FUNCTION guard_candidate_preparation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'preparation identity is retained'; END IF;
    IF NEW.organization <> OLD.organization OR NEW.request_id <> OLD.request_id
       OR NEW.binding <> OLD.binding OR NEW.request <> OLD.request OR NEW.request_digest <> OLD.request_digest
       OR (OLD.dispatch_started AND NOT NEW.dispatch_started)
       OR (OLD.receipt IS NOT NULL AND NEW.receipt IS DISTINCT FROM OLD.receipt) THEN
        RAISE EXCEPTION 'preparation binding is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER immutable_candidate_preparation BEFORE UPDATE OR DELETE ON candidate_preparations
    FOR EACH ROW EXECUTE FUNCTION guard_candidate_preparation();
