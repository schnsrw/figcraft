-- S3-010 synthetic fixture for team-library version, grant and usage integration tests.
-- Tenant 1 has the owning team 11 (principal 9 an editor, principal a a viewer), team 12 (principal
-- c an editor; the tests grant it `publisher`) and the archived team 13. Principal d is a tenant
-- admin in no team, principal e a tenant editor in no team, and principal b belongs only to tenant
-- 2 (team 21). Documents 51 and 52 are active, 53 archived and 54 deleting; 61 is tenant 2's.
-- Libraries, versions and objects are created by the test itself under unique per-run keys.
INSERT INTO public.principals (id, kind, display_name, state) VALUES
    ('a8000000-0000-4000-8000-000000000009', 'user', 'Library probe owner-team editor', 'active'),
    ('a8000000-0000-4000-8000-00000000000a', 'user', 'Library probe owner-team viewer', 'active'),
    ('a8000000-0000-4000-8000-00000000000b', 'user', 'Library probe other tenant', 'active'),
    ('a8000000-0000-4000-8000-00000000000c', 'user', 'Library probe granted-team editor', 'active'),
    ('a8000000-0000-4000-8000-00000000000d', 'user', 'Library probe tenant admin', 'active'),
    ('a8000000-0000-4000-8000-00000000000e', 'user', 'Library probe tenant editor', 'active');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('a8000000-0000-4000-8000-000000000001', 'libraries-probe', 'Libraries Probe', 'test', 'local'),
    ('a8000000-0000-4000-8000-000000000002', 'libraries-other', 'Libraries Other', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000009', 'viewer', 'active'),
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-00000000000a', 'viewer', 'active'),
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-00000000000c', 'viewer', 'active'),
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-00000000000d', 'tenant_admin', 'active'),
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-00000000000e', 'editor', 'active'),
    ('a8000000-0000-4000-8000-000000000002', 'a8000000-0000-4000-8000-00000000000b', 'tenant_owner', 'active');
INSERT INTO public.teams (id, tenant_id, slug, name, state) VALUES
    ('a8000000-0000-4000-8000-000000000011', 'a8000000-0000-4000-8000-000000000001', 'design-system', 'Design System', 'active'),
    ('a8000000-0000-4000-8000-000000000012', 'a8000000-0000-4000-8000-000000000001', 'product', 'Product', 'active'),
    ('a8000000-0000-4000-8000-000000000013', 'a8000000-0000-4000-8000-000000000001', 'retired', 'Retired', 'archived'),
    ('a8000000-0000-4000-8000-000000000021', 'a8000000-0000-4000-8000-000000000002', 'other', 'Other', 'active');
INSERT INTO public.team_members (tenant_id, team_id, principal_id, role, state) VALUES
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000011', 'a8000000-0000-4000-8000-000000000009', 'editor', 'active'),
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000011', 'a8000000-0000-4000-8000-00000000000a', 'viewer', 'active'),
    ('a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000012', 'a8000000-0000-4000-8000-00000000000c', 'editor', 'active'),
    ('a8000000-0000-4000-8000-000000000002', 'a8000000-0000-4000-8000-000000000021', 'a8000000-0000-4000-8000-00000000000b', 'team_manager', 'active');
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('a8000000-0000-4000-8000-000000000031', 'a8000000-0000-4000-8000-000000000001', 'Libraries Workspace'),
    ('a8000000-0000-4000-8000-000000000032', 'a8000000-0000-4000-8000-000000000002', 'Other Workspace');
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('a8000000-0000-4000-8000-000000000041', 'a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000031', 'Libraries Project'),
    ('a8000000-0000-4000-8000-000000000042', 'a8000000-0000-4000-8000-000000000002', 'a8000000-0000-4000-8000-000000000032', 'Other Project');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('a8000000-0000-4000-8000-000000000051', 'a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000031', 'a8000000-0000-4000-8000-000000000041', 'Components', 'active'),
    ('a8000000-0000-4000-8000-000000000052', 'a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000031', 'a8000000-0000-4000-8000-000000000041', 'Consumer', 'active'),
    ('a8000000-0000-4000-8000-000000000053', 'a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000031', 'a8000000-0000-4000-8000-000000000041', 'Archived Consumer', 'archived'),
    ('a8000000-0000-4000-8000-000000000061', 'a8000000-0000-4000-8000-000000000002', 'a8000000-0000-4000-8000-000000000032', 'a8000000-0000-4000-8000-000000000042', 'Other Document', 'active');
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state, deleted_at) VALUES
    ('a8000000-0000-4000-8000-000000000054', 'a8000000-0000-4000-8000-000000000001', 'a8000000-0000-4000-8000-000000000031', 'a8000000-0000-4000-8000-000000000041', 'Deleting Consumer', 'deleting', now());
