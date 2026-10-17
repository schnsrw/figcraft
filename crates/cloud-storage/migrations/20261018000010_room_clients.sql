-- S5-008: the client ids a document's room has handed out, so a room restored after its owner
-- died never hands one out twice and a client that reconnects keeps its id (and with it its undo
-- history and its id space). One bounded row per issued id; written in the same transaction as the
-- edits, so an id is durable before the client's welcome is sent.
CREATE TABLE room_clients (
    tenant_id UUID NOT NULL,
    document_id UUID NOT NULL,
    client_id BIGINT NOT NULL CHECK (client_id BETWEEN 0 AND 4294967295),
    issued_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, document_id, client_id),
    FOREIGN KEY (tenant_id, document_id) REFERENCES documents (tenant_id, id) ON DELETE CASCADE
);

ALTER TABLE room_clients ENABLE ROW LEVEL SECURITY;
ALTER TABLE room_clients FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON room_clients AS PERMISSIVE FOR ALL
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
