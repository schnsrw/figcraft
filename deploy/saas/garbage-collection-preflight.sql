-- S3-015 synthetic fixture for garbage-collection integration tests.
-- One tenant per test so each pass sees only its own objects: 1 age, references and dry run,
-- 2 pointer rows without references, 3 an active export, 4 crash recovery, 5 the staging sweep,
-- 6 a bystander whose objects must survive every other tenant's pass. Objects, pointer rows, jobs
-- and reservations are created by the tests under per-run ids and keys.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('ab000000-0000-4000-8000-000000000009', 'user', 'Garbage collection probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('ab000000-0000-4000-8000-000000000001', 'gc-probe-age', 'GC Probe Age', 'test', 'local'),
    ('ab000000-0000-4000-8000-000000000002', 'gc-probe-pointers', 'GC Probe Pointers', 'test', 'local'),
    ('ab000000-0000-4000-8000-000000000003', 'gc-probe-export', 'GC Probe Export', 'test', 'local'),
    ('ab000000-0000-4000-8000-000000000004', 'gc-probe-crash', 'GC Probe Crash', 'test', 'local'),
    ('ab000000-0000-4000-8000-000000000005', 'gc-probe-staging', 'GC Probe Staging', 'test', 'local'),
    ('ab000000-0000-4000-8000-000000000006', 'gc-probe-bystander', 'GC Probe Bystander', 'test', 'local');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('ab000000-0000-4000-8000-000000000021', 'ab000000-0000-4000-8000-000000000002', 'GC pointers'),
    ('ab000000-0000-4000-8000-000000000031', 'ab000000-0000-4000-8000-000000000003', 'GC export');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('ab000000-0000-4000-8000-000000000022', 'ab000000-0000-4000-8000-000000000002', 'ab000000-0000-4000-8000-000000000021', 'GC pointers'),
    ('ab000000-0000-4000-8000-000000000032', 'ab000000-0000-4000-8000-000000000003', 'ab000000-0000-4000-8000-000000000031', 'GC export');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name) VALUES
    ('ab000000-0000-4000-8000-000000000023', 'ab000000-0000-4000-8000-000000000002', 'ab000000-0000-4000-8000-000000000021',
     'ab000000-0000-4000-8000-000000000022', 'GC pointers'),
    ('ab000000-0000-4000-8000-000000000033', 'ab000000-0000-4000-8000-000000000003', 'ab000000-0000-4000-8000-000000000031',
     'ab000000-0000-4000-8000-000000000032', 'GC export');
INSERT INTO public.libraries (id, tenant_id, name) VALUES
    ('ab000000-0000-4000-8000-000000000024', 'ab000000-0000-4000-8000-000000000002', 'GC library');
INSERT INTO public.plugins (id, tenant_id, publisher, name) VALUES
    ('ab000000-0000-4000-8000-000000000025', 'ab000000-0000-4000-8000-000000000002', 'GC probe', 'GC plugin');
