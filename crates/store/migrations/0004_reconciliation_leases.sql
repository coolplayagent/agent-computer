-- Admission identity is persisted without retaining a bearer secret. Legacy
-- operations have no credential binding and fail closed at worker admission.
ALTER TABLE operations ADD COLUMN credential_id TEXT REFERENCES service_credentials;

ALTER TABLE reconcile_intents
    ADD COLUMN step_id TEXT,
    ADD COLUMN lease_epoch BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0),
    ADD COLUMN lease_owner TEXT,
    ADD COLUMN lease_until_ms BIGINT,
    ADD COLUMN available_at_ms BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN dispatch_started BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN reason_code TEXT,
    ADD COLUMN event_sequence BIGINT NOT NULL DEFAULT 0 CHECK (event_sequence >= 0);
UPDATE reconcile_intents SET step_id='step_' || md5(organization || ':' || operation_id || ':' || resource_id);
UPDATE reconcile_intents i SET event_sequence=o.event_sequence FROM operations o
    WHERE o.organization=i.organization AND o.operation_id=i.operation_id;
ALTER TABLE reconcile_intents ALTER COLUMN step_id SET NOT NULL;
ALTER TABLE reconcile_intents ADD CONSTRAINT reconcile_step_identity UNIQUE (organization,step_id);
ALTER TABLE reconcile_intents ADD CONSTRAINT reconcile_lease_pair
    CHECK ((lease_owner IS NULL) = (lease_until_ms IS NULL));
CREATE INDEX reconcile_pending ON reconcile_intents (organization,state,available_at_ms);

-- Every completed lease attempt retains its exact response, including retries.
-- Replaying a completion cannot change its meaning or overwrite a later attempt.
CREATE TABLE reconciliation_results (
    organization TEXT NOT NULL,
    step_id TEXT NOT NULL,
    lease_epoch BIGINT NOT NULL CHECK (lease_epoch > 0),
    input_digest TEXT NOT NULL,
    response JSONB NOT NULL,
    receipt JSONB,
    PRIMARY KEY (organization,step_id,lease_epoch),
    FOREIGN KEY (organization,step_id) REFERENCES reconcile_intents (organization,step_id)
);
CREATE TRIGGER immutable_reconciliation_result BEFORE UPDATE OR DELETE ON reconciliation_results
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
