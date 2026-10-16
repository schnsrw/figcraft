-- S3-020 backup/restore probe cleanup: drop the scratch databases. The probe drops them itself and
-- deletes every object version under its per-run document prefix; this covers a killed run.
-- Run as the administrator, outside a transaction.
DROP DATABASE IF EXISTS figcraft_backup_source_ae WITH (FORCE);
DROP DATABASE IF EXISTS figcraft_backup_restore_ae WITH (FORCE);
