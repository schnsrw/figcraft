-- Synthetic S2-004 integration checks. All inserted rows are rolled back.
BEGIN;

CREATE FUNCTION pg_temp.expect_fk(statement TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    BEGIN
        EXECUTE statement;
    EXCEPTION WHEN foreign_key_violation THEN
        RETURN;
    END;
    RAISE EXCEPTION 'expected foreign-key rejection: %', statement;
END;
$$;

CREATE FUNCTION pg_temp.expect_constraint(statement TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
BEGIN
    BEGIN
        EXECUTE statement;
    EXCEPTION WHEN check_violation OR unique_violation THEN
        RETURN;
    END;
    RAISE EXCEPTION 'expected check or uniqueness rejection: %', statement;
END;
$$;

INSERT INTO principals (id, kind, display_name) VALUES
    ('10000000-0000-4000-8000-000000000001', 'user', 'Synthetic member');
INSERT INTO identity_links (principal_id, issuer, subject) VALUES
    ('10000000-0000-4000-8000-000000000001', 'https://issuer.invalid', 'synthetic-user');
INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES
    ('20000000-0000-4000-8000-000000000001', 'synthetic-one', 'Synthetic one', 'test', 'local'),
    ('20000000-0000-4000-8000-000000000002', 'synthetic-two', 'Synthetic two', 'test', 'local');
INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', 'tenant_owner');
INSERT INTO teams (id, tenant_id, slug, name) VALUES
    ('30000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', 'design', 'Design'),
    ('30000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000002', 'design', 'Design');
INSERT INTO team_members (tenant_id, team_id, principal_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', 'team_manager');
INSERT INTO workspaces (id, tenant_id, owning_team_id, name) VALUES
    ('40000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000001', 'Workspace one'),
    ('40000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000002', '30000000-0000-4000-8000-000000000002', 'Workspace two'),
    ('40000000-0000-4000-8000-000000000004', '20000000-0000-4000-8000-000000000001', NULL, 'Another workspace in tenant one');
INSERT INTO workspace_members (tenant_id, workspace_id, principal_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', 'workspace_admin');
INSERT INTO workspace_team_grants (tenant_id, workspace_id, team_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000001', 'editor');
INSERT INTO projects (id, tenant_id, workspace_id, owning_team_id, name) VALUES
    ('50000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000001', 'Project one');
INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES
    ('60000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000001', 'Document one');
INSERT INTO document_members (tenant_id, document_id, principal_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', 'editor');
INSERT INTO document_team_grants (tenant_id, document_id, team_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000001', 'viewer');

SELECT pg_temp.expect_fk($sql$INSERT INTO workspaces (id, tenant_id, owning_team_id, name) VALUES
    ('40000000-0000-4000-8000-000000000003', '20000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000002', 'Wrong team')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO team_members (tenant_id, team_id, principal_id, role) VALUES
    ('20000000-0000-4000-8000-000000000002', '30000000-0000-4000-8000-000000000002', '10000000-0000-4000-8000-000000000001', 'viewer')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO workspace_team_grants (tenant_id, workspace_id, team_id, role) VALUES
    ('20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '30000000-0000-4000-8000-000000000002', 'viewer')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES
    ('50000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000002', 'Wrong workspace')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES
    ('60000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000002', '40000000-0000-4000-8000-000000000002', '50000000-0000-4000-8000-000000000001', 'Wrong tenant')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES
    ('60000000-0000-4000-8000-000000000004', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000004', '50000000-0000-4000-8000-000000000001', 'Wrong parent workspace')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO document_members (tenant_id, document_id, principal_id, role) VALUES
    ('20000000-0000-4000-8000-000000000002', '60000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', 'viewer')$sql$);
SELECT pg_temp.expect_constraint($sql$INSERT INTO identity_links (principal_id, issuer, subject) VALUES
    ('10000000-0000-4000-8000-000000000001', 'https://issuer.invalid', 'synthetic-user')$sql$);
SELECT pg_temp.expect_constraint($sql$INSERT INTO documents (id, tenant_id, workspace_id, project_id, name, current_seq) VALUES
    ('60000000-0000-4000-8000-000000000003', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000001', 'Bad sequence', -1)$sql$);

ROLLBACK;
