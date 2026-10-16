-- S3-010/S3-011/S3-012: team-library versions and usage, plugin versions, exports and thumbnails.
-- Every stored payload is an `objects` row; these tables only name which object a version, export
-- or thumbnail is, through composite tenant-scoped foreign keys.

ALTER TABLE jobs ADD CONSTRAINT jobs_tenant_id_key UNIQUE (tenant_id, id);

CREATE TABLE libraries (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    owning_team_id UUID,
    source_document_id UUID,
    name TEXT NOT NULL CHECK (char_length(btrim(name)) BETWEEN 1 AND 255),
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'archived', 'deleted')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id, owning_team_id) REFERENCES teams (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, source_document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT
);

CREATE TABLE library_team_grants (
    tenant_id UUID NOT NULL,
    library_id UUID NOT NULL,
    team_id UUID NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('viewer', 'publisher')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, library_id, team_id),
    FOREIGN KEY (tenant_id, library_id) REFERENCES libraries (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, team_id) REFERENCES teams (tenant_id, id) ON DELETE RESTRICT
);

CREATE TABLE library_versions (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    library_id UUID NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0),
    object_id UUID NOT NULL,
    sha256 BYTEA NOT NULL CHECK (octet_length(sha256) = 32),
    published_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    UNIQUE (tenant_id, library_id, version),
    FOREIGN KEY (tenant_id, library_id) REFERENCES libraries (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT
);

-- Which library version a document uses; a document uses at most one version of each library.
CREATE TABLE library_usages (
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    library_id UUID NOT NULL,
    library_version_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, document_id, library_id),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, library_id) REFERENCES libraries (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, library_version_id) REFERENCES library_versions (tenant_id, id) ON DELETE RESTRICT
);
CREATE INDEX library_usages_version_idx ON library_usages (tenant_id, library_version_id);

-- Private, tenant-installed plugins. A public plugin directory is later work.
CREATE TABLE plugins (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE RESTRICT,
    publisher TEXT NOT NULL CHECK (char_length(btrim(publisher)) BETWEEN 1 AND 255),
    name TEXT NOT NULL CHECK (char_length(btrim(name)) BETWEEN 1 AND 255),
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'disabled', 'deleted')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id)
);

CREATE TABLE plugin_versions (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    plugin_id UUID NOT NULL,
    -- Mirrors `object_metadata::keys::plugin_version_valid`.
    version TEXT NOT NULL CHECK (version ~ '^[a-z0-9][a-z0-9.-]{0,63}$' AND position('..' IN version) = 0),
    object_id UUID NOT NULL,
    sha256 BYTEA NOT NULL CHECK (octet_length(sha256) = 32),
    manifest JSONB NOT NULL CHECK (jsonb_typeof(manifest) = 'object' AND octet_length(manifest::text) <= 262144),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'verified', 'rejected', 'withdrawn')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, id),
    UNIQUE (tenant_id, plugin_id, version),
    FOREIGN KEY (tenant_id, plugin_id) REFERENCES plugins (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT
);

CREATE TABLE exports (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    requested_by UUID NOT NULL REFERENCES principals (id) ON DELETE RESTRICT,
    requested_seq BIGINT NOT NULL CHECK (requested_seq >= 0),
    format TEXT NOT NULL CHECK (format IN ('png', 'jpeg', 'webp', 'svg', 'pdf', 'figcraft')),
    options JSONB NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(options) = 'object' AND octet_length(options::text) <= 16384),
    job_id UUID,
    result_object_id UUID,
    state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'expired')),
    error_code TEXT CHECK (char_length(error_code) BETWEEN 1 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, result_object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT,
    CHECK ((state = 'succeeded') = (result_object_id IS NOT NULL)),
    CHECK ((state IN ('succeeded', 'failed', 'expired')) = (completed_at IS NOT NULL)),
    CHECK ((state = 'failed') = (error_code IS NOT NULL))
);
CREATE INDEX exports_document_created_idx ON exports (tenant_id, document_id, created_at DESC, id);

CREATE TABLE document_thumbnails (
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq >= 0),
    object_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, document_id, seq),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (tenant_id, object_id) REFERENCES objects (tenant_id, id) ON DELETE RESTRICT
);
