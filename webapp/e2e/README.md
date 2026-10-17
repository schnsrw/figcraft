# Real-browser smoke run (TS-003)

Drives the installed Google Chrome (no browser download) through the whole self-host path against a
running stack: first-run owner setup, sign-in, workspace, project, file, "Open in editor", then two
sessions editing the same file. Screenshots go to `E2E_SHOTS` (default `./shots`).

```sh
cd webapp/e2e && npm install
E2E_BASE=http://127.0.0.1:8080 E2E_SETUP_TOKEN=<token from --print-bootstrap-token> npm run smoke
```

`E2E_OWNER_LOGIN` (default `owner`) and `E2E_OWNER_PASSWORD` (generated and printed once when unset)
are the owner account the run creates or signs in with. The run creates its own workspace named
`E2E <time>`, so it can be repeated on the same stack. Test code only: nothing here ships.
