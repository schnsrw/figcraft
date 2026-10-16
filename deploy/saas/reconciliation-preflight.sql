-- S3-016 synthetic fixture for the database-to-bucket reconciliation report integration test.
-- Objects, reservations and bucket keys are planted by the test itself under unique per-run keys.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('ac000000-0000-4000-8000-000000000009', 'user', 'Reconciliation probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('ac000000-0000-4000-8000-000000000001', 'reconciliation-probe', 'Reconciliation Probe', 'test', 'local'),
    ('ac000000-0000-4000-8000-000000000002', 'reconciliation-other', 'Reconciliation Other', 'test', 'local');
