-- Persist an observed external UID even while an effect is still provisioning.
-- Later worker attempts may inspect this identity, never replace or rebind it.
CREATE TABLE reconciliation_objects (
    organization TEXT NOT NULL,
    step_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (length(role) BETWEEN 1 AND 128),
    binding JSONB NOT NULL CHECK (jsonb_typeof(binding) = 'object'),
    event_sequence BIGINT NOT NULL CHECK (event_sequence > 0),
    PRIMARY KEY (organization,step_id,role),
    FOREIGN KEY (organization,step_id) REFERENCES reconcile_intents (organization,step_id)
);
CREATE TRIGGER immutable_reconciliation_object BEFORE UPDATE OR DELETE ON reconciliation_objects
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
