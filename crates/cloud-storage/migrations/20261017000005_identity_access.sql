-- S4 foundation: browser sessions, OIDC login attempts, service accounts, team invitations,
-- sensitive permissions, support-access grants and single-tenant bootstrap. Secrets (session
-- tokens, credentials, invitation and bootstrap tokens) are stored only as SHA-256 hashes.

-- S4-001: one row per started OIDC authorization-code flow. The state is looked up by hash; the
-- PKCE verifier and nonce live only until the callback consumes the row or it expires.
CREATE TABLE oidc_login_attempts (
    state_hash BYTEA PRIMARY KEY CHECK (octet_length(state_hash) = 32),
    issuer TEXT NOT NULL CHECK (char_length(issuer) BETWEEN 1 AND 2048),
    nonce TEXT NOT NULL CHECK (char_length(nonce) BETWEEN 16 AND 256),
    pkce_verifier TEXT NOT NULL CHECK (char_length(pkce_verifier) BETWEEN 43 AND 128),
    return_to TEXT CHECK (char_length(return_to) BETWEEN 1 AND 2048),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    CHECK (expires_at > created_at)
);
CREATE INDEX oidc_login_attempts_expiry_idx ON oidc_login_attempts (expires_at);

-- S4-002: browser sessions. Rotation links the new row to the one it replaced.
CREATE TABLE browser_sessions (
    id UUID PRIMARY KEY,
    principal_id UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    rotated_from UUID REFERENCES browser_sessions (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    idle_expires_at TIMESTAMPTZ NOT NULL,
    absolute_expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    revoke_reason TEXT CHECK (revoke_reason IN ('logout', 'rotated', 'membership_revoked', 'principal_suspended', 'operator', 'expired')),
    CHECK (idle_expires_at > created_at AND absolute_expires_at > created_at),
    CHECK ((revoked_at IS NULL) = (revoke_reason IS NULL))
);
CREATE INDEX browser_sessions_principal_idx ON browser_sessions (principal_id, created_at DESC);

-- S4-003: the single-tenant bootstrap administrator. Until `completed_at` is set the deployment
-- only serves the bootstrap flow.
CREATE TABLE tenant_bootstrap (
    tenant_id UUID PRIMARY KEY REFERENCES tenants (id) ON DELETE RESTRICT,
    token_hash BYTEA NOT NULL CHECK (octet_length(token_hash) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    completed_by UUID REFERENCES principals (id) ON DELETE RESTRICT,
    CHECK (expires_at > created_at),
    CHECK ((completed_at IS NULL) = (completed_by IS NULL))
);

-- S4-004: service accounts are `service` principals scoped to one tenant.
CREATE TABLE service_accounts (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    principal_id UUID NOT NULL UNIQUE REFERENCES principals (id) ON DELETE RESTRICT,
    name TEXT NOT NULL CHECK (char_length(btrim(name)) BETWEEN 1 AND 255),
    scopes TEXT[] NOT NULL CHECK (cardinality(scopes) BETWEEN 1 AND 64),
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'disabled', 'deleted')),
    created_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id)
);

CREATE TABLE service_account_credentials (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    service_account_id UUID NOT NULL,
    secret_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(secret_hash) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    FOREIGN KEY (tenant_id, service_account_id) REFERENCES service_accounts (tenant_id, id) ON DELETE RESTRICT,
    CHECK (expires_at > created_at)
);
CREATE INDEX service_account_credentials_account_idx ON service_account_credentials (tenant_id, service_account_id);

-- S4-009: invitations into a team (and through it, the tenant).
CREATE TABLE team_invitations (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    team_id UUID NOT NULL,
    email TEXT NOT NULL CHECK (char_length(email) BETWEEN 3 AND 320 AND position('@' IN email) > 1),
    role TEXT NOT NULL CHECK (role IN ('team_manager', 'editor', 'commenter', 'viewer')),
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    invited_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ,
    accepted_by UUID REFERENCES principals (id) ON DELETE RESTRICT,
    revoked_at TIMESTAMPTZ,
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id, team_id) REFERENCES teams (tenant_id, id) ON DELETE RESTRICT,
    CHECK (expires_at > created_at),
    CHECK ((accepted_at IS NULL) = (accepted_by IS NULL)),
    CHECK (accepted_at IS NULL OR revoked_at IS NULL)
);
CREATE INDEX team_invitations_team_idx ON team_invitations (tenant_id, team_id, created_at DESC);

-- S4-011: sensitive permissions are explicit grants, never implied by a role.
CREATE TABLE member_permissions (
    tenant_id UUID NOT NULL,
    principal_id UUID NOT NULL,
    permission TEXT NOT NULL CHECK (permission IN ('billing', 'provider_keys', 'support', 'export', 'deletion')),
    granted_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, principal_id, permission),
    FOREIGN KEY (tenant_id, principal_id) REFERENCES tenant_members (tenant_id, principal_id) ON DELETE RESTRICT
);

-- S4-018: time-boxed, audited support access to one tenant.
CREATE TABLE support_access_grants (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    granted_to UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    granted_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    reason TEXT NOT NULL CHECK (char_length(btrim(reason)) BETWEEN 1 AND 500),
    scope TEXT NOT NULL DEFAULT 'read' CHECK (scope IN ('read', 'write')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    CHECK (expires_at > created_at AND expires_at <= created_at + interval '7 days')
);
CREATE INDEX support_access_grants_tenant_idx ON support_access_grants (tenant_id, expires_at);
