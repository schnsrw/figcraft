-- S2-014: a document's accepted sequence and sequencer fencing token never move backwards.
-- `document_events` already enforces unique `(tenant_id, document_id, seq)`; this guards the
-- `documents` counters the append transaction advances, whatever code path updates them.

CREATE FUNCTION documents_counters_monotonic() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.current_seq < OLD.current_seq THEN
        RAISE EXCEPTION 'document sequence cannot decrease' USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.fencing_token < OLD.fencing_token THEN
        RAISE EXCEPTION 'document fencing token cannot decrease' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER documents_counters_monotonic
    BEFORE UPDATE OF current_seq, fencing_token ON documents
    FOR EACH ROW EXECUTE FUNCTION documents_counters_monotonic();
