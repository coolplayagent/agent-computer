ALTER TABLE artifact_commits ADD COLUMN cancel_running BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE artifact_commits ADD CHECK (NOT cancel_running OR stop_after_commit);
ALTER TABLE artifact_commits DROP CONSTRAINT artifact_commits_state_check;
ALTER TABLE artifact_commits ADD CHECK (state IN ('Draining','Capturing','Committed','Conflict'));
-- Replace exactly the three pending/publication nullability checks. Preserve
-- every unrelated constraint, including lease ownership and all references.
DO $$ DECLARE c RECORD; removed INTEGER := 0; BEGIN
    FOR c IN SELECT conname FROM pg_constraint WHERE conrelid='artifact_commits'::regclass
        AND contype='c' AND pg_get_constraintdef(oid) LIKE '%state = ''Capturing''%' LOOP
        EXECUTE format('ALTER TABLE artifact_commits DROP CONSTRAINT %I',c.conname);
        removed:=removed+1;
    END LOOP;
    IF removed<>3 THEN RAISE EXCEPTION 'unexpected artifact nullability constraints'; END IF;
END $$;
ALTER TABLE artifact_commits ADD CONSTRAINT artifact_pending_timestamp
    CHECK ((state IN ('Draining','Capturing'))=(published_at_ms IS NULL));
ALTER TABLE artifact_commits ADD CONSTRAINT artifact_pending_object
    CHECK ((state IN ('Draining','Capturing'))=(object_ref IS NULL));
ALTER TABLE artifact_commits ADD CONSTRAINT artifact_pending_input
    CHECK ((state IN ('Draining','Capturing'))=(input_revision IS NULL));
ALTER TABLE artifact_commits ADD CONSTRAINT artifact_drain_pending
    CHECK (state<>'Draining' OR (cancel_running AND capture IS NULL AND lease_owner IS NULL));
DROP INDEX artifact_worker_queue;
CREATE INDEX artifact_worker_queue ON artifact_commits (organization,commit_id)
    WHERE state IN ('Draining','Capturing');
ALTER TABLE runtime_start_requests DROP CONSTRAINT runtime_start_requests_state_check;
ALTER TABLE runtime_start_requests ADD CHECK (state IN ('Queued','Preparing','Prepared','Draining','Sealing','Sealed','Cancelled','Stopped'));

CREATE FUNCTION checkpoint_stop_eligible(org TEXT, request TEXT, actor TEXT) RETURNS boolean LANGUAGE sql AS $$
    SELECT EXISTS (SELECT 1 FROM runtime_start_requests r
        CROSS JOIN LATERAL jsonb_array_elements(r.snapshot->'resources') resource
        WHERE r.organization=$1 AND r.request_id=$2
            AND resource#>>'{reference,kind}'='computer' AND resource#>>'{reference,resource_id}'=r.computer_id
            AND jsonb_array_length(resource#>'{spec,appRefs}')=0
            AND NOT checkpoint_stop_active_use($1,$2,$3));
$$;
CREATE OR REPLACE FUNCTION guard_checkpoint_stop_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.stop_after_commit AND NOT (
        CASE WHEN NEW.cancel_running THEN checkpoint_stop_eligible(NEW.organization,NEW.request_id,NEW.principal)
        ELSE checkpoint_stop_available(NEW.organization,NEW.request_id,NEW.principal) END) THEN
        RAISE EXCEPTION 'checkpoint stop is blocked';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_artifact_commit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'artifact history is retained'; END IF;
    IF TG_OP='INSERT' THEN
        SELECT * INTO STRICT head FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id FOR UPDATE;
        IF head.state<>'Prepared' OR head.workspace_id<>NEW.workspace_id OR NEW.capture IS NOT NULL
            OR (NEW.cancel_running AND (NOT NEW.stop_after_commit OR NEW.state<>'Draining'))
            OR (NOT NEW.cancel_running AND (NEW.state<>'Capturing' OR NOT artifact_candidate_drained(NEW.organization,NEW.request_id))) THEN
            RAISE EXCEPTION 'candidate is not ready to seal';
        END IF;
    ELSE
        IF (to_jsonb(NEW)-ARRAY['credential_id','state','capture','object_ref','input_revision','published_at_ms','lease_epoch','lease_owner','lease_until_ms'])
            IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['credential_id','state','capture','object_ref','input_revision','published_at_ms','lease_epoch','lease_owner','lease_until_ms'])
            OR OLD.state NOT IN ('Draining','Capturing') OR NEW.lease_epoch<OLD.lease_epoch
            OR (OLD.capture IS NOT NULL AND NEW.capture IS DISTINCT FROM OLD.capture)
            OR (OLD.state='Capturing' AND NEW.state='Draining')
            OR (OLD.state='Draining' AND (NEW.state NOT IN ('Draining','Capturing') OR NEW.capture IS NOT NULL
                OR NEW.lease_owner IS NOT NULL OR (NEW.state='Capturing' AND NOT checkpoint_stop_available(NEW.organization,NEW.request_id,NEW.principal))))
            OR (NEW.state IN ('Committed','Conflict') AND (NEW.capture IS NULL OR NEW.lease_owner IS NULL
                OR NEW.lease_until_ms<=floor(extract(epoch from clock_timestamp())*1000))) THEN
            RAISE EXCEPTION 'invalid artifact transition';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_runtime_start_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'runtime start history is immutable'; END IF;
    IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') OR (NEW.state<>OLD.state AND NOT (
        (OLD.state='Queued' AND NEW.state IN ('Preparing','Cancelled'))
        OR (OLD.state='Preparing' AND NEW.state='Prepared')
        OR (OLD.state='Prepared' AND NEW.state='Draining' AND EXISTS (SELECT 1 FROM artifact_commits
            WHERE organization=OLD.organization AND request_id=OLD.request_id AND state='Draining' AND cancel_running))
        OR (OLD.state='Prepared' AND NEW.state='Sealing' AND EXISTS (SELECT 1 FROM artifact_commits
            WHERE organization=OLD.organization AND request_id=OLD.request_id AND state='Capturing' AND NOT cancel_running))
        OR (OLD.state='Draining' AND NEW.state='Sealing' AND artifact_candidate_drained(OLD.organization,OLD.request_id)
            AND EXISTS (SELECT 1 FROM artifact_commits WHERE organization=OLD.organization AND request_id=OLD.request_id AND state='Capturing' AND cancel_running))
        OR (OLD.state='Sealing' AND NEW.state='Sealed' AND EXISTS (SELECT 1 FROM artifact_commits
            WHERE organization=OLD.organization AND request_id=OLD.request_id AND state IN ('Committed','Conflict')))
        OR (OLD.state IN ('Prepared','Sealed') AND NEW.state='Stopped' AND EXISTS (SELECT 1 FROM runtime_stops WHERE organization=OLD.organization AND request_id=OLD.request_id))
    )) THEN RAISE EXCEPTION 'invalid runtime start mutation'; END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION verify_checkpoint_stop_completion() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.stop_after_commit AND NEW.state IN ('Committed','Conflict') AND (
        NOT checkpoint_stop_available(NEW.organization,NEW.request_id,NEW.principal)
        OR NOT EXISTS (SELECT 1 FROM runtime_stops s JOIN runtime_start_requests r USING(organization,request_id)
            WHERE s.organization=NEW.organization AND s.request_id=NEW.request_id AND r.state='Stopped'
                AND s.receipt#>>'{checkpoint,artifact_id}'=NEW.commit_id
                AND s.receipt->>'proof'='artifact_checkpoint')
        OR NEW.lease_until_ms IS NULL
        OR NEW.lease_until_ms<=floor(extract(epoch from clock_timestamp())*1000)) THEN
        RAISE EXCEPTION 'checkpoint publication and stop must commit together';
    END IF;
    RETURN NULL;
END;
$$;

-- Pending drain must lower the runtime boundary in the same transaction.
CREATE FUNCTION verify_checkpoint_drain() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM artifact_commits a JOIN runtime_start_requests r USING(organization,request_id)
        WHERE a.organization=NEW.organization AND a.commit_id=NEW.commit_id
            AND ((a.state='Draining' AND r.state<>'Draining')
                OR (a.state='Capturing' AND a.cancel_running AND r.state<>'Sealing'))) THEN
        RAISE EXCEPTION 'checkpoint drain boundary is incomplete';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER checkpoint_drain_consistency AFTER INSERT OR UPDATE ON artifact_commits
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION verify_checkpoint_drain();
