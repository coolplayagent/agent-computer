-- A live local round trip is required by the node handle. SQL binds its original
-- receipt to the immutable arm; stored JSON cannot recreate that handle.
CREATE FUNCTION guard_reaper_admission() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    r JSONB := NEW.evidence->'reaper';
    a JSONB := NEW.evidence->'armed';
    b JSONB := NEW.evidence->'backup_armed';
    j JSONB;
    observed BIGINT := (NEW.evidence->>'observed_boottime_ms')::bigint;
    acknowledged BIGINT := (r->>'observed_boottime_ms')::bigint;
BEGIN
    IF r->>'version' IS DISTINCT FROM '1'
        OR COALESCE(r->>'instance','') !~ '^[0-9a-f]{64}$'
        OR COALESCE(r->>'nonce','') !~ '^[0-9a-f]{64}$'
        OR COALESCE((r->>'pid')::bigint,0) NOT BETWEEN 1 AND 4294967295
        OR COALESCE((r->>'spool_device')::bigint,0)<=0
        OR COALESCE((r->>'spool_inode')::bigint,0)<=0
        OR r->'request' IS DISTINCT FROM a->'request'
        OR r->'cgroup_device' IS DISTINCT FROM a->'cgroup_device'
        OR jsonb_typeof(r->'journals') IS DISTINCT FROM 'array'
        OR acknowledged IS NULL OR acknowledged<=0
        OR observed IS NULL OR acknowledged>observed OR observed-acknowledged>200 THEN
        RAISE EXCEPTION 'reaper watchdog is not admitted';
    END IF;
    IF jsonb_array_length(r->'journals')<>2
        OR r->'journals'->0 IS DISTINCT FROM a->'journal'
        OR r->'journals'->1 IS DISTINCT FROM b->'journal'
        OR a->'journal' IS NOT DISTINCT FROM b->'journal' THEN
        RAISE EXCEPTION 'reaper watchdog is not admitted';
    END IF;
    FOR j IN SELECT value FROM jsonb_array_elements(r->'journals') LOOP
        IF COALESCE(j->>'id','') !~ '^journal-[a-zA-Z0-9_-]{1,56}$'
            OR COALESCE((j->>'device')::bigint,0)<=0
            OR COALESCE((j->>'inode')::bigint,0)<=0
            OR COALESCE(j->>'intent_digest','') !~ '^sha256:[0-9a-f]{64}$' THEN
            RAISE EXCEPTION 'reaper watchdog is not admitted';
        END IF;
    END LOOP;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_reaper_admission BEFORE INSERT ON execution_watchdog_arms
    FOR EACH ROW EXECUTE FUNCTION guard_reaper_admission();
CREATE UNIQUE INDEX execution_reaper_challenges ON execution_watchdog_arms
    (boot_id, (evidence->'reaper'->>'instance'), (evidence->'reaper'->>'nonce'))
    WHERE evidence ? 'reaper';
CREATE FUNCTION guard_reaper_startup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM execution_pod_plans WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        AND NOT EXISTS(SELECT 1 FROM execution_watchdog_arms w WHERE w.organization=NEW.organization
            AND w.execution_id=NEW.execution_id AND w.evidence->'reaper'->>'version'='1') THEN
        RAISE EXCEPTION 'reaper watchdog is not armed';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_reaper_startup BEFORE INSERT ON execution_startup_grants
    FOR EACH ROW EXECUTE FUNCTION guard_reaper_startup();
