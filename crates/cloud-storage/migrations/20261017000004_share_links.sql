-- Core slice (pulls the storage half of S4-005 forward): document share links. Only a SHA-256 of
-- the link's secret is stored; the secret itself is shown once when the link is created.

CREATE TABLE share_links (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    token_hash BYTEA NOT NULL CHECK (octet_length(token_hash) = 32),
    role TEXT NOT NULL CHECK (role IN ('editor', 'viewer')),
    label TEXT CHECK (char_length(label) BETWEEN 1 AND 255),
    created_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    UNIQUE (tenant_id, id),
    UNIQUE (token_hash),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT,
    CHECK (expires_at IS NULL OR expires_at > created_at),
    CHECK (revoked_at IS NULL OR revoked_at >= created_at)
);
CREATE INDEX share_links_document_idx ON share_links (tenant_id, document_id, created_at DESC);
