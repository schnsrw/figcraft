-- S3-018/S3-019 synthetic fixture for the legacy import and cutover verification probes. Each run
-- imports freshly named legacy documents, so their derived ids never collide with earlier runs.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('ad000000-0000-4000-8000-000000000009', 'user', 'Legacy import probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('ad000000-0000-4000-8000-000000000001', 'legacy-import-probe', 'Legacy Import Probe', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('ad000000-0000-4000-8000-000000000001', 'ad000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('ad100000-0000-4000-8000-000000000001', 'ad000000-0000-4000-8000-000000000001', 'Workspace Legacy Import');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('ad200000-0000-4000-8000-000000000001', 'ad000000-0000-4000-8000-000000000001', 'ad100000-0000-4000-8000-000000000001', 'Project Legacy Import');
