-- TS-013 / TS-014: a workspace works with or without email.
--
-- * An invitation is no longer bound to an email address: `email` becomes optional (NULL is a
--   "link-only" invitation anyone holding the link can accept by choosing a login name and a
--   password). When an address is noted it only enables sending the link and, for OIDC acceptance,
--   the verified-email check it always had.
-- * A password set by an administrator is temporary: `must_change_password` is true until the person
--   chooses their own. A sign-in with such a password yields a session flagged
--   `password_change_required`, which the API accepts only on the routes that let the person change
--   it (and sign out).
ALTER TABLE team_invitations ALTER COLUMN email DROP NOT NULL;
ALTER TABLE team_invitations DROP CONSTRAINT team_invitations_email_check;
ALTER TABLE team_invitations ADD CONSTRAINT team_invitations_email_check
    CHECK (email IS NULL OR (char_length(email) BETWEEN 3 AND 320 AND position('@' IN email) > 1));

ALTER TABLE local_credentials ADD COLUMN must_change_password BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE browser_sessions ADD COLUMN password_change_required BOOLEAN NOT NULL DEFAULT FALSE;
