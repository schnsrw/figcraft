-- S6-014: durable webhook deliveries with a retry schedule, terminal failure and manual replay.
--
-- One row per (endpoint, event). The body is the exact JSON that is signed and sent, so a retry or
-- a replay always carries the same bytes and the same event identifier. The UNIQUE key makes
-- publishing an event twice a no-op for each endpoint. Attempts are logged separately (status or
-- error category and timing, never a response body).
CREATE TABLE webhook_deliveries (
    tenant_id UUID NOT NULL,
    id UUID NOT NULL,
    endpoint_id UUID NOT NULL,
    event_id UUID NOT NULL,
    event_type TEXT NOT NULL CHECK (char_length(event_type) BETWEEN 1 AND 64),
    body BYTEA NOT NULL CHECK (octet_length(body) BETWEEN 2 AND 16384),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'succeeded', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 100),
    max_attempts INTEGER NOT NULL DEFAULT 8 CHECK (max_attempts BETWEEN 1 AND 20),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    lease_expires_at TIMESTAMPTZ,
    last_status SMALLINT CHECK (last_status IS NULL OR last_status BETWEEN 100 AND 599),
    last_error TEXT CHECK (char_length(last_error) <= 64),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    completed_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, endpoint_id, event_id),
    FOREIGN KEY (tenant_id, endpoint_id) REFERENCES webhook_endpoints (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (next_attempt_at) WHERE state = 'pending';
CREATE INDEX webhook_deliveries_endpoint_idx ON webhook_deliveries (tenant_id, endpoint_id, created_at DESC, id);

CREATE TABLE webhook_delivery_attempts (
    tenant_id UUID NOT NULL,
    delivery_id UUID NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt BETWEEN 1 AND 10000),
    started_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    http_status SMALLINT CHECK (http_status IS NULL OR http_status BETWEEN 100 AND 599),
    error TEXT CHECK (char_length(error) <= 64),
    PRIMARY KEY (tenant_id, delivery_id, attempt),
    FOREIGN KEY (tenant_id, delivery_id) REFERENCES webhook_deliveries (tenant_id, id) ON DELETE CASCADE
);

ALTER TABLE webhook_deliveries ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_deliveries FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON webhook_deliveries AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
ALTER TABLE webhook_delivery_attempts ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_delivery_attempts FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON webhook_delivery_attempts AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);

-- The delivery worker has no tenant until it has claimed work, so claiming goes through a
-- SECURITY DEFINER function (as for share links): it reads the due deliveries of enabled
-- endpoints, of enabled hosts, in active tenants, takes a lease and counts the attempt, and
-- returns only the identifiers. The worker then works inside each tenant.
CREATE POLICY definer_lookup ON webhook_endpoints AS PERMISSIVE FOR SELECT
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');
CREATE POLICY definer_claim ON webhook_deliveries AS PERMISSIVE FOR ALL
    USING (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on')
    WITH CHECK (current_user = 'figcraft_migrator' AND current_setting('figcraft.definer_lookup', true) = 'on');

CREATE FUNCTION public.figcraft_claim_webhook_deliveries(p_limit INTEGER, p_lease_secs INTEGER)
RETURNS TABLE (o_tenant_id TEXT, o_delivery_id TEXT)
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
    PERFORM set_config('figcraft.definer_lookup', 'on', true);
    RETURN QUERY
        WITH due AS (
            SELECT d.tenant_id AS due_tenant, d.id AS due_id
            FROM public.webhook_deliveries d
            JOIN public.webhook_endpoints e ON e.tenant_id = d.tenant_id AND e.id = d.endpoint_id AND e.enabled
            JOIN public.host_systems h ON h.tenant_id = e.tenant_id AND h.system_id = e.system_id AND h.enabled
            JOIN public.tenants t ON t.id = d.tenant_id AND t.state = 'active'
            WHERE d.state = 'pending' AND d.next_attempt_at <= clock_timestamp()
              AND (d.lease_expires_at IS NULL OR d.lease_expires_at <= clock_timestamp())
            ORDER BY d.next_attempt_at
            LIMIT LEAST(GREATEST(p_limit, 1), 100)
            FOR UPDATE OF d SKIP LOCKED
        )
        , claimed AS (
            UPDATE public.webhook_deliveries w
            SET lease_expires_at = clock_timestamp() + make_interval(secs => LEAST(GREATEST(p_lease_secs, 5), 600)),
                attempts = w.attempts + 1
            FROM due
            WHERE w.tenant_id = due.due_tenant AND w.id = due.due_id
            RETURNING w.tenant_id AS claimed_tenant, w.id AS claimed_id
        )
        SELECT claimed.claimed_tenant::text, claimed.claimed_id::text FROM claimed;
    PERFORM set_config('figcraft.definer_lookup', '', true);
END
$fn$;
