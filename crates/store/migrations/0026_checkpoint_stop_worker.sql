ALTER TABLE artifact_commits ADD COLUMN stop_after_commit BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX artifact_worker_queue ON artifact_commits (organization,commit_id) WHERE state='Capturing';

-- Normal stop never omits declared App state or another principal's live use.
-- The caller's own idle connection is permitted; active human input is not.
CREATE FUNCTION checkpoint_stop_active_use(org TEXT, request TEXT, actor TEXT) RETURNS boolean LANGUAGE sql AS $$
    SELECT EXISTS (SELECT 1 FROM runtime_start_requests r
        JOIN connection_sessions s ON s.organization=r.organization AND s.computer_id=r.computer_id
        JOIN principals p ON p.organization=s.organization AND p.principal=s.principal
        WHERE r.organization=$1 AND r.request_id=$2 AND s.state='Active'
            AND s.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
            AND (s.principal<>$3 OR (p.kind='human' AND s.activity='active')));
$$;
CREATE FUNCTION checkpoint_stop_available(org TEXT, request TEXT, actor TEXT) RETURNS boolean LANGUAGE sql AS $$
    SELECT EXISTS (SELECT 1 FROM runtime_start_requests r
        CROSS JOIN LATERAL jsonb_array_elements(r.snapshot->'resources') resource
        WHERE r.organization=$1 AND r.request_id=$2
            AND resource#>>'{reference,kind}'='computer' AND resource#>>'{reference,resource_id}'=r.computer_id
            AND jsonb_array_length(resource#>'{spec,appRefs}')=0
            AND artifact_candidate_drained($1,$2)
            AND NOT checkpoint_stop_active_use($1,$2,$3));
$$;
CREATE FUNCTION guard_checkpoint_stop_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.stop_after_commit AND NOT checkpoint_stop_available(NEW.organization,NEW.request_id,NEW.principal) THEN
        RAISE EXCEPTION 'checkpoint stop is blocked';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_checkpoint_stop_admission BEFORE INSERT ON artifact_commits
    FOR EACH ROW EXECUTE FUNCTION guard_checkpoint_stop_admission();

-- Publication and the stop receipt are one transaction. A successful upload or
-- an Artifact row alone cannot silently complete a requested stop.
CREATE FUNCTION verify_checkpoint_stop_completion() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.stop_after_commit AND NEW.state<>'Capturing' AND (
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
CREATE CONSTRAINT TRIGGER checkpoint_stop_completion AFTER INSERT OR UPDATE ON artifact_commits
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION verify_checkpoint_stop_completion();
