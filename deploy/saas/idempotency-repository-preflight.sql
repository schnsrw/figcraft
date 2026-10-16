-- S2-011 synthetic fixture for idempotency-key reservation repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('91000000-0000-4000-8000-000000000002', 'user', 'Idempotency repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('91000000-0000-4000-8000-000000000001', 'repo-idem', 'Repository Idempotency', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('91000000-0000-4000-8000-000000000001', '91000000-0000-4000-8000-000000000002', 'tenant_owner', 'active');
