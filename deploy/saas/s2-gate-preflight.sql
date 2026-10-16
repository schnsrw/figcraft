-- S2 gate synthetic fixture: only the tenant and its principal. The probe creates the workspace,
-- project and document through the metadata repositories, under ids with the 991/992/993 prefix.
INSERT INTO public.principals (id, kind, display_name) VALUES
    ('99000000-0000-4000-8000-000000000009', 'user', 'S2 gate probe');
INSERT INTO public.tenants (id, slug, name, plan_id, home_region) VALUES
    ('99000000-0000-4000-8000-000000000001', 's2-gate', 'S2 Gate', 'test', 'local');
INSERT INTO public.tenant_members (tenant_id, principal_id, role, state) VALUES
    ('99000000-0000-4000-8000-000000000001', '99000000-0000-4000-8000-000000000009', 'tenant_owner', 'active');
