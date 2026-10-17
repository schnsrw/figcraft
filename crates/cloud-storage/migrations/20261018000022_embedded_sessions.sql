-- S6-010: an embedded session is a browser session that is bound to the host, document and role
-- an embed-token exchange granted. The binding lives on the session row so every existing check
-- (sliding expiry, rotation, "sign out everywhere", the access guard, the relay's revocation
-- sweep) keeps working unchanged, and so the host can end the sessions it opened.
ALTER TABLE browser_sessions
    ADD COLUMN embed_tenant_id UUID,
    ADD COLUMN embed_system_id TEXT,
    ADD COLUMN embed_document_id UUID,
    ADD COLUMN embed_role TEXT CHECK (embed_role IN ('viewer', 'commenter', 'editor')),
    -- Either the whole binding is present or none of it is.
    ADD CONSTRAINT browser_sessions_embed_binding CHECK (
        (embed_tenant_id IS NULL AND embed_system_id IS NULL AND embed_document_id IS NULL AND embed_role IS NULL)
        OR (embed_tenant_id IS NOT NULL AND embed_system_id IS NOT NULL AND embed_document_id IS NOT NULL AND embed_role IS NOT NULL)
    ),
    DROP CONSTRAINT browser_sessions_revoke_reason_check,
    ADD CONSTRAINT browser_sessions_revoke_reason_check CHECK (
        revoke_reason IN ('logout', 'rotated', 'membership_revoked', 'principal_suspended', 'operator', 'expired', 'host_revoked')
    );
CREATE INDEX browser_sessions_embed_host_idx ON browser_sessions (embed_tenant_id, embed_system_id)
    WHERE embed_system_id IS NOT NULL AND revoked_at IS NULL;
CREATE INDEX browser_sessions_embed_document_idx ON browser_sessions (embed_tenant_id, embed_document_id)
    WHERE embed_document_id IS NOT NULL AND revoked_at IS NULL;
