-- Stopping this path proves absence of user dispatch, never flush/fencing of IO.
CREATE TABLE runtime_stops (
    organization TEXT NOT NULL,
    request_id TEXT NOT NULL,
    receipt JSONB NOT NULL CHECK (jsonb_typeof(receipt)='object'),
    PRIMARY KEY (organization,request_id),
    FOREIGN KEY (organization,request_id) REFERENCES runtime_start_requests
);
CREATE TRIGGER immutable_runtime_stop BEFORE UPDATE OR DELETE ON runtime_stops
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE OR REPLACE FUNCTION runtime_stop_is_undispatched(org TEXT, request TEXT) RETURNS boolean LANGUAGE sql AS $$
    SELECT EXISTS (
        SELECT 1 FROM runtime_start_requests r
        JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id
        JOIN candidate_preparations p ON p.organization=r.organization AND p.request_id=r.request_id
        WHERE r.organization=$1 AND r.request_id=$2 AND r.state='Prepared'
        AND c.active_request=r.request_id AND c.generation=r.generation AND p.receipt IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM candidate_writer_leases l
            WHERE l.organization=$1 AND l.request_id=$2 AND (
                l.state<>'Released'
                OR EXISTS (SELECT 1 FROM candidate_writer_dispatches d WHERE d.organization=l.organization AND d.lease_id=l.lease_id)
                OR EXISTS (SELECT 1 FROM execution_requests e WHERE e.organization=l.organization AND e.lease_id=l.lease_id AND e.state<>'Cancelled')))
        AND NOT EXISTS (SELECT 1 FROM connection_sessions s JOIN principals a USING(organization,principal)
            WHERE s.organization=$1 AND s.computer_id=r.computer_id AND a.kind='human'
                AND s.state='Active' AND s.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
                AND s.activity='active')
    );
$$;

CREATE OR REPLACE FUNCTION guard_runtime_stop_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE; control runtime_controls%ROWTYPE; input_revision BIGINT; input_digest TEXT;
BEGIN
    SELECT * INTO STRICT head FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id FOR UPDATE;
    SELECT * INTO STRICT control FROM runtime_controls WHERE organization=NEW.organization AND computer_id=head.computer_id FOR UPDATE;
    IF NOT runtime_stop_is_undispatched(NEW.organization,NEW.request_id) THEN RAISE EXCEPTION 'runtime stop is blocked'; END IF;
    SELECT i.revision,v.digest INTO STRICT input_revision,input_digest FROM runtime_start_inputs i
        JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision
        WHERE i.organization=NEW.organization AND i.request_id=NEW.request_id;
    IF NOT (NEW.receipt @> jsonb_build_object(
        'computer_id',head.computer_id,'request_id',head.request_id,'generation',head.generation,
        'candidate_id',head.candidate_id,'control_revision',control.revision+1,
        'input_revision',input_revision,'input_manifest_digest',input_digest,
        'retained_storage_bytes',head.storage_bytes,'proof','no_user_dispatch'))
        OR COALESCE((NEW.receipt->>'stopped_at_ms')::bigint,0)<=0
        OR COALESCE((NEW.receipt->>'event_sequence')::bigint,0)<=0 THEN
        RAISE EXCEPTION 'invalid runtime stop receipt';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_runtime_stop BEFORE INSERT ON runtime_stops FOR EACH ROW EXECUTE FUNCTION guard_runtime_stop_insert();

ALTER TABLE runtime_start_requests DROP CONSTRAINT runtime_start_requests_state_check;
ALTER TABLE runtime_start_requests ADD CONSTRAINT runtime_start_requests_state_check
    CHECK (state IN ('Queued','Preparing','Prepared','Cancelled','Stopped'));
CREATE OR REPLACE FUNCTION guard_runtime_start_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'runtime start history is immutable'; END IF;
    IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state')
        OR (NEW.state<>OLD.state AND NOT (
            (OLD.state='Queued' AND NEW.state IN ('Preparing','Cancelled'))
            OR (OLD.state='Preparing' AND NEW.state='Prepared')
            OR (OLD.state='Prepared' AND NEW.state='Stopped'
                AND EXISTS (SELECT 1 FROM runtime_stops WHERE organization=OLD.organization AND request_id=OLD.request_id))
        )) THEN RAISE EXCEPTION 'invalid runtime start mutation'; END IF;
    RETURN NEW;
END;
$$;
DROP INDEX runtime_one_active_computer;
DROP INDEX runtime_one_active_workspace;
CREATE UNIQUE INDEX runtime_one_active_computer ON runtime_start_requests (organization,computer_id) WHERE state NOT IN ('Cancelled','Stopped');
CREATE UNIQUE INDEX runtime_one_active_workspace ON runtime_start_requests (organization,workspace_id) WHERE state NOT IN ('Cancelled','Stopped');
-- runtime_reserved_capacity deliberately still includes Stopped storage.

CREATE OR REPLACE FUNCTION guard_writer_start_authority() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE;
BEGIN
    IF NEW.state='Held' THEN
        SELECT * INTO STRICT head FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id FOR UPDATE;
        IF head.state<>'Prepared'
            OR EXISTS (SELECT 1 FROM runtime_stops WHERE organization=NEW.organization AND request_id=NEW.request_id)
            OR NOT EXISTS (SELECT 1 FROM runtime_controls WHERE organization=NEW.organization AND computer_id=head.computer_id AND active_request=head.request_id AND generation=head.generation) THEN
            RAISE EXCEPTION 'writer start is no longer active';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_writer_start BEFORE INSERT OR UPDATE ON candidate_writer_leases FOR EACH ROW EXECUTE FUNCTION guard_writer_start_authority();
