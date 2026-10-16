-- S3 follow-up synthetic fixture for register-first and the orphan-key sweep.
-- Tenant 1: orphan keys (no row, live reservation staging key, registered key, dry run).
-- Tenant 2: the register-first crash window (a document whose snapshot store rolls back, and a
-- library whose publish rolls back). Objects, reservations and keys are created by the tests
-- under per-run ids.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('b0000000-0000-4000-8000-000000000009', 'user', 'Orphan sweep probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('b0000000-0000-4000-8000-000000000001', 'orphan-probe-keys', 'Orphan Probe Keys', 'test', 'local'),
    ('b0000000-0000-4000-8000-000000000002', 'orphan-probe-crash', 'Orphan Probe Crash', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('b0000000-0000-4000-8000-000000000002', 'b0000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('b0000000-0000-4000-8000-000000000021', 'b0000000-0000-4000-8000-000000000002', 'Orphan crash');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('b0000000-0000-4000-8000-000000000022', 'b0000000-0000-4000-8000-000000000002', 'b0000000-0000-4000-8000-000000000021', 'Orphan crash');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, current_seq) VALUES
    ('b0000000-0000-4000-8000-000000000023', 'b0000000-0000-4000-8000-000000000002', 'b0000000-0000-4000-8000-000000000021',
     'b0000000-0000-4000-8000-000000000022', 'Orphan crash', 1000000);
INSERT INTO public.libraries (id, tenant_id, name) VALUES
    ('b0000000-0000-4000-8000-000000000024', 'b0000000-0000-4000-8000-000000000002', 'Orphan crash library');
