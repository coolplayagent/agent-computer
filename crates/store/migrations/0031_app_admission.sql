-- App admission is a reservation only. This migration grants no launch/action permit.
CREATE TABLE browser_profile_owners (
    organization TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    PRIMARY KEY (organization,profile_id),
    FOREIGN KEY (organization,profile_id) REFERENCES catalog_references (organization,resource_id),
    FOREIGN KEY (organization,principal) REFERENCES principals
);
CREATE TRIGGER immutable_browser_profile_owner BEFORE UPDATE OR DELETE ON browser_profile_owners
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE TABLE app_instances (
    organization TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    computer_id TEXT NOT NULL,
    computer_generation BIGINT NOT NULL CHECK (computer_generation>0),
    app_id TEXT NOT NULL,
    app_generation BIGINT NOT NULL CHECK (app_generation>0),
    sandbox_id TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    credential_id TEXT NOT NULL REFERENCES service_credentials,
    session_id TEXT NOT NULL,
    runtime_seconds INTEGER NOT NULL CHECK (runtime_seconds BETWEEN 1 AND 300),
    binding JSONB NOT NULL CHECK (jsonb_typeof(binding)='object'),
    binding_digest TEXT NOT NULL CHECK (binding_digest ~ '^sha256:[0-9a-f]{64}$'),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms>0),
    expires_at_ms BIGINT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    state TEXT NOT NULL DEFAULT 'Queued' CHECK (state IN ('Queued','Blocked','Cancelled')),
    PRIMARY KEY (organization,instance_id),
    UNIQUE (organization,computer_id,app_id,app_generation),
    FOREIGN KEY (organization,computer_id,request_id) REFERENCES runtime_start_requests (organization,computer_id,request_id),
    FOREIGN KEY (organization,app_id) REFERENCES resource_definitions (organization,resource_id),
    FOREIGN KEY (organization,sandbox_id) REFERENCES resource_definitions (organization,resource_id),
    FOREIGN KEY (organization,profile_id) REFERENCES browser_profile_owners,
    FOREIGN KEY (organization,session_id) REFERENCES connection_sessions,
    FOREIGN KEY (organization,principal) REFERENCES principals,
    CHECK (expires_at_ms>created_at_ms AND expires_at_ms-created_at_ms<=runtime_seconds::bigint*1000)
);
CREATE UNIQUE INDEX app_active_definition ON app_instances (organization,computer_id,app_id) WHERE state<>'Cancelled';
CREATE UNIQUE INDEX app_active_sandbox ON app_instances (organization,request_id,sandbox_id) WHERE state<>'Cancelled';
CREATE UNIQUE INDEX app_active_profile ON app_instances (organization,profile_id) WHERE state<>'Cancelled' AND profile_id IS NOT NULL;
CREATE INDEX app_owner_capacity ON app_instances (organization,principal) WHERE state<>'Cancelled';

CREATE FUNCTION guard_app_admission() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE head runtime_start_requests%ROWTYPE; app JSONB; sandbox JSONB;
BEGIN
    IF TG_OP='DELETE' THEN RAISE EXCEPTION 'app history is retained'; END IF;
    IF TG_OP='UPDATE' THEN
        IF (to_jsonb(NEW)-ARRAY['state','revision']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision'])
            OR NEW.revision<>OLD.revision+1 OR NOT (
                (OLD.state='Queued' AND NEW.state IN ('Blocked','Cancelled'))
                OR (OLD.state='Blocked' AND NEW.state='Cancelled')) THEN
            RAISE EXCEPTION 'invalid app admission transition';
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
CREATE TRIGGER check_app_admission BEFORE INSERT OR UPDATE OR DELETE ON app_instances
    FOR EACH ROW EXECUTE FUNCTION guard_app_admission();

-- Revocation is sticky even if the administrator later restores the grant or principal.
CREATE FUNCTION invalidate_app_authority() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME='service_credentials' THEN
        IF NEW.revoked THEN
            UPDATE app_instances SET state='Blocked',revision=revision+1 WHERE organization=NEW.organization AND credential_id=NEW.credential_id AND state='Queued';
        END IF;
    ELSIF TG_TABLE_NAME='principals' THEN
        IF NOT NEW.enabled THEN
            UPDATE app_instances SET state='Blocked',revision=revision+1 WHERE organization=NEW.organization AND principal=NEW.principal AND state='Queued';
        END IF;
    ELSIF TG_TABLE_NAME='connection_sessions' THEN
        IF NEW.state<>'Active' THEN
            UPDATE app_instances SET state='Blocked',revision=revision+1 WHERE organization=NEW.organization AND session_id=NEW.session_id AND state='Queued';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER revoke_app_credential AFTER UPDATE ON service_credentials FOR EACH ROW EXECUTE FUNCTION invalidate_app_authority();
CREATE TRIGGER revoke_app_principal AFTER UPDATE ON principals FOR EACH ROW EXECUTE FUNCTION invalidate_app_authority();
CREATE TRIGGER close_app_connection AFTER UPDATE ON connection_sessions FOR EACH ROW EXECUTE FUNCTION invalidate_app_authority();

-- A Computer cannot release its reserved resources or seal while an App reservation remains.
CREATE FUNCTION guard_computer_app_reservations() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state IN ('Draining','Sealing','Sealed','Stopped','Cancelled') AND EXISTS (
        SELECT 1 FROM app_instances WHERE organization=NEW.organization AND request_id=NEW.request_id AND state<>'Cancelled') THEN
        RAISE EXCEPTION 'app reservations must be cancelled first';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_computer_app_reservations BEFORE UPDATE ON runtime_start_requests
    FOR EACH ROW EXECUTE FUNCTION guard_computer_app_reservations();
