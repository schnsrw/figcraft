-- S2-019 synthetic fixture for the multi-connection sequence, fencing and idempotency race tests.
-- The tests commit real rows and read their starting point from the database, so this fixture is
-- safe to reapply over a previous run's data.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('96000000-0000-4000-8000-000000000009', 'user', 'Concurrency probe')
    ON CONFLICT DO NOTHING;
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('96000000-0000-4000-8000-000000000001', 'repo-concurrency', 'Repository Concurrency', 'test', 'local')
    ON CONFLICT DO NOTHING;
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('96000000-0000-4000-8000-000000000001', '96000000-0000-4000-8000-000000000009', 'tenant_owner', 'active')
    ON CONFLICT DO NOTHING;
INSERT INTO public.workspaces (id, tenant_id, name) VALUES
    ('96100000-0000-4000-8000-000000000001', '96000000-0000-4000-8000-000000000001', 'Workspace Concurrency 1')
    ON CONFLICT DO NOTHING;
INSERT INTO public.projects (id, tenant_id, workspace_id, name) VALUES
    ('96200000-0000-4000-8000-000000000001', '96000000-0000-4000-8000-000000000001', '96100000-0000-4000-8000-000000000001', 'Project Concurrency 1')
    ON CONFLICT DO NOTHING;
-- One document per race so the tests can run in parallel: same-sequence writers, consecutive
-- writers with retry, ordered takeovers against a lock holder, and a free-running fenced owner.
INSERT INTO public.documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('96300000-0000-4000-8000-000000000001', '96000000-0000-4000-8000-000000000001', '96100000-0000-4000-8000-000000000001', '96200000-0000-4000-8000-000000000001', 'Concurrency same seq', 'active'),
    ('96300000-0000-4000-8000-000000000002', '96000000-0000-4000-8000-000000000001', '96100000-0000-4000-8000-000000000001', '96200000-0000-4000-8000-000000000001', 'Concurrency consecutive', 'active'),
    ('96300000-0000-4000-8000-000000000003', '96000000-0000-4000-8000-000000000001', '96100000-0000-4000-8000-000000000001', '96200000-0000-4000-8000-000000000001', 'Concurrency fencing', 'active'),
    ('96300000-0000-4000-8000-000000000004', '96000000-0000-4000-8000-000000000001', '96100000-0000-4000-8000-000000000001', '96200000-0000-4000-8000-000000000001', 'Concurrency takeover', 'active')
    ON CONFLICT DO NOTHING;
