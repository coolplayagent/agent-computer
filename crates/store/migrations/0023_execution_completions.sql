-- A completion binds an in-process kernel/IO seal to the exact dispatch and
-- verified output. Reading this immutable row never recreates the live seal.
CREATE TABLE execution_completions (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    dispatch_digest TEXT NOT NULL,
    arm_digest TEXT NOT NULL,
    seal JSONB NOT NULL CHECK (jsonb_typeof(seal)='object' AND octet_length(seal::text)<=65536),
    seal_digest TEXT NOT NULL CHECK (seal_digest ~ '^sha256:[0-9a-f]{64}$'),
    output_manifest_digest TEXT,
    accepted_state TEXT NOT NULL CHECK (accepted_state IN ('Succeeded','Failed','Cancelled','Unknown')),
    completed_at_ms BIGINT NOT NULL CHECK (completed_at_ms>0),
    PRIMARY KEY (organization,execution_id),
    UNIQUE (organization,lease_id,epoch),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_dispatch_intents,
    FOREIGN KEY (organization,lease_id,epoch) REFERENCES candidate_writer_epochs
);
CREATE TRIGGER immutable_execution_completion BEFORE UPDATE OR DELETE ON execution_completions
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE FUNCTION guard_execution_completion() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r execution_requests%ROWTYPE;
    d execution_dispatch_intents%ROWTYPE;
    w execution_watchdog_arms%ROWTYPE;
    l candidate_writer_leases%ROWTYPE;
    output execution_output_intents%ROWTYPE;
    binding JSONB;
BEGIN
    SELECT * INTO STRICT r FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT d FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT w FROM execution_watchdog_arms WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT l FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    SELECT (manifest#>>'{metadata,annotations,agent-computer.io/binding}')::jsonb INTO STRICT binding
        FROM execution_pod_plans WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    IF r.lease_id<>NEW.lease_id OR r.epoch<>NEW.epoch OR d.lease_id<>NEW.lease_id OR d.epoch<>NEW.epoch
        OR l.epoch<>NEW.epoch OR l.state NOT IN ('Held','Draining') OR r.state NOT IN ('Dispatching','CancelRequested','Unknown')
        OR NEW.dispatch_digest<>d.intent_digest OR NEW.arm_digest<>w.evidence_digest
        OR NEW.seal->>'version' IS DISTINCT FROM '1' OR NEW.seal->'arm' IS DISTINCT FROM w.evidence
        OR COALESCE(NEW.seal->>'domain','') NOT IN ('empty','removed')
        OR NEW.seal->'io'->>'version' IS DISTINCT FROM '1'
        OR COALESCE(NEW.seal->'io'->>'instance','') !~ '^[0-9a-f]{64}$'
        OR NEW.seal->'io'->>'instance' IS DISTINCT FROM w.evidence->'runtime'->'workspace_mount'->>'instance'
        OR NEW.seal->'io'->'prepared' IS DISTINCT FROM r.binding->'prepared'
        OR NEW.seal->'io'->'prepared' IS DISTINCT FROM w.evidence->'runtime'->'workspace_mount'->'prepared'
        OR binding#>'{workspace,fence,mount}' IS NULL
        OR binding#>'{workspace,fence,mount}' IS DISTINCT FROM w.evidence#>'{runtime,workspace_mount}'
        OR binding#>'{workspace,fence,node}' IS DISTINCT FROM w.evidence#>'{runtime,identity,node}'
        OR COALESCE((NEW.seal->'io'->>'accepted_mutating_requests')::numeric,-1)<0
        OR COALESCE((NEW.seal->>'observed_boottime_ms')::numeric,-1)<(w.evidence->>'observed_boottime_ms')::numeric THEN
        RAISE EXCEPTION 'execution seal binding does not match';
    END IF;
    IF NEW.output_manifest_digest IS NOT NULL THEN
        SELECT i.* INTO STRICT output FROM execution_output_intents i JOIN execution_outputs o USING(organization,execution_id)
            WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id
            AND i.manifest_digest=NEW.output_manifest_digest AND o.manifest_digest=i.manifest_digest;
        IF output.dispatch_digest<>NEW.dispatch_digest OR output.arm_digest<>NEW.arm_digest THEN
            RAISE EXCEPTION 'completion output binding does not match';
        END IF;
    END IF;
    IF NEW.accepted_state IN ('Succeeded','Failed') THEN
        IF r.state<>'Dispatching' OR l.state<>'Held' OR NEW.output_manifest_digest IS NULL
            OR NEW.completed_at_ms>=d.deadline_at_ms OR floor(extract(epoch from clock_timestamp())*1000)>=d.deadline_at_ms
            OR (NEW.accepted_state='Succeeded' AND output.manifest->'summary'->>'observed_outcome' IS DISTINCT FROM 'succeeded')
            OR (NEW.accepted_state='Failed' AND COALESCE(output.manifest->'summary'->>'observed_outcome','') NOT IN ('failed','spawn_failed','timed_out','descendants_terminated')) THEN
            RAISE EXCEPTION 'terminal outcome is unconfirmed';
        END IF;
    ELSIF NEW.accepted_state='Cancelled' AND r.state<>'CancelRequested' THEN
        RAISE EXCEPTION 'dispatched cancellation was not requested';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_completion BEFORE INSERT ON execution_completions
    FOR EACH ROW EXECUTE FUNCTION guard_execution_completion();

ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_state_check;
ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_reason_check;
ALTER TABLE execution_requests ADD CHECK (state IN ('Queued','Cancelled','Dispatching','CancelRequested','Unknown','Succeeded','Failed'));
ALTER TABLE execution_requests ADD CHECK (reason IN ('awaiting_runtime_dispatch','user_requested','writer_unavailable','dispatch_committed','dispatch_unconfirmed','completed','completed_cancelled','completion_unconfirmed'));

CREATE OR REPLACE FUNCTION guard_execution_request() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'execution history is retained'; END IF;
    IF TG_OP='UPDATE' THEN
        IF (to_jsonb(NEW)-ARRAY['state','revision','reason']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision','reason'])
            OR NEW.revision<>OLD.revision+1 THEN RAISE EXCEPTION 'invalid execution identity or revision'; END IF;
        IF NEW.reason IN ('completed','completed_cancelled','completion_unconfirmed') THEN
            IF OLD.state NOT IN ('Dispatching','CancelRequested')
                OR NOT EXISTS(SELECT 1 FROM execution_completions c WHERE c.organization=OLD.organization AND c.execution_id=OLD.execution_id
                    AND c.lease_id=OLD.lease_id AND c.epoch=OLD.epoch AND c.accepted_state=NEW.state)
                OR (NEW.reason='completed' AND NEW.state NOT IN ('Succeeded','Failed'))
                OR (NEW.reason='completed_cancelled' AND NEW.state<>'Cancelled')
                OR (NEW.reason='completion_unconfirmed' AND NEW.state<>'Unknown') THEN
                RAISE EXCEPTION 'execution completion is unconfirmed';
            END IF;
        ELSIF OLD.state='Queued' AND NEW.state='Cancelled' THEN
            IF NEW.reason NOT IN ('user_requested','writer_unavailable')
                OR EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=OLD.organization AND lease_id=OLD.lease_id AND epoch=OLD.epoch)
                OR EXISTS(SELECT 1 FROM execution_dispatch_intents WHERE organization=OLD.organization AND execution_id=OLD.execution_id) THEN
                RAISE EXCEPTION 'dispatched execution cannot be cancelled as queued';
            END IF;
        ELSIF OLD.state='Queued' AND NEW.state='Dispatching' THEN
            IF NEW.reason<>'dispatch_committed' OR NOT EXISTS(SELECT 1 FROM execution_dispatch_intents i JOIN candidate_writer_dispatches d ON d.organization=i.organization AND d.dispatch_id=i.execution_id
                WHERE i.organization=OLD.organization AND i.execution_id=OLD.execution_id AND i.lease_id=OLD.lease_id AND i.epoch=OLD.epoch
                AND d.lease_id=i.lease_id AND d.epoch=i.epoch AND d.input_digest=i.intent_digest) THEN
                RAISE EXCEPTION 'execution dispatch journal is missing';
            END IF;
        ELSIF OLD.state='Dispatching' AND NEW.state='CancelRequested' THEN
            IF NEW.reason<>'user_requested' THEN RAISE EXCEPTION 'invalid cancellation request'; END IF;
        ELSIF OLD.state IN ('Dispatching','CancelRequested') AND NEW.state='Unknown' THEN
            IF NEW.reason NOT IN ('writer_unavailable','dispatch_unconfirmed') THEN RAISE EXCEPTION 'invalid uncertain outcome'; END IF;
        ELSE RAISE EXCEPTION 'invalid execution transition'; END IF;
    ELSE
        SELECT * INTO STRICT head FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
        IF head.state<>'Held' OR head.epoch<>NEW.epoch OR head.session_id<>NEW.session_id
            OR NEW.queue_deadline_at_ms>head.expires_at_ms OR NEW.queue_deadline_at_ms<=floor(extract(epoch from clock_timestamp())*1000)
            OR NEW.state<>'Queued' OR NEW.revision<>1 OR NEW.reason<>'awaiting_runtime_dispatch'
            OR EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch) THEN
            RAISE EXCEPTION 'execution reservation is unavailable';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

ALTER TABLE candidate_writer_drains DROP CONSTRAINT candidate_writer_drains_proof_check;
ALTER TABLE candidate_writer_drains ADD CHECK (proof IN ('no_dispatch','bounded_file_drained','execution_drained'));
CREATE OR REPLACE FUNCTION guard_writer_record_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    SELECT * INTO STRICT head FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    IF NEW.epoch<>head.epoch THEN RAISE EXCEPTION 'stale writer epoch'; END IF;
    IF TG_TABLE_NAME='candidate_writer_dispatches' THEN
        IF head.state<>'Held' OR head.expires_at_ms<=floor(extract(epoch from clock_timestamp())*1000) THEN
            RAISE EXCEPTION 'writer dispatch is not admitted';
        END IF;
    ELSIF head.state<>'Draining' THEN RAISE EXCEPTION 'writer is not draining';
    ELSIF NEW.proof='no_dispatch' THEN
        IF EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch) THEN
            RAISE EXCEPTION 'no-dispatch proof is unavailable';
        END IF;
    ELSIF NEW.proof='execution_drained' THEN
        IF NOT EXISTS(SELECT 1 FROM execution_completions c JOIN candidate_writer_dispatches d ON d.organization=c.organization AND d.dispatch_id=c.execution_id
            WHERE c.organization=NEW.organization AND c.lease_id=NEW.lease_id AND c.epoch=NEW.epoch
            AND d.lease_id=c.lease_id AND d.epoch=c.epoch AND d.input_digest=c.dispatch_digest) THEN
            RAISE EXCEPTION 'execution drain is unconfirmed';
        END IF;
    ELSIF NOT EXISTS(SELECT 1 FROM candidate_writer_completions WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch AND observed->>'drain_confirmed'='true') THEN
        RAISE EXCEPTION 'bounded file drain is unconfirmed';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION complete_execution_completion() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN candidate_writer_leases l USING(organization,lease_id,epoch)
        JOIN candidate_writer_drains d USING(organization,lease_id,epoch)
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state=NEW.accepted_state
        AND l.state='Released' AND d.proof='execution_drained') THEN
        RAISE EXCEPTION 'execution completion transaction is incomplete';
    END IF;
    IF NEW.accepted_state IN ('Succeeded','Failed') AND EXISTS(SELECT 1 FROM execution_dispatch_intents i
        WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id
        AND floor(extract(epoch from clock_timestamp())*1000)>=i.deadline_at_ms) THEN
        RAISE EXCEPTION 'execution completion deadline elapsed';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER complete_execution_completion AFTER INSERT ON execution_completions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION complete_execution_completion();

CREATE OR REPLACE FUNCTION guard_execution_writer_slot() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE request execution_requests%ROWTYPE;
BEGIN
    PERFORM 1 FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    SELECT * INTO request FROM execution_requests WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch;
    IF FOUND AND (request.state<>'Cancelled' OR EXISTS(SELECT 1 FROM execution_dispatch_intents WHERE organization=request.organization AND execution_id=request.execution_id)) THEN
        IF TG_TABLE_NAME='candidate_writer_drains' THEN
            IF NEW.proof='execution_drained' AND EXISTS(SELECT 1 FROM execution_completions c
                WHERE c.organization=NEW.organization AND c.lease_id=NEW.lease_id AND c.epoch=NEW.epoch
                AND c.execution_id=request.execution_id AND c.accepted_state=request.state) THEN RETURN NEW; END IF;
            RAISE EXCEPTION 'execution drain requires physical evidence';
        END IF;
        IF TG_TABLE_NAME<>'candidate_writer_dispatches' THEN RAISE EXCEPTION 'execution drain requires physical evidence'; END IF;
        IF request.state<>'Queued' OR NEW.dispatch_id<>request.execution_id
            OR NOT EXISTS(SELECT 1 FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.dispatch_id AND intent_digest=NEW.input_digest) THEN
            RAISE EXCEPTION 'writer slot is reserved by an execution';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- A sealed execution may release its physical writer while its outcome remains
-- Unknown. Artifact/checkpoint publication still requires a resolved outcome.
CREATE OR REPLACE FUNCTION artifact_candidate_drained(org TEXT, request TEXT) RETURNS boolean LANGUAGE sql AS $$
    SELECT NOT EXISTS (SELECT 1 FROM candidate_writer_leases l WHERE l.organization=$1 AND l.request_id=$2 AND (
        l.state<>'Released'
        OR EXISTS (SELECT 1 FROM execution_requests e WHERE e.organization=l.organization AND e.lease_id=l.lease_id
            AND e.state NOT IN ('Cancelled','Succeeded','Failed'))
        OR EXISTS (SELECT 1 FROM candidate_writer_dispatches d
            LEFT JOIN candidate_writer_completions c USING(organization,lease_id,epoch,dispatch_id)
            LEFT JOIN candidate_writer_drains p USING(organization,lease_id,epoch)
            LEFT JOIN execution_completions e ON e.organization=d.organization AND e.lease_id=d.lease_id AND e.epoch=d.epoch AND e.execution_id=d.dispatch_id
            WHERE d.organization=l.organization AND d.lease_id=l.lease_id AND NOT (
                (c.dispatch_id IS NOT NULL AND c.observed->>'drain_confirmed' IS NOT DISTINCT FROM 'true' AND p.proof IS NOT DISTINCT FROM 'bounded_file_drained')
                OR (e.execution_id IS NOT NULL AND e.accepted_state IN ('Succeeded','Failed','Cancelled') AND e.dispatch_digest=d.input_digest AND p.proof IS NOT DISTINCT FROM 'execution_drained')
            ))
    ));
$$;
