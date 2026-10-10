-- Lifetime is part of the immutable, canonically hashed execution input.
ALTER TABLE execution_requests ADD CONSTRAINT execution_lifetime
    CHECK (COALESCE(input->>'lifetime','') IN ('connection','background'));

-- This predicate identifies the reservation only. Callers must still verify
-- the original identity, current grants, generation and fixed deadlines.
-- Terminal history is included because completion rechecks authority after
-- recording the outcome and draining the writer in the same transaction.
CREATE FUNCTION writer_has_background_execution(org TEXT, lease TEXT, epoch BIGINT)
RETURNS boolean LANGUAGE sql AS $$
    SELECT EXISTS (SELECT 1 FROM execution_requests e
        WHERE e.organization=$1 AND e.lease_id=$2 AND e.epoch=$3
            AND e.input->>'lifetime'='background');
$$;
