-- S6-056: per-workspace policy settings. The owner and workspace admins edit them; the server
-- enforces them (who may invite, guests, share links). Sign-in methods stay in
-- tenant_account_settings (S6-034) and are shown and edited together with these through the
-- policy API. An empty object means "all defaults" (figcraft_cloud_domain::policy::WorkspacePolicy).

ALTER TABLE tenants
    ADD COLUMN policy JSONB NOT NULL DEFAULT '{}'::jsonb
    CHECK (jsonb_typeof(policy) = 'object' AND pg_column_size(policy) <= 2048);
