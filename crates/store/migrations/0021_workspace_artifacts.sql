CREATE TABLE artifact_commits (
    organization TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    credential_id TEXT NOT NULL REFERENCES service_credentials,
    input JSONB NOT NULL,
    state TEXT NOT NULL DEFAULT 'Capturing' CHECK (state IN ('Capturing','Committed','Conflict')),
    capture JSONB,
    object_ref JSONB,
    input_revision BIGINT,
    published_at_ms BIGINT,
    lease_epoch BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch>=0),
    lease_owner TEXT,
    lease_until_ms BIGINT,
    PRIMARY KEY (organization,commit_id),
    UNIQUE (organization,request_id),
    FOREIGN KEY (organization,request_id) REFERENCES candidate_preparations,
    FOREIGN KEY (organization,workspace_id) REFERENCES resource_definitions,
    FOREIGN KEY (organization,principal) REFERENCES principals,
    CHECK ((lease_owner IS NULL)=(lease_until_ms IS NULL)),
    CHECK ((state='Capturing')=(published_at_ms IS NULL)),
    CHECK ((state='Capturing')=(object_ref IS NULL)),
    CHECK ((state='Capturing')=(input_revision IS NULL))
);
CREATE FUNCTION artifact_candidate_drained(org TEXT, request TEXT) RETURNS boolean LANGUAGE sql AS $$
    SELECT NOT EXISTS (SELECT 1 FROM candidate_writer_leases l WHERE l.organization=$1 AND l.request_id=$2 AND (
        l.state<>'Released'
        OR EXISTS (SELECT 1 FROM execution_requests e WHERE e.organization=l.organization AND e.lease_id=l.lease_id AND e.state<>'Cancelled')
        OR EXISTS (SELECT 1 FROM candidate_writer_dispatches d
            LEFT JOIN candidate_writer_completions c USING(organization,lease_id,epoch,dispatch_id)
            LEFT JOIN candidate_writer_drains p USING(organization,lease_id,epoch)
            WHERE d.organization=l.organization AND d.lease_id=l.lease_id
                AND (c.dispatch_id IS NULL OR c.observed->>'drain_confirmed' IS DISTINCT FROM 'true' OR p.proof IS DISTINCT FROM 'bounded_file_drained'))
    ));
$$;
CREATE FUNCTION guard_artifact_commit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'artifact history is retained'; END IF;
    IF TG_OP='INSERT' THEN
        SELECT * INTO STRICT head FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id FOR UPDATE;
        IF head.state<>'Prepared' OR head.workspace_id<>NEW.workspace_id OR NOT artifact_candidate_drained(NEW.organization,NEW.request_id)
            OR NEW.state<>'Capturing' OR NEW.capture IS NOT NULL THEN RAISE EXCEPTION 'candidate is not ready to seal'; END IF;
    ELSE
        IF (to_jsonb(NEW)-ARRAY['credential_id','state','capture','object_ref','input_revision','published_at_ms','lease_epoch','lease_owner','lease_until_ms'])
            IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['credential_id','state','capture','object_ref','input_revision','published_at_ms','lease_epoch','lease_owner','lease_until_ms'])
            OR OLD.state<>'Capturing' OR NEW.lease_epoch<OLD.lease_epoch
            OR (OLD.capture IS NOT NULL AND NEW.capture IS DISTINCT FROM OLD.capture)
            OR (NEW.state<>'Capturing' AND (NEW.capture IS NULL OR NEW.lease_owner IS NULL OR NEW.lease_until_ms<=floor(extract(epoch from clock_timestamp())*1000))) THEN
            RAISE EXCEPTION 'invalid artifact transition';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_artifact_commit BEFORE INSERT OR UPDATE OR DELETE ON artifact_commits FOR EACH ROW EXECUTE FUNCTION guard_artifact_commit();
ALTER TABLE workspace_input_versions DROP CONSTRAINT workspace_input_versions_origin_check;
ALTER TABLE workspace_input_versions ADD CHECK (origin IN ('initial_empty','artifact'));
ALTER TABLE workspace_input_versions ADD COLUMN artifact_commit_id TEXT;
ALTER TABLE workspace_input_versions ADD FOREIGN KEY (organization,artifact_commit_id) REFERENCES artifact_commits;
ALTER TABLE workspace_input_versions ADD CHECK ((origin='artifact')=(artifact_commit_id IS NOT NULL));
ALTER TABLE runtime_start_requests DROP CONSTRAINT runtime_start_requests_state_check;
ALTER TABLE runtime_start_requests ADD CHECK (state IN ('Queued','Preparing','Prepared','Sealing','Sealed','Cancelled','Stopped'));
CREATE OR REPLACE FUNCTION guard_runtime_start_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'runtime start history is immutable'; END IF;
    IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') OR (NEW.state<>OLD.state AND NOT (
        (OLD.state='Queued' AND NEW.state IN ('Preparing','Cancelled'))
        OR (OLD.state='Preparing' AND NEW.state='Prepared')
        OR (OLD.state='Prepared' AND NEW.state='Sealing' AND EXISTS (SELECT 1 FROM artifact_commits WHERE organization=OLD.organization AND request_id=OLD.request_id))
        OR (OLD.state='Sealing' AND NEW.state='Sealed' AND EXISTS (SELECT 1 FROM artifact_commits WHERE organization=OLD.organization AND request_id=OLD.request_id AND state<>'Capturing'))
        OR (OLD.state IN ('Prepared','Sealed') AND NEW.state='Stopped' AND EXISTS (SELECT 1 FROM runtime_stops WHERE organization=OLD.organization AND request_id=OLD.request_id))
    )) THEN RAISE EXCEPTION 'invalid runtime start mutation'; END IF;
    RETURN NEW;
END;
$$;

-- Only the file-only checkpoint shape is implemented. App/profile state must
-- never be silently omitted from a Computer that declares Apps.
CREATE FUNCTION runtime_stop_checkpoint(org TEXT, request TEXT) RETURNS jsonb LANGUAGE sql AS $$
    SELECT jsonb_build_object('artifact_id',a.commit_id,'input_revision',a.input_revision,
        'manifest_digest',v.digest,'snapshot_digest',r.snapshot_digest,'computer_spec_digest',resource#>>'{reference,digest}',
        'app_states','[]'::jsonb,'unfinished_execution_ids','[]'::jsonb)
    FROM artifact_commits a JOIN runtime_start_requests r USING(organization,request_id)
    JOIN workspace_input_versions v ON v.organization=a.organization AND v.workspace_id=a.workspace_id AND v.revision=a.input_revision
    JOIN workspace_input_heads h ON h.organization=v.organization AND h.workspace_id=v.workspace_id AND h.revision=v.revision
    CROSS JOIN LATERAL jsonb_array_elements(r.snapshot->'resources') resource
    WHERE a.organization=$1 AND a.request_id=$2 AND a.state='Committed' AND a.input->>'publish_current'='true'
        AND r.state='Sealed' AND resource#>>'{reference,kind}'='computer' AND resource#>>'{reference,resource_id}'=r.computer_id
        AND jsonb_array_length(resource#>'{spec,appRefs}')=0 AND artifact_candidate_drained($1,$2)
        AND NOT EXISTS (SELECT 1 FROM connection_sessions s JOIN principals p USING(organization,principal)
            WHERE s.organization=$1 AND s.computer_id=r.computer_id AND p.kind='human' AND s.state='Active'
                AND s.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000) AND s.activity='active');
$$;
CREATE OR REPLACE FUNCTION guard_runtime_stop_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE; control runtime_controls%ROWTYPE; input_revision BIGINT; input_digest TEXT; checkpoint JSONB; proof TEXT;
BEGIN
    SELECT * INTO STRICT head FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id FOR UPDATE;
    SELECT * INTO STRICT control FROM runtime_controls WHERE organization=NEW.organization AND computer_id=head.computer_id FOR UPDATE;
    IF control.active_request IS DISTINCT FROM head.request_id OR control.generation<>head.generation THEN RAISE EXCEPTION 'stale runtime'; END IF;
    checkpoint:=runtime_stop_checkpoint(NEW.organization,NEW.request_id);
    IF checkpoint IS NOT NULL THEN
        input_revision:=(checkpoint->>'input_revision')::bigint;input_digest:=checkpoint->>'manifest_digest';proof:='artifact_checkpoint';
        IF NEW.receipt->'checkpoint' IS DISTINCT FROM checkpoint THEN RAISE EXCEPTION 'checkpoint mismatch'; END IF;
    ELSE
        IF NOT runtime_stop_is_undispatched(NEW.organization,NEW.request_id) OR NEW.receipt ? 'checkpoint' THEN RAISE EXCEPTION 'runtime stop is blocked'; END IF;
        SELECT i.revision,v.digest INTO STRICT input_revision,input_digest FROM runtime_start_inputs i
            JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision
            WHERE i.organization=NEW.organization AND i.request_id=NEW.request_id;
        proof:='no_user_dispatch';
    END IF;
    IF NOT (NEW.receipt @> jsonb_build_object('computer_id',head.computer_id,'request_id',head.request_id,'generation',head.generation,
        'candidate_id',head.candidate_id,'control_revision',control.revision+1,'input_revision',input_revision,'input_manifest_digest',input_digest,
        'retained_storage_bytes',head.storage_bytes,'proof',proof)) OR COALESCE((NEW.receipt->>'stopped_at_ms')::bigint,0)<=0
        OR COALESCE((NEW.receipt->>'event_sequence')::bigint,0)<=0 THEN RAISE EXCEPTION 'invalid runtime stop receipt'; END IF;
    RETURN NEW;
END;
$$;
