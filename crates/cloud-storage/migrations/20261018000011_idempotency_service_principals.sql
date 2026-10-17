-- S6-012: host API calls are made by service-account principals, which are deliberately not tenant
-- members, yet their idempotent requests need idempotency keys. The old foreign key demanded a
-- tenant membership for every key owner. Keys are disposable (they expire and are swept), so they
-- now only require that the principal exists, and go away with it. Tenant isolation is unchanged:
-- the primary key and the forced row-level security policy both start with tenant_id.
ALTER TABLE idempotency_keys DROP CONSTRAINT idempotency_keys_tenant_id_principal_id_fkey;
ALTER TABLE idempotency_keys ADD CONSTRAINT idempotency_keys_principal_id_fkey
    FOREIGN KEY (principal_id) REFERENCES principals (id) ON DELETE CASCADE;
