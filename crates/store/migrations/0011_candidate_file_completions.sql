CREATE TABLE candidate_writer_completions (
    organization TEXT NOT NULL,
    dispatch_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    prepared_digest TEXT NOT NULL,
    input_digest TEXT NOT NULL,
    observed JSONB NOT NULL,
    accepted JSONB NOT NULL,
    PRIMARY KEY (organization,dispatch_id),
    UNIQUE (organization,lease_id,epoch),
    FOREIGN KEY (organization,dispatch_id) REFERENCES candidate_writer_dispatches,
    FOREIGN KEY (organization,lease_id,epoch) REFERENCES candidate_writer_epochs
);
CREATE TRIGGER immutable_writer_completion BEFORE UPDATE OR DELETE ON candidate_writer_completions
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_writer_completion() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    IF NOT EXISTS(SELECT 1 FROM candidate_writer_leases l JOIN candidate_writer_dispatches d USING(organization,lease_id,epoch)
        WHERE l.organization=NEW.organization AND l.lease_id=NEW.lease_id AND l.epoch=NEW.epoch AND l.state IN ('Held','Draining')
        AND d.dispatch_id=NEW.dispatch_id AND d.input_digest=NEW.input_digest AND l.prepared_digest=NEW.prepared_digest) THEN
        RAISE EXCEPTION 'writer completion binding does not match';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_writer_completion BEFORE INSERT ON candidate_writer_completions
    FOR EACH ROW EXECUTE FUNCTION guard_writer_completion();
ALTER TABLE candidate_writer_drains DROP CONSTRAINT candidate_writer_drains_proof_check;
ALTER TABLE candidate_writer_drains ADD CHECK (proof IN ('no_dispatch','bounded_file_drained'));

CREATE OR REPLACE FUNCTION guard_writer_record_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    SELECT * INTO STRICT head FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    IF NEW.epoch <> head.epoch THEN RAISE EXCEPTION 'stale writer epoch'; END IF;
    IF TG_TABLE_NAME='candidate_writer_dispatches' THEN
        IF head.state <> 'Held' OR head.expires_at_ms <= floor(extract(epoch from clock_timestamp())*1000) THEN
            RAISE EXCEPTION 'writer dispatch is not admitted';
        END IF;
    ELSIF head.state <> 'Draining' THEN
        RAISE EXCEPTION 'writer is not draining';
    ELSIF NEW.proof='no_dispatch' THEN
        IF EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch) THEN RAISE EXCEPTION 'no-dispatch proof is unavailable'; END IF;
    ELSIF NOT EXISTS(SELECT 1 FROM candidate_writer_completions WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch AND observed->>'drain_confirmed'='true') THEN
        RAISE EXCEPTION 'bounded file drain is unconfirmed';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_writer_lease_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'writer ownership history is retained'; END IF;
    IF NEW.organization<>OLD.organization OR NEW.lease_id<>OLD.lease_id OR NEW.request_id<>OLD.request_id OR NEW.prepared_digest<>OLD.prepared_digest OR NEW.revision<>OLD.revision+1 THEN
        RAISE EXCEPTION 'invalid writer identity or revision';
    END IF;
    IF OLD.state='Released' AND NEW.state='Held' THEN
        IF NEW.epoch<>OLD.epoch+1 OR NEW.expires_at_ms<=floor(extract(epoch from clock_timestamp())*1000) THEN RAISE EXCEPTION 'invalid writer acquisition'; END IF;
    ELSE
        IF NEW.epoch<>OLD.epoch OR NEW.session_id<>OLD.session_id THEN RAISE EXCEPTION 'writer ownership is immutable within an epoch'; END IF;
        IF OLD.state='Held' AND NEW.state='Held' THEN
            IF OLD.expires_at_ms<=floor(extract(epoch from clock_timestamp())*1000) OR NEW.expires_at_ms<OLD.expires_at_ms THEN RAISE EXCEPTION 'expired writer cannot renew'; END IF;
        ELSIF (OLD.state='Held' AND NEW.state='Draining') OR (OLD.state='Draining' AND NEW.state='Released') THEN
            IF NEW.expires_at_ms<>OLD.expires_at_ms THEN RAISE EXCEPTION 'draining cannot extend a lease'; END IF;
            IF NEW.state='Released' AND NOT EXISTS(SELECT 1 FROM candidate_writer_drains WHERE organization=OLD.organization AND lease_id=OLD.lease_id AND epoch=OLD.epoch) THEN RAISE EXCEPTION 'writer drain is unconfirmed'; END IF;
        ELSE RAISE EXCEPTION 'invalid writer transition'; END IF;
    END IF;
    RETURN NEW;
END;
$$;
