-- S3 follow-up synthetic fixture for the document recovery quarantine integration tests.
-- Documents are created by the probes themselves (fresh ids per run) under this project.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('af000000-0000-4000-8000-000000000009', 'user', 'Quarantine probe operator');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('af000000-0000-4000-8000-000000000001', 'quarantine-probe', 'Quarantine Probe', 'test', 'local'),
    ('af000000-0000-4000-8000-000000000002', 'quarantine-other', 'Quarantine Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('af000000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('af100000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000001', 'Quarantine workspace');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('af200000-0000-4000-8000-000000000001', 'af000000-0000-4000-8000-000000000001', 'af100000-0000-4000-8000-000000000001', 'Quarantine project');
