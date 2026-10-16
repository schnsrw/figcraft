-- S2-010: external resource identifier mappings for embedded-host deployments.
-- A mapping is unique by tenant, registered system, resource kind and external identifier.
-- internal_id intentionally has no single foreign key: depending on resource_kind it may point at
-- a tenant, workspace, project, document or principal row, which cannot be one FK target.

CREATE TABLE external_mappings (
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    system_id TEXT NOT NULL CHECK (char_length(btrim(system_id)) BETWEEN 1 AND 128),
    resource_kind TEXT NOT NULL CHECK (resource_kind IN ('tenant', 'workspace', 'project', 'document', 'principal')),
    external_id TEXT NOT NULL CHECK (char_length(external_id) BETWEEN 1 AND 512),
    internal_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, system_id, resource_kind, external_id)
);
CREATE INDEX external_mappings_internal_idx ON external_mappings (tenant_id, resource_kind, internal_id);
