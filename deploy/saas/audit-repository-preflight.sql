-- S2-012 synthetic fixture for append-only audit repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('92000000-0000-4000-8000-000000000002', 'user', 'Audit repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('92000000-0000-4000-8000-000000000001', 'repo-audit', 'Repository Audit', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('92000000-0000-4000-8000-000000000001', '92000000-0000-4000-8000-000000000002', 'tenant_owner', 'active');
