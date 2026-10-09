-- API scopes cap credentials; resource grants separately authorize each action.
-- Existing definition credentials and creator grants acquire no runtime access.
ALTER TABLE service_credentials DROP CONSTRAINT service_credentials_scopes_check;
ALTER TABLE service_credentials ADD CONSTRAINT service_credentials_scopes_check CHECK (
    cardinality(scopes) BETWEEN 1 AND 13
    AND scopes <@ ARRAY[
        'definitions.validate', 'definitions.manage',
        'runtime.connect', 'runtime.read', 'runtime.observe', 'runtime.app.use',
        'runtime.activate', 'runtime.execute', 'runtime.modify', 'runtime.control',
        'runtime.publish', 'runtime.manage', 'runtime.delete'
    ]::TEXT[]
);

CREATE TABLE runtime_grants (
    organization TEXT NOT NULL,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('computer','workspace','app','browser_profile')),
    resource_id TEXT NOT NULL CHECK (resource_id ~ '^[A-Za-z0-9_-]{1,128}$'),
    permission TEXT NOT NULL CHECK (permission IN ('connect','read','observe','app.use','activate','execute','modify','control','publish','manage','delete')),
    max_runtime_seconds INTEGER,
    PRIMARY KEY (organization,principal,kind,resource_id,permission),
    FOREIGN KEY (organization,principal) REFERENCES principals,
    CHECK (
        (permission = 'activate' AND max_runtime_seconds BETWEEN 1 AND 86400 AND max_runtime_seconds IS NOT NULL)
        OR (permission <> 'activate' AND max_runtime_seconds IS NULL)
    )
);
-- Targets span resource_definitions and catalog_references. Trusted grant writes
-- and each authorization verify the exact organization/kind/ID under the stream
-- lock. Names, shared organization membership and definition grants never match.
