-- A local trusted node must arm the original Pod's cgroup before startup.
-- This immutable record is not a process result or a writer-drain certificate.
CREATE TABLE execution_watchdog_arms (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    pod_uid TEXT NOT NULL,
    node_uid TEXT NOT NULL,
    boot_id TEXT NOT NULL,
    container_id TEXT NOT NULL CHECK (container_id ~ '^[0-9a-f]{64}$'),
    cgroup_inode BIGINT NOT NULL CHECK (cgroup_inode>0),
    evidence JSONB NOT NULL CHECK (jsonb_typeof(evidence)='object'),
    evidence_digest TEXT NOT NULL CHECK (evidence_digest ~ '^sha256:[0-9a-f]{64}$'),
    registered_at_ms BIGINT NOT NULL CHECK (registered_at_ms>0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms>registered_at_ms AND expires_at_ms<=registered_at_ms+30000),
    PRIMARY KEY (organization,execution_id),
    UNIQUE (node_uid,boot_id,cgroup_inode),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_pod_observations
);
CREATE TRIGGER immutable_execution_watchdog BEFORE UPDATE OR DELETE ON execution_watchdog_arms
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_execution_watchdog() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN execution_dispatch_intents i USING(organization,execution_id)
        JOIN execution_pod_observations p USING(organization,execution_id)
        JOIN candidate_writer_leases l ON l.organization=r.organization AND l.lease_id=r.lease_id AND l.epoch=r.epoch
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state='Dispatching' AND l.state='Held'
        AND p.pod_uid=NEW.pod_uid AND NEW.registered_at_ms>=i.started_at_ms AND NEW.expires_at_ms<=i.deadline_at_ms
        AND NEW.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND l.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000))
        OR EXISTS(SELECT 1 FROM execution_startup_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR NEW.evidence->'runtime'->'identity'->>'pod_uid' IS DISTINCT FROM NEW.pod_uid
        OR NEW.evidence->'runtime'->'identity'->'node'->>'uid' IS DISTINCT FROM NEW.node_uid
        OR NEW.evidence->'runtime'->'identity'->'node'->>'boot_id' IS DISTINCT FROM NEW.boot_id
        OR NEW.evidence->'runtime'->'identity'->>'container_id' IS DISTINCT FROM NEW.container_id
        OR (NEW.evidence->'runtime'->>'cgroup_inode')::bigint IS DISTINCT FROM NEW.cgroup_inode
        OR NEW.evidence->'armed'->>'event' IS DISTINCT FROM 'armed'
        OR NEW.evidence->'armed'->>'version' IS DISTINCT FROM '1'
        OR NEW.evidence->'armed'->'request'->>'execution_id' IS DISTINCT FROM NEW.execution_id
        OR NEW.evidence->'armed'->'request'->>'boot_id' IS DISTINCT FROM NEW.boot_id
        OR (NEW.evidence->'armed'->'request'->>'cgroup_inode')::bigint IS DISTINCT FROM NEW.cgroup_inode
        OR NEW.evidence->'runtime'->>'cgroup_path' IS DISTINCT FROM NEW.evidence->'armed'->'request'->>'cgroup_path' THEN
        RAISE EXCEPTION 'execution watchdog is not admitted';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_watchdog BEFORE INSERT ON execution_watchdog_arms
    FOR EACH ROW EXECUTE FUNCTION guard_execution_watchdog();
CREATE FUNCTION guard_execution_startup_watchdog() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM execution_pod_plans WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        AND NOT EXISTS(SELECT 1 FROM execution_watchdog_arms w WHERE w.organization=NEW.organization AND w.execution_id=NEW.execution_id
            AND w.pod_uid=NEW.pod_uid AND NEW.granted_at_ms>=w.registered_at_ms
            AND w.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
            AND (NEW.grant_body->>'lease_budget_ms')::bigint BETWEEN 1 AND w.expires_at_ms-NEW.granted_at_ms) THEN
        RAISE EXCEPTION 'execution watchdog is not armed';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_startup_watchdog BEFORE INSERT ON execution_startup_grants
    FOR EACH ROW EXECUTE FUNCTION guard_execution_startup_watchdog();
