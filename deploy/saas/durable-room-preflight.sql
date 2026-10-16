-- S2-018 synthetic fixture for durable collaboration room integration tests.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('97000000-0000-4000-8000-000000000009', 'user', 'Durable room probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('97000000-0000-4000-8000-000000000001', 'relay-room', 'Relay Room', 'test', 'local'),
    ('97000000-0000-4000-8000-000000000002', 'relay-room-other', 'Relay Room Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('97000000-0000-4000-8000-000000000001', '97000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('97100000-0000-4000-8000-000000000001', '97000000-0000-4000-8000-000000000001', 'Workspace Room 1');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('97200000-0000-4000-8000-000000000001', '97000000-0000-4000-8000-000000000001', '97100000-0000-4000-8000-000000000001', 'Project Room 1');
-- Doc one accepts edits; doc two is archived and must refuse them.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('97300000-0000-4000-8000-000000000001', '97000000-0000-4000-8000-000000000001', '97100000-0000-4000-8000-000000000001', '97200000-0000-4000-8000-000000000001', 'Room doc one', 'active'),
    ('97300000-0000-4000-8000-000000000002', '97000000-0000-4000-8000-000000000001', '97100000-0000-4000-8000-000000000001', '97200000-0000-4000-8000-000000000001', 'Room doc two', 'archived');
