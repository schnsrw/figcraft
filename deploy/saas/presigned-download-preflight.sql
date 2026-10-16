-- S3-006 synthetic fixture for presigned download integration tests.
-- Principal 9 is an active member of tenants 1, 2 and the suspended tenant 3; principal a is a
-- suspended member of tenant 1, principal b is in no tenant, and principal c is a suspended
-- principal with an active tenant 1 membership.
INSERT INTO public.principals (id, kind, display_name, state) VALUES
    ('a4000000-0000-4000-8000-000000000009', 'user', 'Download probe member', 'active'),
    ('a4000000-0000-4000-8000-00000000000a', 'user', 'Download probe suspended member', 'active'),
    ('a4000000-0000-4000-8000-00000000000b', 'user', 'Download probe outsider', 'active'),
    ('a4000000-0000-4000-8000-00000000000c', 'user', 'Download probe suspended principal', 'suspended');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region, state) VALUES
    ('a4000000-0000-4000-8000-000000000001', 'downloads-probe', 'Downloads Probe', 'test', 'local', 'active'),
    ('a4000000-0000-4000-8000-000000000002', 'downloads-other', 'Downloads Other', 'test', 'local', 'active'),
    ('a4000000-0000-4000-8000-000000000003', 'downloads-suspended', 'Downloads Suspended', 'test', 'local', 'suspended');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a4000000-0000-4000-8000-000000000001', 'a4000000-0000-4000-8000-000000000009', 'viewer', 'active'),
    ('a4000000-0000-4000-8000-000000000002', 'a4000000-0000-4000-8000-000000000009', 'viewer', 'active'),
    ('a4000000-0000-4000-8000-000000000003', 'a4000000-0000-4000-8000-000000000009', 'viewer', 'active'),
    ('a4000000-0000-4000-8000-000000000001', 'a4000000-0000-4000-8000-00000000000a', 'editor', 'suspended'),
    ('a4000000-0000-4000-8000-000000000001', 'a4000000-0000-4000-8000-00000000000c', 'editor', 'active');
