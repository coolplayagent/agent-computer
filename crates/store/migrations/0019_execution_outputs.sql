-- Immutable output publication intents and verified object references. These
-- records never authorize execution completion, writer release or grant replay.
CREATE TABLE execution_output_intents (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    pod_uid TEXT NOT NULL,
    dispatch_digest TEXT NOT NULL,
    grant_digest TEXT NOT NULL,
    arm_digest TEXT NOT NULL,
    manifest JSONB NOT NULL CHECK (jsonb_typeof(manifest)='object'),
    manifest_digest TEXT NOT NULL CHECK (manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms>0),
    PRIMARY KEY (organization,execution_id),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_startup_grants
);
CREATE TABLE execution_outputs (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    verified_at_ms BIGINT NOT NULL CHECK (verified_at_ms>0),
    PRIMARY KEY (organization,execution_id),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_output_intents
);
CREATE TRIGGER immutable_execution_output_intent BEFORE UPDATE OR DELETE ON execution_output_intents
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TRIGGER immutable_execution_output BEFORE UPDATE OR DELETE ON execution_outputs
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_execution_output() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE m JSONB;
    o JSONB;
BEGIN
    IF TG_TABLE_NAME='execution_outputs' THEN
        IF NOT EXISTS(SELECT 1 FROM execution_output_intents i WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id
            AND i.manifest_digest=NEW.manifest_digest AND NEW.verified_at_ms>=i.created_at_ms) THEN
            RAISE EXCEPTION 'output publication does not match its intent';
        END IF;
        RETURN NEW;
    END IF;
    IF NOT EXISTS(SELECT 1 FROM execution_dispatch_intents d JOIN execution_startup_grants g USING(organization,execution_id)
        JOIN execution_watchdog_arms w USING(organization,execution_id)
        WHERE d.organization=NEW.organization AND d.execution_id=NEW.execution_id AND d.intent_digest=NEW.dispatch_digest
        AND g.pod_uid=NEW.pod_uid AND w.pod_uid=NEW.pod_uid AND g.grant_digest=NEW.grant_digest AND w.evidence_digest=NEW.arm_digest)
        OR NEW.manifest->>'version' IS DISTINCT FROM '1'
        OR NEW.manifest->>'organization' IS DISTINCT FROM NEW.organization
        OR NEW.manifest->>'execution_id' IS DISTINCT FROM NEW.execution_id
        OR NEW.manifest->>'pod_uid' IS DISTINCT FROM NEW.pod_uid
        OR NEW.manifest->>'dispatch_digest' IS DISTINCT FROM NEW.dispatch_digest
        OR NEW.manifest->>'grant_digest' IS DISTINCT FROM NEW.grant_digest
        OR NEW.manifest->>'arm_digest' IS DISTINCT FROM NEW.arm_digest
        OR jsonb_typeof(NEW.manifest->'objects') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'execution output binding does not match';
    END IF;
    m:=NEW.manifest;
    IF jsonb_array_length(m->'objects')<>4 THEN RAISE EXCEPTION 'output object set is incomplete'; END IF;
    FOR o IN SELECT value FROM jsonb_array_elements(m->'objects') LOOP
        IF COALESCE(o->>'store_digest','') !~ '^sha256:[0-9a-f]{64}$'
            OR COALESCE(o->>'sha256','') !~ '^sha256:[0-9a-f]{64}$'
            OR COALESCE((o->>'size')::bigint,-1) NOT BETWEEN 0 AND 8404992
            OR o->>'store_digest' IS DISTINCT FROM m->'objects'->0->>'store_digest'
            OR o->>'key' IS DISTINCT FROM 'execution-outputs/v1/'||NEW.organization||'/'||NEW.execution_id||'/'||substring(o->>'sha256' from 8) THEN
            RAISE EXCEPTION 'invalid execution output object';
        END IF;
    END LOOP;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_output_intent BEFORE INSERT ON execution_output_intents
    FOR EACH ROW EXECUTE FUNCTION guard_execution_output();
CREATE TRIGGER check_execution_output BEFORE INSERT ON execution_outputs
    FOR EACH ROW EXECUTE FUNCTION guard_execution_output();
