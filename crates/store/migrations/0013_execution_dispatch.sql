-- A committed intent consumes the writer slot. Neither intent nor cancellation
-- is evidence that a process started, stopped, or drained shared storage.
ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_state_check;
ALTER TABLE execution_requests DROP CONSTRAINT execution_requests_reason_check;
ALTER TABLE execution_requests ADD CHECK (state IN ('Queued','Cancelled','Dispatching','CancelRequested','Unknown'));
ALTER TABLE execution_requests ADD CHECK (reason IN ('awaiting_runtime_dispatch','user_requested','writer_unavailable','dispatch_committed','dispatch_unconfirmed'));

CREATE TABLE execution_dispatch_intents (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    intent_digest TEXT NOT NULL CHECK (intent_digest ~ '^sha256:[0-9a-f]{64}$'),
    started_at_ms BIGINT NOT NULL CHECK (started_at_ms > 0),
    deadline_at_ms BIGINT NOT NULL CHECK (deadline_at_ms > started_at_ms AND deadline_at_ms <= started_at_ms+30000),
    PRIMARY KEY (organization,execution_id),
    UNIQUE (organization,lease_id,epoch),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_requests,
    FOREIGN KEY (organization,execution_id) REFERENCES candidate_writer_dispatches(organization,dispatch_id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (organization,lease_id,epoch) REFERENCES candidate_writer_dispatches(organization,lease_id,epoch) DEFERRABLE INITIALLY DEFERRED
);
CREATE TRIGGER immutable_execution_dispatch BEFORE UPDATE OR DELETE ON execution_dispatch_intents
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE FUNCTION guard_execution_dispatch() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    SELECT * INTO STRICT head FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    IF head.state<>'Held' OR head.epoch<>NEW.epoch OR NEW.deadline_at_ms>head.expires_at_ms
        OR NEW.deadline_at_ms<=floor(extract(epoch from clock_timestamp())*1000)
        OR NOT EXISTS(SELECT 1 FROM execution_requests r WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id
            AND r.lease_id=NEW.lease_id AND r.epoch=NEW.epoch AND r.session_id=head.session_id
            AND r.state='Queued' AND r.queue_deadline_at_ms>=NEW.deadline_at_ms) THEN
        RAISE EXCEPTION 'execution dispatch is not admitted';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_dispatch BEFORE INSERT ON execution_dispatch_intents
    FOR EACH ROW EXECUTE FUNCTION guard_execution_dispatch();

CREATE FUNCTION complete_execution_dispatch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN candidate_writer_dispatches d ON d.organization=r.organization AND d.dispatch_id=r.execution_id
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state IN ('Dispatching','CancelRequested','Unknown')
        AND r.lease_id=NEW.lease_id AND r.epoch=NEW.epoch AND d.lease_id=NEW.lease_id AND d.epoch=NEW.epoch AND d.input_digest=NEW.intent_digest) THEN
        RAISE EXCEPTION 'execution dispatch transaction is incomplete';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER complete_execution_dispatch AFTER INSERT ON execution_dispatch_intents
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION complete_execution_dispatch();

CREATE OR REPLACE FUNCTION guard_execution_request() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'execution history is retained'; END IF;
    IF TG_OP='UPDATE' THEN
        IF (to_jsonb(NEW)-ARRAY['state','revision','reason']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision','reason'])
            OR NEW.revision<>OLD.revision+1 THEN RAISE EXCEPTION 'invalid execution identity or revision'; END IF;
        IF OLD.state='Queued' AND NEW.state='Cancelled' THEN
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

CREATE OR REPLACE FUNCTION guard_execution_writer_slot() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE request execution_requests%ROWTYPE;
BEGIN
    PERFORM 1 FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    SELECT * INTO request FROM execution_requests WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch;
    IF FOUND AND request.state<>'Cancelled' THEN
        IF TG_TABLE_NAME<>'candidate_writer_dispatches' THEN RAISE EXCEPTION 'execution drain requires physical evidence'; END IF;
        IF request.state<>'Queued' OR NEW.dispatch_id<>request.execution_id
            OR NOT EXISTS(SELECT 1 FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.dispatch_id AND intent_digest=NEW.input_digest) THEN
            RAISE EXCEPTION 'writer slot is reserved by an execution';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
-- File completion cannot be used to manufacture execution drain evidence.
CREATE TRIGGER check_execution_file_completion BEFORE INSERT ON candidate_writer_completions
    FOR EACH ROW EXECUTE FUNCTION guard_execution_writer_slot();
