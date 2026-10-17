# Host FigCraft for your team

This guide takes one machine from nothing to a team signing in with passwords, creating teams
and projects and editing the same file together. No identity provider and no email server are
needed. It describes the single-node Compose stack in [`deploy/saas`](../../deploy/saas). Status
and limits are listed at the end; the checklist that tracks this work is the *Priority track* in
[`tracker.md`](tracker.md).

## What you need

- A Linux, macOS or Windows (WSL) machine with Docker and Docker Compose, 4 GB of memory and
  10 GB of free disk. The first build compiles the cloud service and the browser editor and takes
  about 15 to 20 minutes.
- `openssl` and `git`.
- A name people will type. For a quick trial on your own computer that is `http://127.0.0.1:8080`
  and you can skip step 3. For a team it must be a name on your network (for example
  `figcraft.office.lan`) or a public domain, served over HTTPS, because sign-in cookies are only
  kept over HTTPS.

## 1. Get the code

```sh
git clone https://github.com/schnsrw/figcraft.git
cd figcraft
```

## 2. Create the secrets

```sh
deploy/saas/init-secrets.sh
```

This writes fresh random passwords and keys to `deploy/saas/secrets/` (ignored by Git). It refuses to
overwrite an existing set. Keep the directory private: the session signing key in it signs sign-ins
and seals stored webhook secrets. [Back it up](#backups) with everything else.

## 3. Choose the address

Copy the settings template and edit the address:

```sh
cp deploy/saas/env.example deploy/saas/.env
```

For a **trial on this computer** nothing needs to change (`http://127.0.0.1:8080`).

For a **team**, add these lines to `deploy/saas/.env`:

```sh
FIGCRAFT_DOMAIN=figcraft.office.lan
FIGCRAFT_PUBLIC_URL=https://figcraft.office.lan
# internal: for a LAN name. Caddy issues the certificate from its own authority.
# Or your email address for a public domain: the certificate is requested automatically
# (ports 80 and 443 must reach this machine).
FIGCRAFT_TLS=internal
```

and start with the HTTPS file in every command below:

```sh
export FIGCRAFT_COMPOSE="docker compose --env-file deploy/saas/.env -f deploy/saas/compose.yaml -f deploy/saas/compose.tls.yaml"
```

For the trial: `export FIGCRAFT_COMPOSE="docker compose --env-file deploy/saas/.env -f deploy/saas/compose.yaml"`.

## 4. Start it

```sh
$FIGCRAFT_COMPOSE up -d --build
$FIGCRAFT_COMPOSE ps        # every service should say healthy
```

The database is created and migrated automatically on every start.

## 5. Create the first workspace and its owner

```sh
printf '%s\n' 'choose-a-long-password' | $FIGCRAFT_COMPOSE run --rm -T cloud --setup \
  --workspace-name "Acme Design" --owner-login ada --owner-name "Ada Lovelace" \
  --password-file -
```

It prints the address to open. Leave `--password-file` out and a strong password is generated and
printed once. Running it again is safe: it only creates what is missing. The owner is a normal
account with a login name; nobody needs an email address.

## 6. Let people's browsers trust the address (LAN names only)

With `FIGCRAFT_TLS=internal` the certificate comes from Caddy's own authority, so each computer
trusts it once. Export the root certificate and install it on every machine (double-click it on
macOS and Windows and mark it trusted; on Linux add it to the system certificate store):

```sh
$FIGCRAFT_COMPOSE cp gateway:/data/caddy/pki/authorities/local/root.crt ./figcraft-root.crt
```

A public domain with a normal certificate needs none of this.

## 7. Sign in and set up the team

Open the address and sign in with the owner's login and password.

1. **Workspace → project → file.** Create a project, then a file. *Open in editor* opens the file;
   everyone who opens it joins the same live session.
2. **People.** *Add a person* with a name, a login name and a role. You get a one-time setup link
   to hand over (chat, paper, anything); the person opens it and chooses their own password.
   *Reset password link* does the same for someone who forgot theirs, *Disable* stops a person at
   once, and *Sign out everywhere* ends every session they have.
3. **Settings.** Owners and admins choose which sign-in methods the workspace allows.

## Backups

One command backs up the database, the stored files and the secrets. It stops the cloud service
for a few seconds so everything belongs to the same moment; editors reconnect by themselves.

```sh
FIGCRAFT_ENV_FILE=deploy/saas/.env deploy/saas/backup.sh /path/to/backup-folder
```

Copy the folder to another machine: it contains the secrets. Restore into a stack that has been
started once with the same secrets (this **replaces** its current data):

```sh
FIGCRAFT_ENV_FILE=deploy/saas/.env deploy/saas/restore.sh --replace /path/to/backup-folder
```

The restore checks the backup's checksums first. A restore was rehearsed on the test stack: a
workspace created after the backup disappeared and the ones from the backup came back. Practise
once on your own hardware before you rely on it. Back up before every upgrade.

## Upgrade

```sh
git pull
deploy/saas/backup.sh /path/to/backup-folder
$FIGCRAFT_COMPOSE up -d --build
```

Migrations run on start. The service refuses to start against a database newer than it knows, so
you cannot downgrade by only checking out older code; to go back, restore the backup you took.

## Limits you should know about

This is an early single-node release.

- Email is not sent yet: invitations and reset links are copied and handed over by an admin.
- Large images in a file may not sync (they travel inside edits); this is being fixed.
- Collaborators appear as "Guest 1", "Guest 2" and files open as "Untitled" in the editor; names
  from accounts are coming.
- There is no per-file history or restore yet.
- Teams that own projects, and team leads, are not finished.
- One machine, no high availability, no automatic backup schedule (run `backup.sh` from cron).
- Billing, single sign-on groups and a platform operator console are not part of the team setup.
