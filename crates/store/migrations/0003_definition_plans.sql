-- Explicit definition permissions are independent of credential scopes and all
-- runtime permissions. Names remain reserved; deletion/name reuse is not exposed.
CREATE TABLE definition_grants (
    organization TEXT NOT NULL,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('declaration','volume','workspace','sandbox','app','agent','computer','storage_class','network_policy','browser_profile','secret')),
    name TEXT NOT NULL,
    permission TEXT NOT NULL CHECK (permission IN ('create','manage','reference')),
    PRIMARY KEY (organization,principal,kind,name,permission),
    FOREIGN KEY (organization,principal) REFERENCES principals,
    CHECK (permission <> 'create' OR name = '*')
);

CREATE TABLE catalog_references (
    organization TEXT NOT NULL REFERENCES organization_streams,
    resource_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('storage_class','network_policy','browser_profile','secret')),
    name TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    PRIMARY KEY (organization,resource_id),
    UNIQUE (organization,kind,name)
);

CREATE TABLE resource_definitions (
    organization TEXT NOT NULL REFERENCES organization_streams,
    resource_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('volume','workspace','sandbox','app','agent','computer')),
    name TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    PRIMARY KEY (organization,resource_id),
    UNIQUE (organization,kind,name)
);
CREATE TABLE resource_spec_versions (
    organization TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    digest TEXT NOT NULL CHECK (digest ~ '^sha256:[0-9a-f]{64}$'),
    spec JSONB NOT NULL,
    dependencies JSONB NOT NULL,
    PRIMARY KEY (organization,resource_id,revision),
    FOREIGN KEY (organization,resource_id) REFERENCES resource_definitions DEFERRABLE INITIALLY DEFERRED
);
ALTER TABLE resource_definitions ADD CONSTRAINT resource_head_version
    FOREIGN KEY (organization,resource_id,revision) REFERENCES resource_spec_versions DEFERRABLE INITIALLY DEFERRED;
CREATE TRIGGER immutable_resource_spec BEFORE UPDATE OR DELETE ON resource_spec_versions
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE TABLE definition_plans (
    organization TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    digest TEXT NOT NULL,
    preview JSONB NOT NULL,
    canonical BYTEA NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    PRIMARY KEY (organization,plan_id),
    FOREIGN KEY (organization,principal) REFERENCES principals
);
CREATE TRIGGER immutable_definition_plan BEFORE UPDATE OR DELETE ON definition_plans
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE TABLE operations (
    organization TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'Queued' CHECK (state IN ('Queued','Running','Succeeded','Failed','Blocked')),
    event_sequence BIGINT NOT NULL CHECK (event_sequence > 0),
    PRIMARY KEY (organization,operation_id),
    UNIQUE (organization,plan_id),
    FOREIGN KEY (organization,plan_id) REFERENCES definition_plans,
    FOREIGN KEY (organization,principal) REFERENCES principals
);
CREATE TABLE reconcile_intents (
    organization TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    resource_id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    state TEXT NOT NULL DEFAULT 'Pending' CHECK (state IN ('Pending','Running','Succeeded','Failed','Blocked')),
    requires_drain BOOLEAN NOT NULL,
    PRIMARY KEY (organization,operation_id,resource_id),
    FOREIGN KEY (organization,operation_id) REFERENCES operations,
    FOREIGN KEY (organization,resource_id,revision) REFERENCES resource_spec_versions
);
