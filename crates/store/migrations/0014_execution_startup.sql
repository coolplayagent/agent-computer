-- A fresh grant answers exactly one runtime challenge. It is not a process
-- status or a drain certificate, and is never regenerated after a lost reply.
CREATE TABLE execution_startup_grants (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    pod_uid TEXT NOT NULL CHECK (pod_uid ~ '^[A-Za-z0-9_-]{1,128}$'),
    challenge JSONB NOT NULL CHECK (jsonb_typeof(challenge)='object'),
    grant_body JSONB NOT NULL CHECK (jsonb_typeof(grant_body)='object'),
    grant_digest TEXT NOT NULL CHECK (grant_digest ~ '^sha256:[0-9a-f]{64}$'),
    granted_at_ms BIGINT NOT NULL CHECK (granted_at_ms > 0),
    PRIMARY KEY (organization,execution_id),
    UNIQUE (organization,pod_uid),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_dispatch_intents
);
CREATE TRIGGER immutable_execution_startup BEFORE UPDATE OR DELETE ON execution_startup_grants
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_execution_startup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN execution_dispatch_intents i USING(organization,execution_id)
        JOIN candidate_writer_leases l ON l.organization=r.organization AND l.lease_id=r.lease_id AND l.epoch=r.epoch
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state='Dispatching' AND l.state='Held'
        AND l.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND i.deadline_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND NEW.granted_at_ms>=i.started_at_ms AND NEW.granted_at_ms<i.deadline_at_ms
        AND NEW.challenge->>'execution_id'=r.execution_id AND (NEW.challenge->>'generation')::bigint=(r.binding->>'generation')::bigint
        AND NEW.challenge->>'version'='1' AND NEW.grant_body->>'version'='1'
        AND (NEW.grant_body->>'lease_budget_ms')::bigint BETWEEN 1 AND i.deadline_at_ms-NEW.granted_at_ms) THEN
        RAISE EXCEPTION 'execution startup is not admitted';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_startup BEFORE INSERT ON execution_startup_grants
    FOR EACH ROW EXECUTE FUNCTION guard_execution_startup();
