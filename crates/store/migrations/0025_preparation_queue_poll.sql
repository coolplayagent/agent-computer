-- Discovery excludes historical stopped/sealed/prepared requests. Authority and
-- once-only dispatch remain in the existing preparation claim transaction.
CREATE INDEX candidate_preparation_queue_poll ON runtime_start_requests
    (organization,volume_id,request_id) WHERE state IN ('Queued','Preparing');
