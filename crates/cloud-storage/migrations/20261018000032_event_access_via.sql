-- S6-039: how access was held when a batch was committed. `actor_principal_id` already says WHO; these
-- two columns say THROUGH WHAT: directly as a member (workspace, document or tenant role), through a
-- team grant (`via_id` = the team), through a share link (`via_id` = the link) or as a service
-- account. They are written once with the batch (the log stays append-only: the runtime role has
-- INSERT and SELECT only) and are NULL on batches committed before this existed or whose source could
-- not be determined, which history shows as "via unknown".
ALTER TABLE document_events
    ADD COLUMN via_kind TEXT CHECK (via_kind IN ('member', 'team', 'share_link', 'service_account')),
    ADD COLUMN via_id UUID,
    ADD CONSTRAINT document_events_via_check CHECK (
        (via_kind IS NULL AND via_id IS NULL)
        OR (via_kind = 'member' AND via_id IS NULL)
        OR (via_kind IN ('team', 'share_link', 'service_account') AND via_id IS NOT NULL)
    );
