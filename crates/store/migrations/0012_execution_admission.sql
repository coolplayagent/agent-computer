-- Queue reservation only. No process or dispatch proof is inferred from this table.
CREATE TABLE execution_requests (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    session_id TEXT NOT NULL,
    input JSONB NOT NULL CHECK (jsonb_typeof(input)='object'),
    binding JSONB NOT NULL CHECK (jsonb_typeof(binding)='object'),
    input_digest TEXT NOT NULL CHECK (input_digest ~ '^sha256:[0-9a-f]{64}$'),
    binding_digest TEXT NOT NULL CHECK (binding_digest ~ '^sha256:[0-9a-f]{64}$'),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms > 0),
    queue_deadline_at_ms BIGINT NOT NULL CHECK (queue_deadline_at_ms > created_at_ms),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    state TEXT NOT NULL DEFAULT 'Queued' CHECK (state IN ('Queued','Cancelled')),
    reason TEXT NOT NULL DEFAULT 'awaiting_runtime_dispatch' CHECK (reason IN ('awaiting_runtime_dispatch','user_requested','writer_unavailable')),
    PRIMARY KEY (organization,execution_id),
    UNIQUE (organization,lease_id,epoch),
    FOREIGN KEY (organization,lease_id,epoch) REFERENCES candidate_writer_epochs,
    FOREIGN KEY (organization,session_id) REFERENCES connection_sessions
);

CREATE FUNCTION guard_execution_request() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'execution history is retained'; END IF;
    IF TG_OP='UPDATE' THEN
        IF (to_jsonb(NEW)-ARRAY['state','revision','reason']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision','reason'])
            OR OLD.state<>'Queued' OR NEW.state<>'Cancelled' OR NEW.revision<>OLD.revision+1 OR NEW.reason='awaiting_runtime_dispatch'
            OR EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=OLD.organization AND lease_id=OLD.lease_id AND epoch=OLD.epoch) THEN
            RAISE EXCEPTION 'invalid execution cancellation';
        END IF;
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
CREATE TRIGGER check_execution_request BEFORE INSERT OR UPDATE OR DELETE ON execution_requests
    FOR EACH ROW EXECUTE FUNCTION guard_execution_request();

CREATE FUNCTION guard_execution_writer_slot() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    IF EXISTS(SELECT 1 FROM execution_requests WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch AND state='Queued') THEN
        RAISE EXCEPTION 'writer slot is reserved by a queued execution';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_writer_dispatch BEFORE INSERT ON candidate_writer_dispatches
    FOR EACH ROW EXECUTE FUNCTION guard_execution_writer_slot();
CREATE TRIGGER check_execution_writer_drain BEFORE INSERT ON candidate_writer_drains
    FOR EACH ROW EXECUTE FUNCTION guard_execution_writer_slot();
