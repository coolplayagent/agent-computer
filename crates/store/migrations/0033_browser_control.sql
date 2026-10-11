-- GUI leases do not grant process ownership and cannot release App reservations.
CREATE TABLE browser_controls (
    organization TEXT NOT NULL, instance_id TEXT NOT NULL,
    epoch BIGINT NOT NULL CHECK(epoch BETWEEN 1 AND 9007199254740991),
    state TEXT NOT NULL CHECK(state IN ('Owned','Draining','Idle','Blocked')),
    principal TEXT NOT NULL, credential_id TEXT NOT NULL, session_id TEXT NOT NULL,
    expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>0),
    drain_nonce TEXT,
    PRIMARY KEY(organization,instance_id),
    FOREIGN KEY(organization,instance_id) REFERENCES app_instances,
    FOREIGN KEY(organization,session_id) REFERENCES connection_sessions(organization,session_id)
);
CREATE TABLE browser_requests (
    sequence BIGSERIAL UNIQUE NOT NULL,
    organization TEXT NOT NULL, request_id TEXT NOT NULL, instance_id TEXT NOT NULL,
    principal TEXT NOT NULL, credential_id TEXT NOT NULL, session_id TEXT NOT NULL,
    epoch BIGINT NOT NULL CHECK(epoch BETWEEN 0 AND 9007199254740991),
    command JSONB NOT NULL CHECK(jsonb_typeof(command)='object'),
    input_digest TEXT NOT NULL CHECK(input_digest ~ '^sha256:[0-9a-f]{64}$'),
    state TEXT NOT NULL CHECK(state IN ('Queued','Dispatched','Complete','Rejected','Unknown')),
    dispatch_nonce TEXT,
    result JSONB,
    result_valid_until_ms BIGINT,
    created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
    expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>created_at_ms),
    PRIMARY KEY(organization,request_id),
    FOREIGN KEY(organization,instance_id) REFERENCES app_instances,
    FOREIGN KEY(organization,session_id) REFERENCES connection_sessions(organization,session_id),
    CHECK(octet_length(command::text)<=65536),
    CHECK(result IS NULL OR octet_length(result::text)<=3145728)
);
CREATE UNIQUE INDEX browser_one_dispatched ON browser_requests(organization,instance_id) WHERE state='Dispatched';
CREATE INDEX browser_pending ON browser_requests(organization,instance_id,sequence) WHERE state='Queued';
CREATE TABLE browser_action_journal (
    organization TEXT NOT NULL, request_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('begin','complete')),
    payload JSONB NOT NULL CHECK(jsonb_typeof(payload)='object'),
    digest TEXT NOT NULL CHECK(digest ~ '^sha256:[0-9a-f]{64}$'),
    PRIMARY KEY(organization,request_id,kind),
    FOREIGN KEY(organization,request_id) REFERENCES browser_requests
);
CREATE TRIGGER immutable_browser_action_journal BEFORE UPDATE OR DELETE ON browser_action_journal
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_browser_request() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'browser request history retained'; END IF;
    IF TG_OP='INSERT' THEN
        IF NEW.state<>'Queued' OR NEW.dispatch_nonce IS NOT NULL OR NEW.result IS NOT NULL OR NEW.result_valid_until_ms IS NOT NULL THEN RAISE EXCEPTION 'invalid initial browser request'; END IF;
    ELSE
        IF (to_jsonb(NEW)-ARRAY['state','dispatch_nonce','result','result_valid_until_ms']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','dispatch_nonce','result','result_valid_until_ms']) OR NOT (
            (OLD.state='Queued' AND NEW.state IN ('Dispatched','Rejected')) OR
            (OLD.state='Dispatched' AND NEW.state IN ('Complete','Rejected','Unknown'))) OR
            (OLD.dispatch_nonce IS NOT NULL AND NEW.dispatch_nonce IS DISTINCT FROM OLD.dispatch_nonce) OR
            (NEW.state='Dispatched' AND NEW.dispatch_nonce IS NULL) THEN
            RAISE EXCEPTION 'invalid browser request transition';
        END IF;
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER check_browser_request BEFORE INSERT OR UPDATE OR DELETE ON browser_requests
    FOR EACH ROW EXECUTE FUNCTION guard_browser_request();
CREATE FUNCTION guard_browser_control() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'browser control history retained'; END IF;
    IF TG_OP='INSERT' THEN
        IF NEW.epoch<>1 OR NEW.state<>'Owned' OR NEW.drain_nonce IS NOT NULL THEN RAISE EXCEPTION 'invalid initial control'; END IF;
    ELSIF OLD.state='Idle' AND NEW.state='Owned' THEN
        IF NEW.epoch<>OLD.epoch+1 OR NEW.drain_nonce IS NOT NULL THEN RAISE EXCEPTION 'invalid control epoch'; END IF;
    ELSE
        IF (to_jsonb(NEW)-ARRAY['state','drain_nonce']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','drain_nonce']) OR NOT (
            (OLD.state='Owned' AND NEW.state IN ('Draining','Blocked')) OR
            (OLD.state='Draining' AND NEW.state IN ('Idle','Blocked')) OR
            (OLD.state='Draining' AND NEW.state='Draining' AND OLD.drain_nonce IS NULL AND NEW.drain_nonce IS NOT NULL)) OR
            (OLD.drain_nonce IS NOT NULL AND NEW.drain_nonce IS DISTINCT FROM OLD.drain_nonce) OR
            (NEW.state='Idle' AND NEW.drain_nonce IS NULL) THEN RAISE EXCEPTION 'invalid control transition'; END IF;
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER check_browser_control BEFORE INSERT OR UPDATE OR DELETE ON browser_controls
    FOR EACH ROW EXECUTE FUNCTION guard_browser_control();
CREATE FUNCTION invalidate_browser_control() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME='service_credentials' THEN
        IF NEW.revoked THEN UPDATE browser_controls SET state='Draining' WHERE organization=NEW.organization AND credential_id=NEW.credential_id AND state='Owned'; END IF;
    ELSIF TG_TABLE_NAME='principals' THEN
        IF NOT NEW.enabled THEN UPDATE browser_controls SET state='Draining' WHERE organization=NEW.organization AND principal=NEW.principal AND state='Owned'; END IF;
    ELSIF TG_TABLE_NAME='connection_sessions' THEN
        IF NEW.state<>'Active' THEN UPDATE browser_controls SET state='Draining' WHERE organization=NEW.organization AND session_id=NEW.session_id AND state='Owned'; END IF;
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER revoke_browser_credential AFTER UPDATE ON service_credentials FOR EACH ROW EXECUTE FUNCTION invalidate_browser_control();
CREATE TRIGGER revoke_browser_principal AFTER UPDATE ON principals FOR EACH ROW EXECUTE FUNCTION invalidate_browser_control();
CREATE TRIGGER revoke_browser_session AFTER UPDATE ON connection_sessions FOR EACH ROW EXECUTE FUNCTION invalidate_browser_control();

CREATE FUNCTION guard_browser_action_journal() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM browser_requests WHERE organization=NEW.organization AND request_id=NEW.request_id AND state='Dispatched' AND command->>'kind'='act') OR
       (NEW.kind='complete' AND NOT EXISTS (SELECT 1 FROM browser_action_journal WHERE organization=NEW.organization AND request_id=NEW.request_id AND kind='begin')) THEN
        RAISE EXCEPTION 'invalid browser action journal order';
    END IF;
    RETURN NEW;
END; $$;
CREATE TRIGGER check_browser_action_journal BEFORE INSERT ON browser_action_journal FOR EACH ROW EXECUTE FUNCTION guard_browser_action_journal();
