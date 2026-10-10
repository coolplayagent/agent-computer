-- New bindings explicitly enable protocol v3; historical records remain fixed.
ALTER TABLE execution_requests ADD CONSTRAINT execution_stream_policy CHECK (
    NOT (binding ? 'output_stream') OR binding->'output_stream'='{"version":1}'::jsonb);

CREATE TABLE execution_output_chunk_intents (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence BETWEEN 1 AND 8192),
    manifest JSONB NOT NULL CHECK (jsonb_typeof(manifest)='object'),
    manifest_digest TEXT NOT NULL CHECK (manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms>0),
    PRIMARY KEY (organization,execution_id,sequence),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_startup_grants
);
CREATE TABLE execution_output_chunks (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    manifest_digest TEXT NOT NULL CHECK (manifest_digest ~ '^sha256:[0-9a-f]{64}$'),
    verified_at_ms BIGINT NOT NULL CHECK (verified_at_ms>0),
    PRIMARY KEY (organization,execution_id,sequence),
    FOREIGN KEY (organization,execution_id,sequence) REFERENCES execution_output_chunk_intents
);
CREATE TRIGGER immutable_execution_output_chunk_intent BEFORE UPDATE OR DELETE ON execution_output_chunk_intents
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TRIGGER immutable_execution_output_chunk BEFORE UPDATE OR DELETE ON execution_output_chunks
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE FUNCTION guard_execution_output_chunk() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r execution_requests%ROWTYPE;
    d execution_dispatch_intents%ROWTYPE;
    s execution_startup_grants%ROWTYPE;
    w execution_watchdog_arms%ROWTYPE;
    m JSONB;
    o JSONB;
    prior JSONB;
    same_stream JSONB;
    last_sequence INTEGER;
    cap BIGINT;
    size BIGINT;
    pos BIGINT;
    observed BIGINT;
BEGIN
    -- Serialize even direct SQL writers on this immutable execution identity.
    SELECT * INTO STRICT r FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id FOR UPDATE;
    IF TG_TABLE_NAME='execution_output_chunks' THEN
        IF NOT EXISTS(SELECT 1 FROM execution_output_chunk_intents i WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id AND i.sequence=NEW.sequence AND i.manifest_digest=NEW.manifest_digest AND NEW.verified_at_ms>=i.created_at_ms)
            OR (NEW.sequence>1 AND NOT EXISTS(SELECT 1 FROM execution_output_chunks c WHERE c.organization=NEW.organization AND c.execution_id=NEW.execution_id AND c.sequence=NEW.sequence-1)) THEN
            RAISE EXCEPTION 'output chunk verification does not match its contiguous intent';
        END IF;
        RETURN NEW;
    END IF;
    SELECT * INTO STRICT d FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT s FROM execution_startup_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT w FROM execution_watchdog_arms WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT sequence,manifest INTO last_sequence,prior FROM execution_output_chunk_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id ORDER BY sequence DESC LIMIT 1;
    m:=NEW.manifest;o:=m->'object';cap:=(r.input#>>'{command,output_limit_bytes}')::bigint;
    size:=COALESCE((o->>'size')::bigint,-1);pos:=COALESCE((m->>'offset')::bigint,-1);observed:=COALESCE((m->>'observed_bytes')::bigint,-1);
    IF r.binding->'output_stream' IS DISTINCT FROM '{"version":1}'::jsonb
        OR s.grant_body->>'version' IS DISTINCT FROM '3'
        OR NEW.sequence<>COALESCE(last_sequence,0)+1
        OR (last_sequence IS NOT NULL AND NOT EXISTS(SELECT 1 FROM execution_output_chunks WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND sequence=last_sequence))
        OR EXISTS(SELECT 1 FROM execution_output_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR m->>'version' IS DISTINCT FROM '1'
        OR m->>'organization' IS DISTINCT FROM NEW.organization OR m->>'execution_id' IS DISTINCT FROM NEW.execution_id
        OR m->>'sequence' IS DISTINCT FROM NEW.sequence::text
        OR m->>'dispatch_digest' IS DISTINCT FROM d.intent_digest
        OR m->>'grant_digest' IS DISTINCT FROM s.grant_digest
        OR m->>'pod_uid' IS DISTINCT FROM s.pod_uid OR m->>'pod_uid' IS DISTINCT FROM w.pod_uid
        OR m->>'arm_digest' IS DISTINCT FROM w.evidence_digest
        OR m->>'previous_digest' IS DISTINCT FROM COALESCE(prior->>'chunk_digest',s.grant_digest)
        OR COALESCE(m->>'chunk_digest','') !~ '^sha256:[0-9a-f]{64}$'
        OR COALESCE(m->>'stream','') NOT IN ('stdout','stderr')
        OR jsonb_typeof(m->'eof') IS DISTINCT FROM 'boolean' OR jsonb_typeof(m->'truncated') IS DISTINCT FROM 'boolean'
        OR jsonb_typeof(o) IS DISTINCT FROM 'object'
        OR m-ARRAY['version','organization','execution_id','pod_uid','dispatch_digest','grant_digest','arm_digest','sequence','previous_digest','chunk_digest','stream','offset','observed_bytes','truncated','eof','object'] IS DISTINCT FROM '{}'::jsonb
        OR o-ARRAY['store_digest','key','sha256','size'] IS DISTINCT FROM '{}'::jsonb
        OR COALESCE(o->>'store_digest','') !~ '^sha256:[0-9a-f]{64}$'
        OR COALESCE(o->>'sha256','') !~ '^sha256:[0-9a-f]{64}$'
        OR o->>'key' IS DISTINCT FROM 'execution-outputs/v1/'||NEW.organization||'/'||NEW.execution_id||'/'||substring(o->>'sha256' from 8)
        OR (prior IS NOT NULL AND o->>'store_digest' IS DISTINCT FROM prior#>>'{object,store_digest}')
        OR cap NOT BETWEEN 0 AND 1048576 OR size NOT BETWEEN 0 AND 8192 OR pos NOT BETWEEN 0 AND cap
        OR pos+size>cap OR observed<pos+size OR (size=0 AND NOT (m->>'eof')::boolean)
        OR (m->>'truncated')::boolean IS DISTINCT FROM (observed>cap)
        OR ((m->>'eof')::boolean AND pos+size<>LEAST(observed,cap))
        OR NEW.created_at_ms<s.granted_at_ms THEN
        RAISE EXCEPTION 'output chunk is not bound to its admitted stream';
    END IF;
    SELECT manifest INTO same_stream FROM execution_output_chunk_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND manifest->>'stream'=m->>'stream' ORDER BY sequence DESC LIMIT 1;
    IF pos<>COALESCE((same_stream->>'offset')::bigint+(same_stream#>>'{object,size}')::bigint,0)
        OR observed<COALESCE((same_stream->>'observed_bytes')::bigint,0)
        OR COALESCE((same_stream->>'eof')::boolean,false) THEN
        RAISE EXCEPTION 'output stream offset or EOF is inconsistent';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_output_chunk_intent BEFORE INSERT ON execution_output_chunk_intents
    FOR EACH ROW EXECUTE FUNCTION guard_execution_output_chunk();
CREATE TRIGGER check_execution_output_chunk BEFORE INSERT ON execution_output_chunks
    FOR EACH ROW EXECUTE FUNCTION guard_execution_output_chunk();

CREATE FUNCTION execution_output_stream_progress(org TEXT, execution TEXT) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE s execution_startup_grants%ROWTYPE;
    latest_sequence INTEGER;
    m JSONB;
BEGIN
    SELECT * INTO s FROM execution_startup_grants WHERE organization=$1 AND execution_id=$2;
    IF s.grant_body->>'version' IS DISTINCT FROM '3' THEN RETURN NULL; END IF;
    SELECT i.sequence,i.manifest INTO latest_sequence,m FROM execution_output_chunk_intents i WHERE i.organization=$1 AND i.execution_id=$2 ORDER BY i.sequence DESC LIMIT 1;
    IF latest_sequence IS NULL THEN RETURN jsonb_build_object('sequence',0,'last_digest',s.grant_digest); END IF;
    IF NOT EXISTS(SELECT 1 FROM execution_output_chunks c WHERE c.organization=$1 AND c.execution_id=$2 AND c.sequence=latest_sequence) THEN
        RAISE EXCEPTION 'pending output chunk cannot support a final report';
    END IF;
    RETURN jsonb_build_object('sequence',latest_sequence,'last_digest',m->>'chunk_digest');
END;
$$;

CREATE FUNCTION guard_execution_output_stream_final() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE stream TEXT;
    prior JSONB;
    summary JSONB;
    end_offset BIGINT;
BEGIN
    IF NEW.manifest->>'version' IS DISTINCT FROM '3' THEN RETURN NEW; END IF;
    PERFORM 1 FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id FOR UPDATE;
    FOREACH stream IN ARRAY ARRAY['stdout','stderr'] LOOP
        SELECT manifest INTO prior FROM execution_output_chunk_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND manifest->>'stream'=stream ORDER BY sequence DESC LIMIT 1;
        summary:=NEW.manifest->'summary'->stream;
        end_offset:=COALESCE((prior->>'offset')::bigint+(prior#>>'{object,size}')::bigint,0);
        IF end_offset>COALESCE((summary->>'retained_bytes')::bigint,-1)
            OR COALESCE((prior->>'observed_bytes')::bigint,0)>COALESCE((summary->>'observed_bytes')::bigint,-1)
            OR (COALESCE((prior->>'eof')::boolean,false) AND (
                prior->'observed_bytes' IS DISTINCT FROM summary->'observed_bytes'
                OR summary->>'eof' IS DISTINCT FROM 'true'
                OR end_offset<>(summary->>'retained_bytes')::bigint))
            OR (NEW.manifest#>>'{summary,observed_outcome}'='succeeded' AND COALESCE(prior->>'eof','false')<>'true') THEN
            RAISE EXCEPTION 'final output contradicts the observed chunk prefix';
        END IF;
    END LOOP;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_output_stream_final BEFORE INSERT ON execution_output_intents
    FOR EACH ROW EXECUTE FUNCTION guard_execution_output_stream_final();

CREATE OR REPLACE FUNCTION guard_execution_startup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN execution_dispatch_intents i USING(organization,execution_id)
        JOIN candidate_writer_leases l ON l.organization=r.organization AND l.lease_id=r.lease_id AND l.epoch=r.epoch
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state='Dispatching' AND l.state='Held'
        AND l.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND i.deadline_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND NEW.granted_at_ms>=i.started_at_ms AND NEW.granted_at_ms<i.deadline_at_ms
        AND NEW.challenge->>'execution_id'=r.execution_id AND (NEW.challenge->>'generation')::bigint=(r.binding->>'generation')::bigint
        AND NEW.challenge->>'version' IS NOT DISTINCT FROM NEW.grant_body->>'version'
        AND NEW.grant_body->>'version'=CASE WHEN r.binding ? 'output_stream' THEN '3' WHEN i.hard_deadline_at_ms IS NOT NULL THEN '2' ELSE '1' END
        AND ((i.hard_deadline_at_ms IS NULL
                AND NOT (NEW.grant_body ? 'hard_budget_ms'))
            OR (i.hard_deadline_at_ms IS NOT NULL
                AND (NEW.grant_body->>'hard_budget_ms')::bigint BETWEEN (NEW.grant_body->>'lease_budget_ms')::bigint AND i.hard_deadline_at_ms-NEW.granted_at_ms
                AND EXISTS(SELECT 1 FROM execution_watchdog_arms w WHERE w.organization=NEW.organization AND w.execution_id=NEW.execution_id
                    AND w.evidence#>>'{armed,request,renewal,authority_digest}'=i.intent_digest)))
        AND (NEW.grant_body->>'lease_budget_ms')::bigint BETWEEN 1 AND i.deadline_at_ms-NEW.granted_at_ms) THEN
        RAISE EXCEPTION 'execution startup is not admitted';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_execution_renewal_grant() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r execution_requests%ROWTYPE;
    d execution_dispatch_intents%ROWTYPE;
    s execution_startup_grants%ROWTYPE;
    l candidate_writer_leases%ROWTYPE;
    prior execution_renewal_grants%ROWTYPE;
    expected_sequence INTEGER := 1;
    prior_digest TEXT;
BEGIN
    SELECT * INTO STRICT r FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT d FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT s FROM execution_startup_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT l FROM candidate_writer_leases WHERE organization=r.organization AND lease_id=r.lease_id FOR UPDATE;
    SELECT g.* INTO prior FROM execution_renewal_grants g JOIN execution_renewal_acks a USING(organization,execution_id,sequence)
        WHERE g.organization=NEW.organization AND g.execution_id=NEW.execution_id ORDER BY sequence DESC LIMIT 1;
    prior_digest := s.grant_digest;
    IF FOUND THEN expected_sequence := prior.sequence+1; prior_digest := prior.grant_digest; END IF;
    IF r.state<>'Dispatching' OR l.state<>'Held' OR l.epoch<>r.epoch
        OR EXISTS(SELECT 1 FROM execution_output_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR EXISTS(SELECT 1 FROM execution_completions WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR l.expires_at_ms<=floor(extract(epoch from clock_timestamp())*1000)
        OR d.hard_deadline_at_ms IS NULL OR NEW.deadline_at_ms>d.hard_deadline_at_ms
        OR NEW.sequence<>expected_sequence OR NEW.previous_grant_digest<>prior_digest
        OR NEW.previous_deadline_at_ms IS DISTINCT FROM execution_effective_deadline(NEW.organization,NEW.execution_id)
        OR NEW.previous_deadline_at_ms<=floor(extract(epoch from clock_timestamp())*1000)
        OR NOT (s.grant_body ? 'hard_budget_ms')
        OR NEW.challenge->>'version' IS DISTINCT FROM '1'
        OR NEW.challenge->>'startup_grant_digest' IS DISTINCT FROM s.grant_digest
        OR (NEW.challenge->>'sequence')::integer IS DISTINCT FROM NEW.sequence
        OR COALESCE(NEW.challenge->>'nonce','') !~ '^[0-9a-f]{64}$'
        OR NEW.grant_body->>'version' IS DISTINCT FROM '1'
        OR COALESCE(NEW.grant_body->>'challenge_digest','') !~ '^sha256:[0-9a-f]{64}$'
        OR COALESCE((NEW.grant_body->>'lease_budget_ms')::bigint,0) NOT BETWEEN 1 AND 30000
        OR NEW.deadline_at_ms IS DISTINCT FROM LEAST(d.hard_deadline_at_ms,NEW.granted_at_ms+(NEW.grant_body->>'lease_budget_ms')::bigint)
        OR NEW.node_command->>'version' IS DISTINCT FROM '1'
        OR (NEW.node_command->>'sequence')::integer IS DISTINCT FROM NEW.sequence
        OR NEW.node_command->>'grant_digest' IS DISTINCT FROM NEW.grant_digest
        OR COALESCE(NEW.node_command->>'request_digest','') !~ '^sha256:[0-9a-f]{64}$' THEN
        RAISE EXCEPTION 'execution renewal is not admitted';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION execution_renewal_progress(org TEXT, execution TEXT) RETURNS jsonb LANGUAGE sql AS $$
    SELECT CASE WHEN s.grant_body ? 'hard_budget_ms' THEN COALESCE(
        (SELECT jsonb_build_object('sequence',g.sequence,'grant_digest',g.grant_digest)
            FROM execution_renewal_grants g JOIN execution_renewal_acks a USING(organization,execution_id,sequence)
            WHERE g.organization=$1 AND g.execution_id=$2 ORDER BY sequence DESC LIMIT 1),
        jsonb_build_object('sequence',0,'grant_digest',s.grant_digest)) ELSE NULL END
    FROM execution_startup_grants s WHERE s.organization=$1 AND s.execution_id=$2;
$$;

CREATE OR REPLACE FUNCTION guard_execution_output() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE m JSONB;
    o JSONB;
BEGIN
    IF TG_TABLE_NAME='execution_outputs' THEN
        IF NOT EXISTS(SELECT 1 FROM execution_output_intents i WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id
            AND i.manifest_digest=NEW.manifest_digest AND NEW.verified_at_ms>=i.created_at_ms) THEN
            RAISE EXCEPTION 'output publication does not match its intent';
        END IF;
        RETURN NEW;
    END IF;
    PERFORM 1 FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id FOR UPDATE;
    IF NOT EXISTS(SELECT 1 FROM execution_dispatch_intents d JOIN execution_startup_grants g USING(organization,execution_id)
        JOIN execution_watchdog_arms w USING(organization,execution_id)
        WHERE d.organization=NEW.organization AND d.execution_id=NEW.execution_id AND d.intent_digest=NEW.dispatch_digest
        AND g.pod_uid=NEW.pod_uid AND w.pod_uid=NEW.pod_uid AND g.grant_digest=NEW.grant_digest AND w.evidence_digest=NEW.arm_digest)
        OR NEW.manifest->>'version' IS DISTINCT FROM (SELECT grant_body->>'version' FROM execution_startup_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR NEW.manifest->'renewal' IS DISTINCT FROM execution_renewal_progress(NEW.organization,NEW.execution_id)
        OR NEW.manifest->'stream' IS DISTINCT FROM execution_output_stream_progress(NEW.organization,NEW.execution_id)
        OR NEW.manifest->>'organization' IS DISTINCT FROM NEW.organization
        OR NEW.manifest->>'execution_id' IS DISTINCT FROM NEW.execution_id
        OR NEW.manifest->>'pod_uid' IS DISTINCT FROM NEW.pod_uid
        OR NEW.manifest->>'dispatch_digest' IS DISTINCT FROM NEW.dispatch_digest
        OR NEW.manifest->>'grant_digest' IS DISTINCT FROM NEW.grant_digest
        OR NEW.manifest->>'arm_digest' IS DISTINCT FROM NEW.arm_digest
        OR jsonb_typeof(NEW.manifest->'objects') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'execution output binding does not match';
    END IF;
    m:=NEW.manifest;
    IF jsonb_array_length(m->'objects')<>4 THEN RAISE EXCEPTION 'output object set is incomplete'; END IF;
    FOR o IN SELECT value FROM jsonb_array_elements(m->'objects') LOOP
        IF COALESCE(o->>'store_digest','') !~ '^sha256:[0-9a-f]{64}$'
            OR COALESCE(o->>'sha256','') !~ '^sha256:[0-9a-f]{64}$'
            OR COALESCE((o->>'size')::bigint,-1) NOT BETWEEN 0 AND 8404992
            OR o->>'store_digest' IS DISTINCT FROM m->'objects'->0->>'store_digest'
            OR o->>'key' IS DISTINCT FROM 'execution-outputs/v1/'||NEW.organization||'/'||NEW.execution_id||'/'||substring(o->>'sha256' from 8) THEN
            RAISE EXCEPTION 'invalid execution output object';
        END IF;
    END LOOP;
    RETURN NEW;
END;
$$;
