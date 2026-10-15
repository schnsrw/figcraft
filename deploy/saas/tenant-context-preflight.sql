-- Synthetic S2-007 RLS fixture. This is not a production policy or migration.
CREATE TABLE public.tenant_context_probe (
    tenant_id UUID NOT NULL,
    label TEXT NOT NULL
);
INSERT INTO public.tenant_context_probe (tenant_id, label) VALUES
    ('a0000000-0000-4000-8000-000000000001', 'tenant-a'),
    ('b0000000-0000-4000-8000-000000000002', 'tenant-b');
ALTER TABLE public.tenant_context_probe ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_context_probe_scope ON public.tenant_context_probe
    USING (tenant_id = NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid);
GRANT SELECT ON public.tenant_context_probe TO figcraft_runtime;
