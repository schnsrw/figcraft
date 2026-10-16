-- S3-004/S3-005 synthetic fixture for upload reservation integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('a2000000-0000-4000-8000-000000000009', 'user', 'Upload reservation probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a2000000-0000-4000-8000-000000000001', 'uploads-probe', 'Uploads Probe', 'test', 'local'),
    ('a2000000-0000-4000-8000-000000000002', 'uploads-other', 'Uploads Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a2000000-0000-4000-8000-000000000001', 'a2000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
