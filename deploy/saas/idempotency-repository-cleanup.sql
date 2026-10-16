-- Remove only the synthetic S2-011 fixture rows from the disposable database.
-- idempotency_keys is deleted first because it holds a FK to tenant_members, which must be
-- satisfied for the remaining deletes to proceed in order.
DELETE FROM public.idempotency_keys WHERE tenant_id = '91000000-0000-4000-8000-000000000001';
DELETE FROM public.tenant_members WHERE tenant_id = '91000000-0000-4000-8000-000000000001';
DELETE FROM public.tenants WHERE id = '91000000-0000-4000-8000-000000000001';
DELETE FROM public.principals WHERE id = '91000000-0000-4000-8000-000000000002';
