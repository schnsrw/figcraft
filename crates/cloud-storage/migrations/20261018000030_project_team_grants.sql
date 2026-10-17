-- S6-052: share a project with a team other than the one that owns it. Same shape as
-- workspace_team_grants: the grant's role is capped by each member's role in the team when access
-- is resolved (effective_roles.rs). Documents inherit it through their project.

CREATE TABLE project_team_grants (
    tenant_id UUID NOT NULL,
    project_id UUID NOT NULL,
    team_id UUID NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('workspace_admin', 'editor', 'commenter', 'viewer')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, project_id, team_id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES projects (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, team_id) REFERENCES teams (tenant_id, id) ON DELETE RESTRICT
);
CREATE INDEX project_team_grants_team_idx ON project_team_grants (tenant_id, team_id);

ALTER TABLE public.project_team_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.project_team_grants FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON public.project_team_grants AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
