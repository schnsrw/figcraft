-- S3-007 synthetic fixture for object-reference accounting and retention-hold integration tests.
-- Objects are registered by the test itself under unique per-run keys; only tenants are seeded.
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a5000000-0000-4000-8000-000000000001', 'object-references-probe', 'Object References Probe', 'test', 'local'),
    ('a5000000-0000-4000-8000-000000000002', 'object-references-other', 'Object References Other', 'test', 'local');
