-- A journal entry is historical data, never a replayable Pod/launch permit.
ALTER TABLE app_instances DROP CONSTRAINT app_instances_state_check;
ALTER TABLE app_instances ADD CHECK (state IN ('Queued','Blocked','Cancelled','Dispatching','Running','Draining'));
CREATE TABLE app_browser_journal (
    organization TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('dispatch','pod','arm','launch','running','stop')),
    payload JSONB NOT NULL CHECK (jsonb_typeof(payload)='object'),
    digest TEXT NOT NULL CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    recorded_at_ms BIGINT NOT NULL CHECK (recorded_at_ms>0),
    PRIMARY KEY (organization,instance_id,kind),
    FOREIGN KEY (organization,instance_id) REFERENCES app_instances
);
CREATE TRIGGER immutable_app_browser_journal BEFORE UPDATE OR DELETE ON app_browser_journal
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE FUNCTION guard_app_browser_journal() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE current_state TEXT; predecessor TEXT;
BEGIN
    SELECT state INTO STRICT current_state FROM app_instances WHERE organization=NEW.organization AND instance_id=NEW.instance_id FOR UPDATE;
    predecessor := CASE NEW.kind WHEN 'pod' THEN 'dispatch' WHEN 'arm' THEN 'pod' WHEN 'launch' THEN 'arm' WHEN 'running' THEN 'launch' WHEN 'stop' THEN 'arm' ELSE NULL END;
    IF (NEW.kind='dispatch' AND current_state<>'Queued')
        OR (NEW.kind IN ('arm','launch','running') AND current_state<>'Dispatching')
        OR (NEW.kind IN ('pod','stop') AND current_state NOT IN ('Dispatching','Running','Draining'))
        OR (predecessor IS NOT NULL AND NOT EXISTS (SELECT 1 FROM app_browser_journal WHERE organization=NEW.organization AND instance_id=NEW.instance_id AND kind=predecessor)) THEN
        RAISE EXCEPTION 'invalid browser journal order';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_app_browser_journal BEFORE INSERT ON app_browser_journal
    FOR EACH ROW EXECUTE FUNCTION guard_app_browser_journal();
CREATE OR REPLACE FUNCTION guard_app_admission() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE; app JSONB; sandbox JSONB;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'app history is retained'; END IF;
    IF TG_OP='UPDATE' THEN
        IF (to_jsonb(NEW)-ARRAY['state','revision']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision'])
            OR NEW.revision<>OLD.revision+1 OR NOT (
                (OLD.state='Queued' AND NEW.state IN ('Blocked','Cancelled','Dispatching'))
                OR (OLD.state='Blocked' AND NEW.state='Cancelled')
                OR (OLD.state='Dispatching' AND NEW.state IN ('Running','Draining'))
                OR (OLD.state='Running' AND NEW.state='Draining')) THEN
            RAISE EXCEPTION 'invalid app admission transition';
        END IF;
        IF NEW.state='Dispatching' AND NOT EXISTS (SELECT 1 FROM app_browser_journal WHERE organization=NEW.organization AND instance_id=NEW.instance_id AND kind='dispatch') THEN
            RAISE EXCEPTION 'missing browser dispatch';
        END IF;
        IF NEW.state='Running' AND NOT EXISTS (SELECT 1 FROM app_browser_journal WHERE organization=NEW.organization AND instance_id=NEW.instance_id AND kind='running') THEN
            RAISE EXCEPTION 'missing browser launch observation';
        END IF;
        IF NEW.state='Cancelled' AND EXISTS (SELECT 1 FROM app_browser_journal WHERE organization=NEW.organization AND instance_id=NEW.instance_id) THEN
            RAISE EXCEPTION 'dispatched browser cannot be cancelled';
        END IF;
        RETURN NEW;
    END IF;
    SELECT * INTO STRICT head FROM runtime_start_requests WHERE organization=NEW.organization AND request_id=NEW.request_id FOR UPDATE;
    SELECT r INTO app FROM jsonb_array_elements(head.snapshot->'resources') r
        WHERE r#>>'{reference,kind}'='app' AND r#>>'{reference,resource_id}'=NEW.app_id;
    SELECT r INTO sandbox FROM jsonb_array_elements(head.snapshot->'resources') r
        WHERE r#>>'{reference,kind}'='sandbox' AND r#>>'{reference,resource_id}'=NEW.sandbox_id;
    IF NEW.state<>'Queued' OR NEW.revision<>1 OR head.state<>'Prepared'
        OR NEW.computer_id<>head.computer_id OR NEW.computer_generation<>head.generation
        OR NEW.runtime_seconds>head.max_runtime_seconds OR app IS NULL OR sandbox IS NULL
        OR NEW.binding IS DISTINCT FROM jsonb_build_object('app',app,'sandbox',sandbox)
        OR app#>>'{spec,driver}'<>'chromium-playwright' OR sandbox#>>'{spec,runtimeClass}'<>'gvisor'
        OR app#>>'{spec,sandboxRef}' IS DISTINCT FROM 'id:'||NEW.sandbox_id
        OR app#>>'{spec,profileRef}' IS DISTINCT FROM (CASE WHEN NEW.profile_id IS NULL THEN NULL ELSE 'id:'||NEW.profile_id END)
        OR NOT EXISTS (SELECT 1 FROM runtime_controls WHERE organization=NEW.organization AND computer_id=NEW.computer_id AND active_request=NEW.request_id AND generation=NEW.computer_generation)
        OR NOT EXISTS (SELECT 1 FROM connection_sessions WHERE organization=NEW.organization AND session_id=NEW.session_id AND computer_id=NEW.computer_id AND principal=NEW.principal AND credential_id=NEW.credential_id AND state='Active' AND expires_at_ms>=NEW.expires_at_ms)
        OR (NEW.profile_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM browser_profile_owners WHERE organization=NEW.organization AND profile_id=NEW.profile_id AND principal=NEW.principal))
        OR NEW.app_generation<>1+COALESCE((SELECT max(app_generation) FROM app_instances WHERE organization=NEW.organization AND computer_id=NEW.computer_id AND app_id=NEW.app_id),0) THEN
        RAISE EXCEPTION 'invalid app admission binding';
    END IF;
    RETURN NEW;
END;
$$;
CREATE OR REPLACE FUNCTION invalidate_app_authority() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME='service_credentials' THEN
        IF NEW.revoked THEN
            UPDATE app_instances SET state=CASE WHEN state='Queued' THEN 'Blocked' ELSE 'Draining' END,revision=revision+1 WHERE organization=NEW.organization AND credential_id=NEW.credential_id AND state IN ('Queued','Dispatching','Running');
        END IF;
    ELSIF TG_TABLE_NAME='principals' THEN
        IF NOT NEW.enabled THEN
            UPDATE app_instances SET state=CASE WHEN state='Queued' THEN 'Blocked' ELSE 'Draining' END,revision=revision+1 WHERE organization=NEW.organization AND principal=NEW.principal AND state IN ('Queued','Dispatching','Running');
        END IF;
    ELSIF TG_TABLE_NAME='connection_sessions' THEN
        IF NEW.state<>'Active' THEN
            UPDATE app_instances SET state=CASE WHEN state='Queued' THEN 'Blocked' ELSE 'Draining' END,revision=revision+1 WHERE organization=NEW.organization AND session_id=NEW.session_id AND state IN ('Queued','Dispatching','Running');
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
-- Draining retains all existing active reservation indexes. A process-stop
-- proof does not establish profile checkpoint durability or permit slot release.
