-- A Workspace owns many isolated Candidates; a Computer still owns one active
-- generation. Capacity is reserved for every Candidate, including retained ones.
DROP INDEX runtime_one_active_workspace;
CREATE INDEX runtime_workspace_candidates ON runtime_start_requests (organization,workspace_id) WHERE state NOT IN ('Cancelled','Stopped');

-- Admission receipts bind the exact immutable input independently of the head.
CREATE FUNCTION guard_runtime_start_input() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE started runtime_start_requests%ROWTYPE; version workspace_input_versions%ROWTYPE;
BEGIN
    SELECT * INTO STRICT started FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id;
    SELECT * INTO STRICT version FROM workspace_input_versions WHERE organization=NEW.organization AND workspace_id=NEW.workspace_id AND revision=NEW.revision;
    IF started.workspace_id<>NEW.workspace_id
        OR (started.receipt->>'input_revision')::bigint IS DISTINCT FROM NEW.revision
        OR started.receipt->>'input_manifest_digest' IS DISTINCT FROM version.digest
        OR started.receipt->>'input_artifact_id' IS DISTINCT FROM version.artifact_commit_id
        OR (version.artifact_commit_id IS NOT NULL AND NOT EXISTS (
            SELECT 1 FROM artifact_commits a WHERE a.organization=NEW.organization AND a.workspace_id=NEW.workspace_id
                AND a.commit_id=version.artifact_commit_id AND a.input_revision=NEW.revision
                AND a.state IN ('Committed','Conflict') AND a.object_ref IS NOT NULL)) THEN
        RAISE EXCEPTION 'start input does not match its immutable admission';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_runtime_start_input BEFORE INSERT ON runtime_start_inputs FOR EACH ROW EXECUTE FUNCTION guard_runtime_start_input();

-- A checkpoint belongs to its Computer and fixed input, even when publication
-- retained a branch or lost the Workspace-head CAS. It does not move that head.
CREATE OR REPLACE FUNCTION runtime_stop_checkpoint(org TEXT, request TEXT) RETURNS jsonb LANGUAGE sql AS $$
    SELECT jsonb_build_object('artifact_id',a.commit_id,'input_revision',a.input_revision,
        'manifest_digest',v.digest,'snapshot_digest',r.snapshot_digest,'computer_spec_digest',resource#>>'{reference,digest}',
        'app_states','[]'::jsonb,'unfinished_execution_ids','[]'::jsonb)
    FROM artifact_commits a JOIN runtime_start_requests r USING(organization,request_id)
    JOIN workspace_input_versions v ON v.organization=a.organization AND v.workspace_id=a.workspace_id AND v.revision=a.input_revision
    CROSS JOIN LATERAL jsonb_array_elements(r.snapshot->'resources') resource
    WHERE a.organization=$1 AND a.request_id=$2 AND a.state IN ('Committed','Conflict')
        AND r.state='Sealed' AND resource#>>'{reference,kind}'='computer' AND resource#>>'{reference,resource_id}'=r.computer_id
        AND jsonb_array_length(resource#>'{spec,appRefs}')=0 AND artifact_candidate_drained($1,$2)
        AND NOT EXISTS (SELECT 1 FROM connection_sessions s JOIN principals p USING(organization,principal)
            WHERE s.organization=$1 AND s.computer_id=r.computer_id AND p.kind='human' AND s.state='Active'
                AND s.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000) AND s.activity='active');
$$;
