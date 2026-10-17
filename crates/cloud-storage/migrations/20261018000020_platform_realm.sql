-- S6-044: the platform realm. Platform operators are not workspace users: they have their own
-- login records, sessions, bootstrap token and audit log, in tables that no workspace transaction
-- can read or write.
--
-- Isolation. These tables carry no tenant_id. Each has ENABLE and FORCE ROW LEVEL SECURITY with one
-- policy that admits a row only while the transaction-local `figcraft.platform_scope` is 'on'. Only
-- `begin_platform_transaction` (crates/cloud-storage/src/platform.rs) sets it, and it never sets
-- `figcraft.tenant_id`; `begin_tenant_transaction` never sets the platform scope. So a workspace
-- transaction sees zero platform rows, and a platform transaction sees zero workspace rows
-- (tenants and everything below it stay under the tenant policy). The platform learns about
-- workspaces only through the two SECURITY DEFINER functions below, which return metadata columns
-- of `tenants` and a member count: never documents, names of people, files or audit content.
-- This is one database role and one code path guarded by RLS, not a second database role; see
-- docs/saas/security.md ("Platform realm") for what that does and does not defend against.

CREATE TABLE platform_admins (
    id UUID PRIMARY KEY,
    login TEXT NOT NULL UNIQUE CHECK (login = lower(btrim(login)) AND char_length(login) BETWEEN 3 AND 254),
    display_name TEXT NOT NULL CHECK (char_length(btrim(display_name)) BETWEEN 1 AND 255),
    -- Argon2id PHC string (S6-033 module). NULL until a password is set; such an admin cannot sign in.
    password_hash TEXT CHECK (password_hash IS NULL OR char_length(password_hash) BETWEEN 20 AND 512),
    -- Sealed TOTP secret (secret_box) and the time the first code was verified.
    totp_secret BYTEA CHECK (totp_secret IS NULL OR octet_length(totp_secret) BETWEEN 1 AND 512),
    totp_confirmed_at TIMESTAMPTZ,
    -- The last accepted 30-second step, so a code cannot be used twice.
    totp_last_step BIGINT NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'disabled')),
    failed_logins INTEGER NOT NULL DEFAULT 0 CHECK (failed_logins >= 0),
    locked_until TIMESTAMPTZ,
    created_by UUID REFERENCES platform_admins (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at TIMESTAMPTZ
);

CREATE TABLE platform_sessions (
    id UUID PRIMARY KEY,
    admin_id UUID NOT NULL REFERENCES platform_admins (id) ON DELETE CASCADE,
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    -- A session that has passed the password step but not the second factor cannot call the API.
    second_factor_done BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    idle_expires_at TIMESTAMPTZ NOT NULL,
    absolute_expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    revoke_reason TEXT CHECK (revoke_reason IS NULL OR revoke_reason IN ('logout', 'admin_disabled', 'password_changed', 'operator', 'expired'))
);
CREATE INDEX platform_sessions_admin_idx ON platform_sessions (admin_id) WHERE revoked_at IS NULL;

-- The one-time token that creates the first platform admin (printed by the CLI).
CREATE TABLE platform_bootstrap (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    token_hash BYTEA NOT NULL CHECK (octet_length(token_hash) = 32),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);

-- Append-only. tenant_id is a plain column (no foreign key) so a platform record outlives any
-- workspace data and never restricts its deletion.
CREATE TABLE platform_audit_events (
    id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('admin', 'credential', 'system', 'anonymous')),
    actor_id UUID,
    action TEXT NOT NULL CHECK (char_length(action) BETWEEN 1 AND 128),
    resource_kind TEXT NOT NULL CHECK (char_length(resource_kind) BETWEEN 1 AND 64),
    resource_id UUID,
    tenant_id UUID,
    outcome TEXT NOT NULL CHECK (outcome IN ('succeeded', 'denied', 'failed')),
    request_id TEXT CHECK (request_id IS NULL OR char_length(request_id) <= 64),
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object' AND pg_column_size(metadata) <= 8192)
);
CREATE INDEX platform_audit_created_idx ON platform_audit_events (created_at DESC, id);
CREATE INDEX platform_audit_tenant_idx ON platform_audit_events (tenant_id, created_at DESC) WHERE tenant_id IS NOT NULL;

DO $platform$
DECLARE
    target TEXT;
BEGIN
    FOREACH target IN ARRAY ARRAY['platform_admins', 'platform_sessions', 'platform_bootstrap', 'platform_audit_events']
    LOOP
        EXECUTE format('ALTER TABLE public.%I ENABLE ROW LEVEL SECURITY', target);
        EXECUTE format('ALTER TABLE public.%I FORCE ROW LEVEL SECURITY', target);
        EXECUTE format(
            'CREATE POLICY platform_scope ON public.%I AS PERMISSIVE FOR ALL '
            'USING (current_setting(''figcraft.platform_scope'', true) = ''on'') '
            'WITH CHECK (current_setting(''figcraft.platform_scope'', true) = ''on'')',
            target
        );
    END LOOP;
END
$platform$;

-- Workspace metadata for the platform: one page of workspaces, or one workspace. Metadata columns
-- of `tenants` plus the number of active members; nothing from any table below a tenant. The
-- caller must be in a platform transaction (the function refuses otherwise) and the function reads
-- through the same `definer_lookup` policy the pre-tenant lookups use (S4-014).
CREATE FUNCTION public.figcraft_platform_workspaces(p_after UUID, p_limit INTEGER, p_state TEXT, p_slug_prefix TEXT)
RETURNS TABLE (id TEXT, slug TEXT, name TEXT, state TEXT, plan_id TEXT, home_region TEXT, created_at BIGINT, updated_at BIGINT, active_members BIGINT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    IF current_setting('figcraft.platform_scope', true) IS DISTINCT FROM 'on' THEN
        RAISE EXCEPTION 'platform scope required' USING ERRCODE = '42501';
    END IF;
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 101 THEN
        RAISE EXCEPTION 'invalid page size' USING ERRCODE = '22023';
    END IF;
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT t.id::text, t.slug, t.name, t.state, t.plan_id, t.home_region,
               floor(extract(epoch FROM t.created_at))::bigint, floor(extract(epoch FROM t.updated_at))::bigint,
               (SELECT count(*) FROM public.tenant_members m WHERE m.tenant_id = t.id AND m.state = 'active')
        FROM public.tenants t
        WHERE (p_after IS NULL OR t.id > p_after)
          AND (p_state IS NULL OR t.state = p_state)
          AND (p_slug_prefix IS NULL OR left(t.slug, char_length(p_slug_prefix)) = p_slug_prefix)
        ORDER BY t.id
        LIMIT p_limit;
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;

CREATE FUNCTION public.figcraft_platform_workspace(p_id UUID)
RETURNS TABLE (id TEXT, slug TEXT, name TEXT, state TEXT, plan_id TEXT, home_region TEXT, created_at BIGINT, updated_at BIGINT, active_members BIGINT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    IF current_setting('figcraft.platform_scope', true) IS DISTINCT FROM 'on' THEN
        RAISE EXCEPTION 'platform scope required' USING ERRCODE = '42501';
    END IF;
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        SELECT t.id::text, t.slug, t.name, t.state, t.plan_id, t.home_region,
               floor(extract(epoch FROM t.created_at))::bigint, floor(extract(epoch FROM t.updated_at))::bigint,
               (SELECT count(*) FROM public.tenant_members m WHERE m.tenant_id = t.id AND m.state = 'active')
        FROM public.tenants t WHERE t.id = p_id;
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;

-- A workspace the platform creates for an owner who has no account yet (or does not yet use the
-- workspace): the workspace starts `pending` with one pending owner invitation, and accepting it
-- makes the acceptor the `tenant_owner` and the workspace `active`. Tenant-scoped like every table
-- below a tenant. The token is shown once to the operator (who delivers the link) and stored only
-- as its SHA-256.
CREATE TABLE workspace_owner_invitations (
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    id UUID NOT NULL,
    email TEXT NOT NULL CHECK (char_length(email) BETWEEN 3 AND 320),
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'accepted', 'revoked')),
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    accepted_by UUID REFERENCES principals (id) ON DELETE RESTRICT,
    accepted_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, id)
);
CREATE UNIQUE INDEX workspace_owner_invitations_pending_idx ON workspace_owner_invitations (tenant_id) WHERE state = 'pending';

ALTER TABLE public.workspace_owner_invitations ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.workspace_owner_invitations FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON public.workspace_owner_invitations AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
CREATE POLICY definer_lookup ON public.workspace_owner_invitations AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');

-- The workspace an owner-invitation token hash belongs to; acceptance then runs inside it.
CREATE FUNCTION public.figcraft_owner_invitation_tenant(p_token_hash BYTEA)
RETURNS TEXT
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    found_tenant TEXT;
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    SELECT i.tenant_id::text INTO found_tenant FROM public.workspace_owner_invitations i
    WHERE octet_length(p_token_hash) = 32 AND i.token_hash = p_token_hash
    LIMIT 1;
    PERFORM set_config('figcraft.definer_lookup', '', true);
    RETURN found_tenant;
END
$fn$;
REVOKE ALL ON FUNCTION public.figcraft_owner_invitation_tenant(BYTEA) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.figcraft_platform_workspaces(UUID, INTEGER, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION public.figcraft_platform_workspace(UUID) FROM PUBLIC;
