-- S3-002: immutable object metadata, object references and upload reservations.
-- Object bytes live in S3-compatible storage; these rows record where they are, what they must
-- hash to and who still needs them. Keys are confined to the owning tenant's prefix by constraint,
-- so a row can never point at another tenant's objects whatever code path writes it.

CREATE TABLE objects (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (kind IN ('snapshot', 'asset', 'library', 'plugin', 'export', 'thumbnail')),
    bucket TEXT NOT NULL CHECK (char_length(bucket) BETWEEN 3 AND 63),
    object_key TEXT NOT NULL CHECK (
        char_length(object_key) BETWEEN 1 AND 1024
        AND starts_with(object_key, 'tenants/' || tenant_id::text || '/')
        AND object_key !~ '(^|/)\.\.?(/|$)'
        AND object_key !~ '//'
    ),
    sha256 BYTEA NOT NULL CHECK (octet_length(sha256) = 32),
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    media_type TEXT NOT NULL CHECK (char_length(media_type) BETWEEN 1 AND 255),
    producer_version TEXT NOT NULL CHECK (char_length(producer_version) BETWEEN 1 AND 64),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'verified', 'rejected', 'deleting', 'deleted')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    verified_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ,
    UNIQUE (tenant_id, id),
    CHECK (state <> 'verified' OR verified_at IS NOT NULL),
    CHECK ((state IN ('deleting', 'deleted')) = (deleted_at IS NOT NULL))
);
-- One live row per stored key; a deleted row's key may be reused by a later write.
CREATE UNIQUE INDEX objects_live_key_idx ON objects (bucket, object_key) WHERE state <> 'deleted';
CREATE INDEX objects_tenant_digest_idx ON objects (tenant_id, kind, sha256);
CREATE INDEX objects_state_created_idx ON objects (state, created_at);

CREATE TABLE object_references (
    tenant_id UUID NOT NULL,
    object_id UUID NOT NULL,
    owner_kind TEXT NOT NULL CHECK (owner_kind IN (
        'document', 'document_snapshot', 'asset', 'library_version', 'plugin_version', 'export', 'thumbnail', 'retention_hold'
    )),
    owner_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id, owner_kind, owner_id),
    FOREIGN KEY (tenant_id, object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT
);
CREATE INDEX object_references_owner_idx ON object_references (tenant_id, owner_kind, owner_id);

CREATE TABLE upload_reservations (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    principal_id UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (kind IN ('snapshot', 'asset', 'library', 'plugin', 'export', 'thumbnail')),
    method TEXT NOT NULL DEFAULT 'PUT' CHECK (method = 'PUT'),
    staging_key TEXT NOT NULL,
    expected_sha256 BYTEA NOT NULL CHECK (octet_length(expected_sha256) = 32),
    expected_size BIGINT NOT NULL CHECK (expected_size > 0),
    media_type TEXT NOT NULL CHECK (char_length(media_type) BETWEEN 1 AND 255),
    state TEXT NOT NULL DEFAULT 'reserved' CHECK (state IN ('reserved', 'completed', 'promoted', 'rejected', 'expired')),
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    object_id UUID,
    UNIQUE (tenant_id, id),
    UNIQUE (staging_key),
    FOREIGN KEY (tenant_id, object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT,
    CHECK (staging_key = 'tenants/' || tenant_id::text || '/staging/' || id::text),
    CHECK (expires_at > created_at),
    CHECK ((state = 'promoted') = (object_id IS NOT NULL)),
    -- A rejection may happen before or after completion, so only it leaves `completed_at` free.
    CHECK (state = 'rejected' OR ((state IN ('completed', 'promoted')) = (completed_at IS NOT NULL)))
);
CREATE INDEX upload_reservations_expiry_idx ON upload_reservations (state, expires_at);
CREATE INDEX upload_reservations_tenant_created_idx ON upload_reservations (tenant_id, created_at DESC, id);
