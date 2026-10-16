-- Synthetic S2-004/S2-005/S2-009 integration checks. All inserted rows are rolled back.
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
SELECT pg_temp.expect_constraint($sql$INSERT INTO documents (id, tenant_id, workspace_id, project_id, name, deleted_at) VALUES
    ('60000000-0000-4000-8000-000000000006', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000001', 'Bad deleted_at', now())$sql$);
SELECT pg_temp.expect_constraint($sql$INSERT INTO documents (id, tenant_id, workspace_id, project_id, name, state) VALUES
    ('60000000-0000-4000-8000-000000000007', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000001', 'Bad deleting state', 'deleting')$sql$);
INSERT INTO documents (id, tenant_id, workspace_id, project_id, name, state, deleted_at) VALUES
    ('60000000-0000-4000-8000-000000000008', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000001', 'Valid deleting document', 'deleting', now());

INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES
    ('60000000-0000-4000-8000-000000000005', '20000000-0000-4000-8000-000000000001', '40000000-0000-4000-8000-000000000001', '50000000-0000-4000-8000-000000000001', 'Another document');
INSERT INTO document_events (tenant_id, document_id, seq, fencing_token, payload, checksum, actor_principal_id) VALUES
    ('20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', 1, 1, decode('01', 'hex'), decode(repeat('ab', 32), 'hex'), '10000000-0000-4000-8000-000000000001');
UPDATE documents SET current_seq = 1 WHERE id = '60000000-0000-4000-8000-000000000001';
SELECT pg_temp.expect_constraint($sql$INSERT INTO document_events (tenant_id, document_id, seq, fencing_token, payload, checksum, actor_principal_id) VALUES
    ('20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', 1, 1, decode('02', 'hex'), decode(repeat('cd', 32), 'hex'), '10000000-0000-4000-8000-000000000001')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO document_events (tenant_id, document_id, seq, fencing_token, payload, checksum, actor_principal_id) VALUES
    ('20000000-0000-4000-8000-000000000002', '60000000-0000-4000-8000-000000000001', 2, 1, decode('02', 'hex'), decode(repeat('cd', 32), 'hex'), '10000000-0000-4000-8000-000000000001')$sql$);
SELECT pg_temp.expect_constraint($sql$INSERT INTO document_events (tenant_id, document_id, seq, fencing_token, payload, checksum, actor_principal_id) VALUES
    ('20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', 2, 1, decode('02', 'hex'), decode('ff', 'hex'), '10000000-0000-4000-8000-000000000001')$sql$);

INSERT INTO document_snapshots (id, tenant_id, document_id, seq, object_id, checksum, size_bytes, state, verified_at) VALUES
    ('70000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', 1, '71000000-0000-4000-8000-000000000001', decode(repeat('ab', 32), 'hex'), 128, 'verified', now()),
    ('70000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000005', 0, '71000000-0000-4000-8000-000000000002', decode(repeat('cd', 32), 'hex'), 64, 'verified', now());
INSERT INTO document_snapshots (id, tenant_id, document_id, seq, object_id, checksum, size_bytes) VALUES
    ('70000000-0000-4000-8000-000000000004', '20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', 1, '71000000-0000-4000-8000-000000000004', decode(repeat('ef', 32), 'hex'), 128);
UPDATE documents SET current_snapshot_id = '70000000-0000-4000-8000-000000000001'
    WHERE id = '60000000-0000-4000-8000-000000000001';
SELECT pg_temp.expect_fk($sql$UPDATE documents SET current_snapshot_id = '70000000-0000-4000-8000-000000000002'
    WHERE id = '60000000-0000-4000-8000-000000000001'$sql$);
SELECT pg_temp.expect_constraint($sql$INSERT INTO document_snapshots (id, tenant_id, document_id, seq, object_id, checksum, size_bytes) VALUES
    ('70000000-0000-4000-8000-000000000003', '20000000-0000-4000-8000-000000000001', '60000000-0000-4000-8000-000000000001', 1, '71000000-0000-4000-8000-000000000003', decode('ff', 'hex'), 128)$sql$);

INSERT INTO jobs (id, tenant_id, kind, input) VALUES
    ('80000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', 'synthetic', '{}'::jsonb);
SELECT pg_temp.expect_fk($sql$INSERT INTO jobs (id, tenant_id, kind, input) VALUES
    ('80000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000003', 'synthetic', '{}'::jsonb)$sql$);
SELECT pg_temp.expect_constraint($sql$UPDATE jobs SET lease_owner = 'worker-one' WHERE id = '80000000-0000-4000-8000-000000000001'$sql$);

INSERT INTO idempotency_keys (tenant_id, principal_id, route, key_hash, request_hash, expires_at) VALUES
    ('20000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', '/v1/documents', decode(repeat('11', 32), 'hex'), decode(repeat('22', 32), 'hex'), now() + interval '1 hour');
UPDATE idempotency_keys SET state = 'completed', response_status = 201, response_body = decode('7b7d', 'hex'), response_content_type = 'application/json'
    WHERE tenant_id = '20000000-0000-4000-8000-000000000001';
SELECT pg_temp.expect_constraint($sql$INSERT INTO idempotency_keys (tenant_id, principal_id, route, key_hash, request_hash, expires_at) VALUES
    ('20000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', '/v1/documents', decode(repeat('11', 32), 'hex'), decode(repeat('33', 32), 'hex'), now() + interval '1 hour')$sql$);
SELECT pg_temp.expect_fk($sql$INSERT INTO idempotency_keys (tenant_id, principal_id, route, key_hash, request_hash, expires_at) VALUES
    ('20000000-0000-4000-8000-000000000002', '10000000-0000-4000-8000-000000000001', '/v1/documents', decode(repeat('11', 32), 'hex'), decode(repeat('22', 32), 'hex'), now() + interval '1 hour')$sql$);

INSERT INTO audit_events (id, tenant_id, actor_principal_id, action, resource_kind, resource_id, outcome) VALUES
    ('90000000-0000-4000-8000-000000000001', '20000000-0000-4000-8000-000000000001', '10000000-0000-4000-8000-000000000001', 'document.created', 'document', '60000000-0000-4000-8000-000000000001', 'succeeded');
SELECT pg_temp.expect_fk($sql$INSERT INTO audit_events (id, tenant_id, action, resource_kind, outcome) VALUES
    ('90000000-0000-4000-8000-000000000002', '20000000-0000-4000-8000-000000000003', 'document.created', 'document', 'succeeded')$sql$);
SELECT pg_temp.expect_constraint($sql$INSERT INTO audit_events (id, tenant_id, action, resource_kind, outcome) VALUES
    ('90000000-0000-4000-8000-000000000003', '20000000-0000-4000-8000-000000000001', 'document.created', 'document', 'invalid')$sql$);

ROLLBACK;
