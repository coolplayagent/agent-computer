CREATE TABLE candidate_writer_leases (
    organization TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    revision BIGINT NOT NULL CHECK (revision > 0),
    state TEXT NOT NULL CHECK (state IN ('Held','Draining','Released')),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > 0),
    prepared_digest TEXT NOT NULL CHECK (prepared_digest ~ '^sha256:[0-9a-f]{64}$'),
    PRIMARY KEY (organization,lease_id),
    UNIQUE (organization,request_id),
    FOREIGN KEY (organization,request_id) REFERENCES candidate_preparations,
    FOREIGN KEY (organization,session_id) REFERENCES connection_sessions
);
CREATE TABLE candidate_writer_epochs (
    organization TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL CHECK (epoch > 0),
    session_id TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    PRIMARY KEY (organization,lease_id,epoch),
    FOREIGN KEY (organization,lease_id) REFERENCES candidate_writer_leases,
    FOREIGN KEY (organization,session_id) REFERENCES connection_sessions
);
CREATE TABLE candidate_writer_dispatches (
    organization TEXT NOT NULL,
    dispatch_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    input_digest TEXT NOT NULL CHECK (input_digest ~ '^sha256:[0-9a-f]{64}$'),
    PRIMARY KEY (organization,dispatch_id),
    UNIQUE (organization,lease_id,epoch),
    FOREIGN KEY (organization,lease_id,epoch) REFERENCES candidate_writer_epochs
);
CREATE TABLE candidate_writer_drains (
    organization TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    proof TEXT NOT NULL CHECK (proof='no_dispatch'),
    PRIMARY KEY (organization,lease_id,epoch),
    FOREIGN KEY (organization,lease_id,epoch) REFERENCES candidate_writer_epochs
);
CREATE TRIGGER immutable_writer_epoch BEFORE UPDATE OR DELETE ON candidate_writer_epochs
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TRIGGER immutable_writer_dispatch BEFORE UPDATE OR DELETE ON candidate_writer_dispatches
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TRIGGER immutable_writer_drain BEFORE UPDATE OR DELETE ON candidate_writer_drains
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE FUNCTION guard_writer_record_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head candidate_writer_leases%ROWTYPE;
BEGIN
    SELECT * INTO STRICT head FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    IF NEW.epoch <> head.epoch THEN RAISE EXCEPTION 'stale writer epoch'; END IF;
    IF TG_TABLE_NAME='candidate_writer_dispatches' THEN
        IF head.state <> 'Held' OR head.expires_at_ms <= floor(extract(epoch from clock_timestamp())*1000) THEN
            RAISE EXCEPTION 'writer dispatch is not admitted';
        END IF;
    ELSIF head.state <> 'Draining' OR EXISTS(SELECT 1 FROM candidate_writer_dispatches WHERE organization=NEW.organization AND lease_id=NEW.lease_id AND epoch=NEW.epoch) THEN
        RAISE EXCEPTION 'no-dispatch proof is unavailable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_writer_dispatch BEFORE INSERT ON candidate_writer_dispatches FOR EACH ROW EXECUTE FUNCTION guard_writer_record_insert();
CREATE TRIGGER check_writer_drain BEFORE INSERT ON candidate_writer_drains FOR EACH ROW EXECUTE FUNCTION guard_writer_record_insert();

CREATE FUNCTION guard_writer_lease_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
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
            IF NEW.state='Released' AND NOT EXISTS(SELECT 1 FROM candidate_writer_drains WHERE organization=OLD.organization AND lease_id=OLD.lease_id AND epoch=OLD.epoch AND proof='no_dispatch') THEN RAISE EXCEPTION 'writer drain is unconfirmed'; END IF;
        ELSE RAISE EXCEPTION 'invalid writer transition'; END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_writer_lease BEFORE UPDATE OR DELETE ON candidate_writer_leases FOR EACH ROW EXECUTE FUNCTION guard_writer_lease_mutation();
