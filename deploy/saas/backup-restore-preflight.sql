-- S3-020 backup/restore probe preflight. The probe works only in two scratch databases it
-- creates and drops itself (it never writes the shared database); run this as the administrator,
-- outside a transaction (DROP DATABASE refuses one), to clear scratch databases a killed run left.
DROP DATABASE IF EXISTS figcraft_backup_source_ae WITH (FORCE);
DROP DATABASE IF EXISTS figcraft_backup_restore_ae WITH (FORCE);
-- The fixture prefix `ae` must stay unused in the shared database, so the restore probe's
-- identifiers can never be confused with another suite's rows.
DO $check$
BEGIN
    IF EXISTS (SELECT 1 FROM public.tenants WHERE id::text LIKE 'ae%') THEN
        RAISE EXCEPTION 'fixture prefix ae is in use in the shared database';
    END IF;
END
$check$;
