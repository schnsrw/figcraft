-- S3-011 synthetic fixture for plugin bundle and manifest storage integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('a9000000-0000-4000-8000-000000000009', 'user', 'Plugin store probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a9000000-0000-4000-8000-000000000001', 'plugin-store-probe', 'Plugin Store Probe', 'test', 'local'),
    ('a9000000-0000-4000-8000-000000000002', 'plugin-store-other', 'Plugin Store Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a9000000-0000-4000-8000-000000000001', 'a9000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
