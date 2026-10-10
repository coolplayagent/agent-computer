-- New arms require two independent processes with one unchanged deadline.
-- Historical rows remain readable; they cannot authorize a new startup grant.
CREATE FUNCTION guard_redundant_watchdog() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    a JSONB := NEW.evidence->'armed';
    b JSONB := NEW.evidence->'backup_armed';
    p JSONB := NEW.evidence->'watchdog_pids';
    observed BIGINT := (NEW.evidence->>'observed_boottime_ms')::bigint;
    deadline BIGINT := (a->'request'->>'deadline_boottime_ms')::bigint;
BEGIN
    IF NEW.evidence->>'version' IS DISTINCT FROM '2'
        OR b->>'version' IS DISTINCT FROM '1'
        OR b->>'event' IS DISTINCT FROM 'armed'
        OR b->'request' IS DISTINCT FROM a->'request'
        OR b->'cgroup_device' IS DISTINCT FROM a->'cgroup_device'
        OR COALESCE((a->>'cgroup_device')::bigint,0)<=0
        OR jsonb_typeof(p) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'redundant watchdog is not admitted';
    END IF;
    IF jsonb_array_length(p)<>2
        OR COALESCE((p->>0)::bigint,0) NOT BETWEEN 1 AND 4294967295
        OR COALESCE((p->>1)::bigint,0) NOT BETWEEN 1 AND 4294967295
        OR (p->>0)::bigint=(p->>1)::bigint
        OR observed IS NULL OR deadline IS NULL OR observed>=deadline
        OR COALESCE((a->>'armed_boottime_ms')::bigint,0)<=0
        OR COALESCE((b->>'armed_boottime_ms')::bigint,0)<=0
        OR (a->>'armed_boottime_ms')::bigint>observed
        OR (b->>'armed_boottime_ms')::bigint>observed THEN
        RAISE EXCEPTION 'redundant watchdog is not admitted';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_redundant_watchdog BEFORE INSERT ON execution_watchdog_arms
    FOR EACH ROW EXECUTE FUNCTION guard_redundant_watchdog();
CREATE FUNCTION guard_redundant_startup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM execution_pod_plans WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        AND NOT EXISTS(SELECT 1 FROM execution_watchdog_arms w WHERE w.organization=NEW.organization
            AND w.execution_id=NEW.execution_id AND w.evidence->>'version'='2') THEN
        RAISE EXCEPTION 'redundant watchdog is not armed';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_redundant_startup BEFORE INSERT ON execution_startup_grants
    FOR EACH ROW EXECUTE FUNCTION guard_redundant_startup();
