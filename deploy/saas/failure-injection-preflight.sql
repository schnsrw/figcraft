-- S2-020 synthetic fixture for the failure-injection tests: killed connections, lock and statement
-- timeouts, lost acknowledgements and idempotency retry boundaries. The tests commit real rows and
-- read their starting point from the database, so this fixture is safe to reapply over a previous
-- run's data.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('98000000-0000-4000-8000-000000000009', 'user', 'Failure injection probe')
    ON CONFLICT DO NOTHING;
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('98000000-0000-4000-8000-000000000001', 'failure-injection', 'Failure Injection', 'test', 'local')
    ON CONFLICT DO NOTHING;
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('98000000-0000-4000-8000-000000000001', '98000000-0000-4000-8000-000000000009', 'tenant_owner', 'active')
    ON CONFLICT DO NOTHING;
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('98100000-0000-4000-8000-000000000001', '98000000-0000-4000-8000-000000000001', 'Workspace Failure 1')
    ON CONFLICT DO NOTHING;
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('98200000-0000-4000-8000-000000000001', '98000000-0000-4000-8000-000000000001', '98100000-0000-4000-8000-000000000001', 'Project Failure 1')
    ON CONFLICT DO NOTHING;
-- One document per scenario: a kill after append, a kill while queued on the row lock, a lost
-- acknowledgement, lock and statement timeouts, and the durable relay room.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('98300000-0000-4000-8000-000000000001', '98000000-0000-4000-8000-000000000001', '98100000-0000-4000-8000-000000000001', '98200000-0000-4000-8000-000000000001', 'Failure kill after append', 'active'),
    ('98300000-0000-4000-8000-000000000002', '98000000-0000-4000-8000-000000000001', '98100000-0000-4000-8000-000000000001', '98200000-0000-4000-8000-000000000001', 'Failure kill in lock wait', 'active'),
    ('98300000-0000-4000-8000-000000000003', '98000000-0000-4000-8000-000000000001', '98100000-0000-4000-8000-000000000001', '98200000-0000-4000-8000-000000000001', 'Failure lost acknowledgement', 'active'),
    ('98300000-0000-4000-8000-000000000004', '98000000-0000-4000-8000-000000000001', '98100000-0000-4000-8000-000000000001', '98200000-0000-4000-8000-000000000001', 'Failure timeouts', 'active'),
    ('98300000-0000-4000-8000-000000000005', '98000000-0000-4000-8000-000000000001', '98100000-0000-4000-8000-000000000001', '98200000-0000-4000-8000-000000000001', 'Failure relay room', 'active')
    ON CONFLICT DO NOTHING;
