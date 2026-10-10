-- Old input/binding/dispatch hashes are retained. Only a new immutable policy
-- permits protocol v2; neither queued records nor old rows gain renewability.
ALTER TABLE execution_dispatch_intents ADD COLUMN hard_deadline_at_ms BIGINT;
ALTER TABLE execution_dispatch_intents ADD CHECK (hard_deadline_at_ms IS NULL OR
    (hard_deadline_at_ms>=deadline_at_ms AND hard_deadline_at_ms<=started_at_ms+3630000));
ALTER TABLE execution_requests ADD CONSTRAINT execution_renewal_input CHECK (
    NOT (input ? 'renewable') OR jsonb_typeof(input->'renewable')='boolean');
ALTER TABLE execution_requests ADD CONSTRAINT execution_renewal_policy CHECK (
    NOT (binding ? 'execution_lease') OR
    (binding->'execution_lease'->>'version' IS NOT DISTINCT FROM '1'
        AND COALESCE((binding->'execution_lease'->>'max_budget_ms')::bigint,0) BETWEEN 1 AND 3630000
        AND input->>'renewable' IS DISTINCT FROM 'false'));

CREATE FUNCTION guard_execution_hard_limit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r execution_requests%ROWTYPE;
    session connection_sessions%ROWTYPE;
    credential_until BIGINT;
BEGIN
    SELECT * INTO STRICT r FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    IF (NEW.hard_deadline_at_ms IS NOT NULL) IS DISTINCT FROM (r.binding ? 'execution_lease') THEN
        RAISE EXCEPTION 'execution hard limit policy mismatch';
    END IF;
    IF NEW.hard_deadline_at_ms IS NOT NULL THEN
        SELECT * INTO STRICT session FROM connection_sessions WHERE organization=r.organization AND session_id=r.session_id;
        SELECT floor(extract(epoch from expires_at)*1000)::bigint INTO STRICT credential_until
            FROM service_credentials WHERE organization=r.organization AND credential_id=session.credential_id;
        IF NEW.hard_deadline_at_ms>NEW.started_at_ms+(r.binding->'execution_lease'->>'max_budget_ms')::bigint
            OR NEW.hard_deadline_at_ms>NEW.started_at_ms+(r.input->'command'->>'timeout_seconds')::bigint*1000+30000
            OR NEW.hard_deadline_at_ms>NEW.started_at_ms+(SELECT start.max_runtime_seconds::bigint*1000 FROM candidate_writer_leases l JOIN runtime_start_requests start USING(organization,request_id) WHERE l.organization=r.organization AND l.lease_id=r.lease_id)
            OR NEW.hard_deadline_at_ms>credential_until
            OR (r.input->>'lifetime'='connection' AND NEW.hard_deadline_at_ms>session.expires_at_ms) THEN
            RAISE EXCEPTION 'execution hard limit exceeds admission';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_hard_limit BEFORE INSERT ON execution_dispatch_intents
    FOR EACH ROW EXECUTE FUNCTION guard_execution_hard_limit();

-- Authorization alone does not extend the database deadline or writer lease.
CREATE TABLE execution_renewal_grants (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence BETWEEN 1 AND 4096),
    challenge JSONB NOT NULL CHECK (jsonb_typeof(challenge)='object'),
    grant_body JSONB NOT NULL CHECK (jsonb_typeof(grant_body)='object'),
    node_command JSONB NOT NULL CHECK (jsonb_typeof(node_command)='object'),
    grant_digest TEXT NOT NULL CHECK (grant_digest ~ '^sha256:[0-9a-f]{64}$'),
    previous_grant_digest TEXT NOT NULL CHECK (previous_grant_digest ~ '^sha256:[0-9a-f]{64}$'),
    renewal_digest TEXT NOT NULL CHECK (renewal_digest ~ '^sha256:[0-9a-f]{64}$'),
    granted_at_ms BIGINT NOT NULL CHECK (granted_at_ms>0),
    previous_deadline_at_ms BIGINT NOT NULL CHECK (previous_deadline_at_ms>granted_at_ms),
    deadline_at_ms BIGINT NOT NULL CHECK (deadline_at_ms>previous_deadline_at_ms AND deadline_at_ms<=granted_at_ms+30000),
    PRIMARY KEY (organization,execution_id,sequence),
    UNIQUE (organization,execution_id,grant_digest),
    FOREIGN KEY (organization,execution_id) REFERENCES execution_startup_grants
);
CREATE TABLE execution_renewal_acks (
    organization TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    evidence JSONB NOT NULL CHECK (jsonb_typeof(evidence)='array' AND jsonb_array_length(evidence)=2),
    evidence_digest TEXT NOT NULL CHECK (evidence_digest ~ '^sha256:[0-9a-f]{64}$'),
    acknowledged_at_ms BIGINT NOT NULL CHECK (acknowledged_at_ms>0),
    PRIMARY KEY (organization,execution_id,sequence),
    FOREIGN KEY (organization,execution_id,sequence) REFERENCES execution_renewal_grants
);
CREATE TRIGGER immutable_execution_renewal_grant BEFORE UPDATE OR DELETE ON execution_renewal_grants
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();
CREATE TRIGGER immutable_execution_renewal_ack BEFORE UPDATE OR DELETE ON execution_renewal_acks
    FOR EACH ROW EXECUTE FUNCTION reject_declaration_version_mutation();

CREATE FUNCTION execution_effective_deadline(org TEXT, execution TEXT) RETURNS bigint LANGUAGE sql AS $$
    SELECT COALESCE((SELECT g.deadline_at_ms FROM execution_renewal_grants g
            JOIN execution_renewal_acks a USING(organization,execution_id,sequence)
            WHERE g.organization=$1 AND g.execution_id=$2 ORDER BY g.sequence DESC LIMIT 1),
        CASE WHEN d.hard_deadline_at_ms IS NULL THEN d.deadline_at_ms
            ELSE LEAST(d.deadline_at_ms,w.expires_at_ms) END)
    FROM execution_dispatch_intents d LEFT JOIN execution_watchdog_arms w USING(organization,execution_id)
    WHERE d.organization=$1 AND d.execution_id=$2;
$$;

CREATE FUNCTION guard_execution_renewal_grant() RETURNS trigger LANGUAGE plpgsql AS $$
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
        OR s.grant_body->>'version' IS DISTINCT FROM '2'
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
CREATE TRIGGER check_execution_renewal_grant BEFORE INSERT ON execution_renewal_grants
    FOR EACH ROW EXECUTE FUNCTION guard_execution_renewal_grant();

CREATE FUNCTION guard_execution_renewal_ack() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r execution_requests%ROWTYPE;
    g execution_renewal_grants%ROWTYPE;
    w execution_watchdog_arms%ROWTYPE;
    l candidate_writer_leases%ROWTYPE;
    prior JSONB;
    old_deadline BIGINT;
    receipt JSONB;
    ordinal INTEGER;
BEGIN
    SELECT * INTO STRICT r FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT g FROM execution_renewal_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND sequence=NEW.sequence;
    SELECT * INTO STRICT w FROM execution_watchdog_arms WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT l FROM candidate_writer_leases WHERE organization=r.organization AND lease_id=r.lease_id FOR UPDATE;
    IF r.state<>'Dispatching' OR l.state<>'Held' OR l.epoch<>r.epoch
        OR EXISTS(SELECT 1 FROM execution_output_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR EXISTS(SELECT 1 FROM execution_completions WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR l.expires_at_ms<=floor(extract(epoch from clock_timestamp())*1000)
        OR g.previous_deadline_at_ms<=floor(extract(epoch from clock_timestamp())*1000)
        OR NEW.acknowledged_at_ms<g.granted_at_ms OR NEW.acknowledged_at_ms>=g.previous_deadline_at_ms
        OR g.previous_deadline_at_ms IS DISTINCT FROM execution_effective_deadline(NEW.organization,NEW.execution_id) THEN
        RAISE EXCEPTION 'execution renewal acknowledgment expired';
    END IF;
    old_deadline := (w.evidence#>>'{armed,request,deadline_boottime_ms}')::bigint;
    IF NEW.sequence>1 THEN
        SELECT evidence INTO STRICT prior FROM execution_renewal_acks WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND sequence=NEW.sequence-1;
        old_deadline := (prior#>>'{0,command,deadline_boottime_ms}')::bigint;
    END IF;
    FOR ordinal IN 0..1 LOOP
        receipt := NEW.evidence->ordinal;
        IF receipt->>'version' IS DISTINCT FROM '1' OR receipt->>'event' IS DISTINCT FROM 'renewed'
            OR receipt->'command' IS DISTINCT FROM g.node_command
            OR receipt->'journal' IS DISTINCT FROM w.evidence->(CASE WHEN ordinal=0 THEN 'armed' ELSE 'backup_armed' END)->'journal'
            OR (receipt->>'previous_deadline_boottime_ms')::bigint IS DISTINCT FROM old_deadline
            OR COALESCE((receipt->>'accepted_boottime_ms')::bigint,0)<(w.evidence->>'observed_boottime_ms')::bigint
            OR (receipt->>'accepted_boottime_ms')::bigint>=old_deadline
            OR COALESCE((g.node_command->>'deadline_boottime_ms')::bigint,0)<=old_deadline
            OR (g.node_command->>'deadline_boottime_ms')::bigint>(receipt->>'accepted_boottime_ms')::bigint+30000
            OR (g.node_command->>'deadline_boottime_ms')::bigint>(w.evidence#>>'{armed,request,renewal,hard_deadline_boottime_ms}')::bigint THEN
            RAISE EXCEPTION 'execution renewal acknowledgment binding mismatch';
        END IF;
    END LOOP;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_renewal_ack BEFORE INSERT ON execution_renewal_acks
    FOR EACH ROW EXECUTE FUNCTION guard_execution_renewal_ack();

CREATE FUNCTION complete_execution_renewal() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE g execution_renewal_grants%ROWTYPE;
BEGIN
    IF TG_TABLE_NAME='execution_renewal_grants' THEN g := NEW;
    ELSE
        SELECT * INTO STRICT g FROM execution_renewal_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id AND sequence=NEW.sequence;
        IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN candidate_writer_leases l USING(organization,lease_id,epoch)
            WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state='Dispatching'
                AND l.state='Held' AND l.expires_at_ms>=g.deadline_at_ms) THEN
            RAISE EXCEPTION 'execution renewal writer update incomplete';
        END IF;
    END IF;
    IF floor(extract(epoch from clock_timestamp())*1000)>=g.previous_deadline_at_ms THEN
        RAISE EXCEPTION 'execution renewal original deadline elapsed';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER complete_execution_renewal_grant AFTER INSERT ON execution_renewal_grants
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION complete_execution_renewal();
CREATE CONSTRAINT TRIGGER complete_execution_renewal_ack AFTER INSERT ON execution_renewal_acks
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION complete_execution_renewal();

CREATE OR REPLACE FUNCTION guard_execution_startup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN execution_dispatch_intents i USING(organization,execution_id)
        JOIN candidate_writer_leases l ON l.organization=r.organization AND l.lease_id=r.lease_id AND l.epoch=r.epoch
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state='Dispatching' AND l.state='Held'
        AND l.expires_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND i.deadline_at_ms>floor(extract(epoch from clock_timestamp())*1000)
        AND NEW.granted_at_ms>=i.started_at_ms AND NEW.granted_at_ms<i.deadline_at_ms
        AND NEW.challenge->>'execution_id'=r.execution_id AND (NEW.challenge->>'generation')::bigint=(r.binding->>'generation')::bigint
        AND ((i.hard_deadline_at_ms IS NULL AND NEW.challenge->>'version'='1' AND NEW.grant_body->>'version'='1'
                AND NOT (NEW.grant_body ? 'hard_budget_ms'))
            OR (i.hard_deadline_at_ms IS NOT NULL AND NEW.challenge->>'version'='2' AND NEW.grant_body->>'version'='2'
                AND (NEW.grant_body->>'hard_budget_ms')::bigint BETWEEN (NEW.grant_body->>'lease_budget_ms')::bigint AND i.hard_deadline_at_ms-NEW.granted_at_ms
                AND EXISTS(SELECT 1 FROM execution_watchdog_arms w WHERE w.organization=NEW.organization AND w.execution_id=NEW.execution_id
                    AND w.evidence#>>'{armed,request,renewal,authority_digest}'=i.intent_digest)))
        AND (NEW.grant_body->>'lease_budget_ms')::bigint BETWEEN 1 AND i.deadline_at_ms-NEW.granted_at_ms) THEN
        RAISE EXCEPTION 'execution startup is not admitted';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_execution_completion() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r execution_requests%ROWTYPE;
    d execution_dispatch_intents%ROWTYPE;
    w execution_watchdog_arms%ROWTYPE;
    l candidate_writer_leases%ROWTYPE;
    output execution_output_intents%ROWTYPE;
    binding JSONB;
BEGIN
    SELECT * INTO STRICT r FROM execution_requests WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT d FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT w FROM execution_watchdog_arms WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    SELECT * INTO STRICT l FROM candidate_writer_leases WHERE organization=NEW.organization AND lease_id=NEW.lease_id FOR UPDATE;
    SELECT (manifest#>>'{metadata,annotations,agent-computer.io/binding}')::jsonb INTO STRICT binding
        FROM execution_pod_plans WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    IF r.lease_id<>NEW.lease_id OR r.epoch<>NEW.epoch OR d.lease_id<>NEW.lease_id OR d.epoch<>NEW.epoch
        OR l.epoch<>NEW.epoch OR l.state NOT IN ('Held','Draining') OR r.state NOT IN ('Dispatching','CancelRequested','Unknown')
        OR NEW.dispatch_digest<>d.intent_digest OR NEW.arm_digest<>w.evidence_digest
        OR NEW.seal->>'version' IS DISTINCT FROM '1' OR NEW.seal->'arm' IS DISTINCT FROM w.evidence
        OR COALESCE(NEW.seal->>'domain','') NOT IN ('empty','removed')
        OR NEW.seal->'io'->>'version' IS DISTINCT FROM '1'
        OR COALESCE(NEW.seal->'io'->>'instance','') !~ '^[0-9a-f]{64}$'
        OR NEW.seal->'io'->>'instance' IS DISTINCT FROM w.evidence->'runtime'->'workspace_mount'->>'instance'
        OR NEW.seal->'io'->'prepared' IS DISTINCT FROM r.binding->'prepared'
        OR NEW.seal->'io'->'prepared' IS DISTINCT FROM w.evidence->'runtime'->'workspace_mount'->'prepared'
        OR binding#>'{workspace,fence,mount}' IS NULL
        OR binding#>'{workspace,fence,mount}' IS DISTINCT FROM w.evidence#>'{runtime,workspace_mount}'
        OR binding#>'{workspace,fence,node}' IS DISTINCT FROM w.evidence#>'{runtime,identity,node}'
        OR COALESCE((NEW.seal->'io'->>'accepted_mutating_requests')::numeric,-1)<0
        OR COALESCE((NEW.seal->>'observed_boottime_ms')::numeric,-1)<(w.evidence->>'observed_boottime_ms')::numeric THEN
        RAISE EXCEPTION 'execution seal binding does not match';
    END IF;
    IF NEW.output_manifest_digest IS NOT NULL THEN
        SELECT i.* INTO STRICT output FROM execution_output_intents i JOIN execution_outputs o USING(organization,execution_id)
            WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id
            AND i.manifest_digest=NEW.output_manifest_digest AND o.manifest_digest=i.manifest_digest;
        IF output.dispatch_digest<>NEW.dispatch_digest OR output.arm_digest<>NEW.arm_digest THEN
            RAISE EXCEPTION 'completion output binding does not match';
        END IF;
    END IF;
    IF NEW.accepted_state IN ('Succeeded','Failed') THEN
        IF r.state<>'Dispatching' OR l.state<>'Held' OR NEW.output_manifest_digest IS NULL
            OR NEW.seal->'renewals' IS DISTINCT FROM (SELECT evidence FROM execution_renewal_acks WHERE organization=NEW.organization AND execution_id=NEW.execution_id ORDER BY sequence DESC LIMIT 1)
            OR NEW.completed_at_ms>=execution_effective_deadline(NEW.organization,NEW.execution_id) OR floor(extract(epoch from clock_timestamp())*1000)>=execution_effective_deadline(NEW.organization,NEW.execution_id)
            OR (NEW.accepted_state='Succeeded' AND output.manifest->'summary'->>'observed_outcome' IS DISTINCT FROM 'succeeded')
            OR (NEW.accepted_state='Failed' AND COALESCE(output.manifest->'summary'->>'observed_outcome','') NOT IN ('failed','spawn_failed','timed_out','descendants_terminated')) THEN
            RAISE EXCEPTION 'terminal outcome is unconfirmed';
        END IF;
    ELSIF NEW.accepted_state='Cancelled' AND r.state<>'CancelRequested' THEN
        RAISE EXCEPTION 'dispatched cancellation was not requested';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION complete_execution_completion() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS(SELECT 1 FROM execution_requests r JOIN candidate_writer_leases l USING(organization,lease_id,epoch)
        JOIN candidate_writer_drains d USING(organization,lease_id,epoch)
        WHERE r.organization=NEW.organization AND r.execution_id=NEW.execution_id AND r.state=NEW.accepted_state
        AND l.state='Released' AND d.proof='execution_drained') THEN
        RAISE EXCEPTION 'execution completion transaction is incomplete';
    END IF;
    IF NEW.accepted_state IN ('Succeeded','Failed') AND EXISTS(SELECT 1 FROM execution_dispatch_intents i
        WHERE i.organization=NEW.organization AND i.execution_id=NEW.execution_id
        AND floor(extract(epoch from clock_timestamp())*1000)>=execution_effective_deadline(NEW.organization,NEW.execution_id)) THEN
        RAISE EXCEPTION 'execution completion deadline elapsed';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION execution_renewal_progress(org TEXT, execution TEXT) RETURNS jsonb LANGUAGE sql AS $$
    SELECT CASE WHEN s.grant_body->>'version'='2' THEN COALESCE(
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
    IF NOT EXISTS(SELECT 1 FROM execution_dispatch_intents d JOIN execution_startup_grants g USING(organization,execution_id)
        JOIN execution_watchdog_arms w USING(organization,execution_id)
        WHERE d.organization=NEW.organization AND d.execution_id=NEW.execution_id AND d.intent_digest=NEW.dispatch_digest
        AND g.pod_uid=NEW.pod_uid AND w.pod_uid=NEW.pod_uid AND g.grant_digest=NEW.grant_digest AND w.evidence_digest=NEW.arm_digest)
        OR NEW.manifest->>'version' IS DISTINCT FROM (SELECT grant_body->>'version' FROM execution_startup_grants WHERE organization=NEW.organization AND execution_id=NEW.execution_id)
        OR NEW.manifest->'renewal' IS DISTINCT FROM execution_renewal_progress(NEW.organization,NEW.execution_id)
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

CREATE FUNCTION guard_execution_watchdog_lease_policy() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE d execution_dispatch_intents%ROWTYPE;
    request JSONB := NEW.evidence#>'{armed,request}';
BEGIN
    SELECT * INTO STRICT d FROM execution_dispatch_intents WHERE organization=NEW.organization AND execution_id=NEW.execution_id;
    IF d.hard_deadline_at_ms IS NULL THEN
        IF request->>'version' IS DISTINCT FROM '1' OR request ? 'renewal' THEN
            RAISE EXCEPTION 'legacy execution watchdog cannot renew';
        END IF;
    ELSIF request->>'version' IS DISTINCT FROM '2'
        OR request#>>'{renewal,authority_digest}' IS DISTINCT FROM d.intent_digest
        OR (request#>>'{renewal,hard_deadline_boottime_ms}')::bigint-(request->>'deadline_boottime_ms')::bigint IS DISTINCT FROM d.hard_deadline_at_ms-d.deadline_at_ms THEN
        RAISE EXCEPTION 'execution watchdog renewal ceiling mismatch';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER check_execution_watchdog_lease_policy BEFORE INSERT ON execution_watchdog_arms
    FOR EACH ROW EXECUTE FUNCTION guard_execution_watchdog_lease_policy();
