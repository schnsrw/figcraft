-- S2-016 synthetic fixture for document event replay integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('95000000-0000-4000-8000-000000000009', 'user', 'Event replay probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('95000000-0000-4000-8000-000000000001', 'repo-replay', 'Repository Replay', 'test', 'local'),
    ('95000000-0000-4000-8000-000000000002', 'repo-replay-other', 'Repository Replay Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('95000000-0000-4000-8000-000000000001', '95000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('95100000-0000-4000-8000-000000000001', '95000000-0000-4000-8000-000000000001', 'Workspace Replay 1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('95200000-0000-4000-8000-000000000001', '95000000-0000-4000-8000-000000000001', '95100000-0000-4000-8000-000000000001', 'Project Replay 1');
-- Doc one carries the committed chain; doc two only ever sees corruption inside rolled-back
-- transactions, so it stays empty.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('95300000-0000-4000-8000-000000000001', '95000000-0000-4000-8000-000000000001', '95100000-0000-4000-8000-000000000001', '95200000-0000-4000-8000-000000000001', 'Replay doc one', 'active'),
    ('95300000-0000-4000-8000-000000000002', '95000000-0000-4000-8000-000000000001', '95100000-0000-4000-8000-000000000001', '95200000-0000-4000-8000-000000000001', 'Replay doc two', 'active');
