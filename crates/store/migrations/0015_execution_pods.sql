-- A fixed creation plan precedes POST; an observed UID never permits replacement.
CREATE TABLE execution_pod_plans (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    namespace_uid TEXT NOT NULL CHECK (namespace_uid ~ '^[A-Za-z0-9_-]{1,128}$'),
    namespace TEXT NOT NULL CHECK (namespace ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
    pod_name TEXT NOT NULL CHECK (pod_name ~ '^ac-[0-9a-f]{52}$'),
    plan_digest TEXT NOT NULL CHECK (plan_digest ~ '^sha256:[0-9a-f]{64}$'),
    manifest JSONB NOT NULL CHECK (jsonb_typeof(manifest)='object'),
    PRIMARY KEY (organization,execution_id),
    UNIQUE (namespace_uid,pod_name),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_dispatch_intents
);
CREATE TABLE execution_pod_observations (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    pod_uid TEXT NOT NULL CHECK (pod_uid ~ '^[A-Za-z0-9_-]{1,128}$'),
    PRIMARY KEY (organization,execution_id),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_pod_plans
);
CREATE TRIGGER immutable_execution_pod_plan BEFORE UPDATE OR DELETE ON execution_pod_plans
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TRIGGER immutable_execution_pod_observation BEFORE UPDATE OR DELETE ON execution_pod_observations
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_execution_pod_plan() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN execution_dispatch_intents i USING(organization,execution_id)
        JOIN candidate_writer_leases l ON l.organization=r.organization AND l.lease_id=r.lease_id AND l.epoch=r.epoch
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state='Dispatching' AND l.state='Held'
        AND i.deadline_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND l.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000))
        OR EXISTS(SELECT 1 FROM execution_startup_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR NEW.manifest->>'kind' IS DISTINCT FROM 'Pod' OR NEW.manifest->>'apiVersion' IS DISTINCT FROM 'v1'
        OR NEW.manifest->'metadata'->>'name' IS DISTINCT FROM NEW.pod_name
        OR NEW.manifest->'metadata'->>'namespace' IS DISTINCT FROM NEW.namespace THEN
        RAISE EXCEPTION 'execution Pod plan is not admitted';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_pod_plan BEFORE INSERT ON execution_pod_plans
    FOR EACH ROW EXECUTE FUNCTION guard_execution_pod_plan();
CREATE FUNCTION guard_execution_startup_pod() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM execution_pod_plans WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        AND NOT EXISTS(SELECT 1 FROM execution_pod_observations WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND pod_uid=NEW.pod_uid) THEN
        RAISE EXCEPTION 'execution Pod UID is not recorded';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_startup_pod BEFORE INSERT ON execution_startup_grants
    FOR EACH ROW EXECUTE FUNCTION guard_execution_startup_pod();
