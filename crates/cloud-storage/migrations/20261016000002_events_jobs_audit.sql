-- S2-005: durable event, snapshot, job, idempotency and audit foundations.
-- Payloads and checksums are opaque to PostgreSQL; Rust owns their wire formats.

CREATE TABLE document_events (
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq > 0),
    fencing_token BIGINT NOT NULL CHECK (fencing_token > 0),
    payload BYTEA NOT NULL CHECK (octet_length(payload) BETWEEN 1 AND 16777216),
    checksum BYTEA NOT NULL CHECK (octet_length(checksum) = 32),
    actor_principal_id UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, document_id, seq),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT
);
CREATE INDEX document_events_actor_idx ON document_events (tenant_id, actor_principal_id, created_at DESC);

CREATE TABLE document_snapshots (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq >= 0),
    object_id UUID NOT NULL,
    checksum BYTEA NOT NULL CHECK (octet_length(checksum) = 32),
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'verified', 'rejected')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    verified_at TIMESTAMPTZ,
    UNIQUE (tenant_id, document_id, id),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT,
    CHECK ((state = 'verified') = (verified_at IS NOT NULL))
);
CREATE INDEX document_snapshots_document_seq_idx ON document_snapshots (tenant_id, document_id, seq DESC, created_at DESC);
CREATE INDEX document_snapshots_object_idx ON document_snapshots (tenant_id, object_id);

ALTER TABLE documents ADD COLUMN current_snapshot_id UUID;
ALTER TABLE documents ADD CONSTRAINT documents_current_snapshot_fk
    FOREIGN KEY (tenant_id, id, current_snapshot_id)
    REFERENCES document_snapshots (tenant_id, document_id, id) ON DELETE RESTRICT;

CREATE TABLE jobs (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (char_length(btrim(kind)) BETWEEN 1 AND 128),
    input JSONB NOT NULL CHECK (jsonb_typeof(input) = 'object'),
    state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    priority SMALLINT NOT NULL DEFAULT 0 CHECK (priority BETWEEN -10 AND 10),
    lease_owner TEXT CHECK (char_length(lease_owner) BETWEEN 1 AND 255),
    lease_expires_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts INTEGER NOT NULL DEFAULT 3 CHECK (max_attempts BETWEEN 1 AND 100),
    result JSONB,
    error_code TEXT CHECK (char_length(error_code) BETWEEN 1 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK (result IS NULL OR jsonb_typeof(result) = 'object')
);
CREATE INDEX jobs_claim_idx ON jobs (state, priority DESC, created_at, id)
    WHERE state IN ('queued', 'running');
CREATE INDEX jobs_tenant_created_idx ON jobs (tenant_id, created_at DESC, id);

CREATE TABLE idempotency_keys (
    tenant_id UUID NOT NULL,
    principal_id UUID NOT NULL,
    route TEXT NOT NULL CHECK (char_length(route) BETWEEN 1 AND 255),
    key_hash BYTEA NOT NULL CHECK (octet_length(key_hash) = 32),
    request_hash BYTEA NOT NULL CHECK (octet_length(request_hash) = 32),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'completed')),
    response_status SMALLINT CHECK (response_status BETWEEN 100 AND 599),
    response_body BYTEA CHECK (octet_length(response_body) <= 1048576),
    response_content_type TEXT CHECK (char_length(response_content_type) BETWEEN 1 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_id, principal_id, route, key_hash),
    FOREIGN KEY (tenant_id, principal_id) REFERENCES tenant_members (tenant_id, principal_id) ON DELETE RESTRICT,
    CHECK (expires_at > created_at),
    CHECK ((state = 'pending' AND response_status IS NULL AND response_body IS NULL AND response_content_type IS NULL)
        OR (state = 'completed' AND response_status IS NOT NULL AND response_body IS NOT NULL AND response_content_type IS NOT NULL))
);
CREATE INDEX idempotency_keys_expiry_idx ON idempotency_keys (expires_at);

CREATE TABLE audit_events (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    actor_principal_id UUID REFERENCES principals (id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (char_length(btrim(action)) BETWEEN 1 AND 128),
    resource_kind TEXT NOT NULL CHECK (char_length(btrim(resource_kind)) BETWEEN 1 AND 128),
    resource_id UUID,
    request_id UUID,
    source_ip INET,
    outcome TEXT NOT NULL CHECK (outcome IN ('succeeded', 'denied', 'failed')),
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_events_tenant_created_idx ON audit_events (tenant_id, created_at DESC, id);
CREATE INDEX audit_events_actor_created_idx ON audit_events (tenant_id, actor_principal_id, created_at DESC);
