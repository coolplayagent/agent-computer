-- Historical execution records are retained. Poll only bounded queued admissions;
-- storage identity and current authority remain checked inside the claim transaction.
CREATE INDEX execution_queue_poll ON execution_requests
    (organization,created_at_ms,execution_id) WHERE state='Queued';
