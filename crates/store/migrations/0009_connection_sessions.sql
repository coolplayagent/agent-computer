-- Logical connections are independent of compute generations and transport.
CREATE TABLE connection_sessions (
    organization TEXT NOT NULL,
    session_id TEXT NOT NULL,
    computer_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    credential_id TEXT NOT NULL REFERENCES service_credentials,
    requested JSONB NOT NULL CHECK (jsonb_typeof(requested) = 'array'),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms > 0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > created_at_ms),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    revocation_revision BIGINT NOT NULL DEFAULT 0 CHECK (revocation_revision >= 0),
    state TEXT NOT NULL DEFAULT 'Active' CHECK (state IN ('Active','Closed','Revoked')),
    last_seen_at_ms BIGINT NOT NULL CHECK (last_seen_at_ms >= created_at_ms),
    activity TEXT NOT NULL DEFAULT 'idle' CHECK (activity IN ('idle','active')),
    visibility TEXT NOT NULL DEFAULT 'hidden' CHECK (visibility IN ('hidden','visible')),
    PRIMARY KEY (organization,session_id),
    FOREIGN KEY (organization,computer_id) REFERENCES resource_definitions,
    FOREIGN KEY (organization,principal) REFERENCES principals
);
CREATE INDEX connection_admission_capacity ON connection_sessions (organization,principal,computer_id,expires_at_ms) WHERE state='Active';

CREATE FUNCTION guard_connection_session_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'connection history is immutable';
    END IF;
    IF (to_jsonb(NEW) - ARRAY['revision','revocation_revision','state','last_seen_at_ms','activity','visibility'])
        IS DISTINCT FROM (to_jsonb(OLD) - ARRAY['revision','revocation_revision','state','last_seen_at_ms','activity','visibility'])
       OR OLD.state <> 'Active'
       OR NEW.revision <> OLD.revision + 1
       OR NEW.last_seen_at_ms < OLD.last_seen_at_ms
       OR NEW.revocation_revision <> OLD.revocation_revision + (CASE WHEN NEW.state IN ('Closed','Revoked') THEN 1 ELSE 0 END) THEN
        RAISE EXCEPTION 'invalid connection session mutation';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER immutable_connection_session BEFORE UPDATE OR DELETE ON connection_sessions
    FOR EACH ROW EXECUTE FUNCTION guard_connection_session_mutation();
