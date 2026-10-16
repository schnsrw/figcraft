-- S2-010 synthetic fixture for external mapping repository integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('c0000000-0000-4000-8000-000000000003', 'user', 'External mapping repository probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('90000000-0000-4000-8000-000000000001', 'repo-ext', 'Repository Ext', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('90000000-0000-4000-8000-000000000001', 'c0000000-0000-4000-8000-000000000003', 'tenant_owner', 'active');
