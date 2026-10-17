# Enterprise SAML through an OIDC identity broker

Status: **broker selected and validation procedure written; end-to-end run not yet executed**
(S4-025 partial). The [remaining steps](#remaining-steps) are listed at the end.

FigCraft Cloud speaks one login protocol, OpenID Connect (`crates/cloud-auth/src/oidc.rs`, S4-001).
Enterprise customers whose directory only speaks SAML 2.0 log in through an identity broker that
is a SAML service provider towards the customer and an OIDC provider towards FigCraft. FigCraft
never parses SAML ([product-requirements.md](product-requirements.md),
[data-tenancy-integration.md](data-tenancy-integration.md)). How the brokered identity maps to
tenants and teams is in [identity-mapping-policy.md](identity-mapping-policy.md).

## Broker choice

| Candidate | Licence | SAML IdP brokering | OIDC to us | Notes |
|---|---|---|---|---|
| **Keycloak** (selected) | Apache-2.0 | Yes: "SAML v2.0" identity provider per realm, signed assertions, metadata import, attribute and group mappers | Yes: realm is an OIDC issuer with discovery, JWKS, PKCE, back-channel logout | One container (`quay.io/keycloak/keycloak`), `start-dev` for tests, realm import/export as JSON, `kcadm.sh` for scripted setup. Can also play the customer's SAML IdP in a second realm, so one image validates both sides. |
| Dex | Apache-2.0 | SAML connector (marked unmaintained upstream) | Yes | Smaller, but the SAML connector has no logout and limited maintenance. |
| authentik | MIT (core) | Yes | Yes | Heavier stack (PostgreSQL + Redis + worker). |
| SimpleSAMLphp | LGPL-2.1 | IdP and SP, no OIDC issuer without modules | Module | Good as a *test SAML IdP* only, not as the broker. |

Keycloak is selected as the reference broker for validation. Production may use a managed broker
(the customer's choice or the operator's) as long as it meets the [contract](#broker-contract).
Nothing in the FigCraft binary depends on Keycloak.

## Broker contract

What FigCraft requires of any broker (checked by the procedure below):

1. A stable issuer URL with discovery and JWKS over HTTPS; signing with RS256 or ES256 only.
2. Authorization-code flow with PKCE S256, `state` and `nonce` round-tripped unchanged.
3. `sub` is stable for one person across logins and **not** reassigned (Keycloak: the broker
   user id, created on first brokered login, linked to the SAML `NameID` persistent id; use
   `Principal type = Subject NameID` with `NameID format = persistent`).
4. `email` is only sent with `email_verified = true` when the SAML IdP is trusted to verify email
   (Keycloak: "Trust email" on the identity provider). Otherwise `email_verified = false`.
5. Groups, when used, are sent as stable identifiers in a `groups` claim (a mapper from a SAML
   attribute), at most 1024 values.
6. Back-channel logout to FigCraft's logout endpoint when the SAML session ends (SLO), once that
   endpoint exists.
7. Key rotation keeps the old key in JWKS for at least one token lifetime after the new one
   becomes active.

## Validation topology

One disposable Keycloak container with two realms:

```
browser/test client ──OIDC──► realm "figcraft-broker" (OIDC issuer for FigCraft)
                                   │  identity provider "corp-saml" (SAML SP)
                                   ▼
                              realm "corp-idp" (plays the customer's SAML IdP)
                                   └ SAML client = the broker's SP entity id, user "alice"
```

FigCraft side: `OidcProvider::discover(OidcConfig { issuer_url:
"http://127.0.0.1:<port>/realms/figcraft-broker", client_id: "figcraft-cloud", client_secret,
redirect_url: "http://127.0.0.1:<cb>/api/v1/auth/oidc/callback", allow_insecure_loopback: true, ..
})`, then `begin_login` / `complete_login` against the dev PostgreSQL. HTTP to loopback is
allowed only with `allow_insecure_loopback`, which exists for exactly this kind of test.

## Procedure

Run only disposable containers with unique names; remove them afterwards. Pin the image by digest
in `deploy/saas/env.example` (record it in [dependency-images.md](dependency-images.md)) before the
run becomes evidence.

1. **Start the broker.**
   `docker run -d --name figcraft-kc-<rand> -p 127.0.0.1::8080 -e KC_BOOTSTRAP_ADMIN_USERNAME=admin
   -e KC_BOOTSTRAP_ADMIN_PASSWORD=<random> quay.io/keycloak/keycloak@sha256:<digest> start-dev`
   and wait for `/health/ready` (management port 9000 inside the container) or the master realm
   discovery document.
2. **Create the SAML IdP realm** (`kcadm.sh` inside the container, `config credentials` against
   `http://localhost:8080` first):
   - `create realms -s realm=corp-idp -s enabled=true`
   - `create users -r corp-idp -s username=alice -s email=alice@corp.example -s emailVerified=true
     -s enabled=true`; `set-password -r corp-idp --username alice --new-password <random>`
   - a group `designers`, alice a member.
3. **Create the broker realm and the FigCraft client:**
   - `create realms -s realm=figcraft-broker -s enabled=true`
   - `create clients -r figcraft-broker -s clientId=figcraft-cloud -s publicClient=false
     -s standardFlowEnabled=true -s 'redirectUris=["http://127.0.0.1:<cb>/*"]'
     -s 'attributes."pkce.code.challenge.method"=S256'`; read the secret with
     `get clients/<id>/client-secret`.
4. **Wire SAML between the realms:**
   - In `corp-idp`, create a SAML client whose client id is the broker SP entity id
     `http://127.0.0.1:<port>/realms/figcraft-broker`, with the redirect/ACS URL
     `http://127.0.0.1:<port>/realms/figcraft-broker/broker/corp-saml/endpoint`, signed assertions,
     `NameID format = persistent`, and a "Group list" mapper emitting attribute `memberOf`.
   - In `figcraft-broker`, create identity provider `corp-saml` (`providerId=saml`) by importing
     `http://localhost:8080/realms/corp-idp/protocol/saml/descriptor`; set "Principal type: Subject
     NameID", "Want assertions signed: on", "Validate signature: on", "Trust email: on", "Sync
     mode: force"; add an attribute importer for `memberOf` into a user attribute and a client
     scope mapper that emits it as the `groups` claim; set the realm's browser flow to redirect to
     `corp-saml` (identity provider redirector, default provider `corp-saml`).
5. **Login through the broker** (a Rust integration test in `crates/cloud-auth/tests/`, gated on
   `FIGCRAFT_TEST_KEYCLOAK_URL`): `discover`, `begin_login(pool, Some("/"))` and follow the
   authorization URL with a cookie-keeping `reqwest` client built with
   `tls_backend_preconfigured` (never `Client::new()`): broker redirect → `corp-idp` login form
   (POST username/password) → SAML POST binding auto-submit form (parse the `SAMLResponse` and
   `RelayState` hidden inputs and POST them to the ACS URL) → broker → redirect to the callback
   with `code` and `state`. Call `complete_login(pool, code, state, Some(state_cookie))`.
   Expect: a principal created on first login, `identity_links.issuer` = the broker issuer,
   `subject` = the broker user id, `verified_metadata.email` = `alice@corp.example` with
   `email_verified`, `groups` contains `designers`. Log in again: same principal (no duplicate).
6. **Logout.** End the FigCraft session (`sessions::logout`) and the broker session
   (`/protocol/openid-connect/logout` with `id_token_hint`). Expect the next `begin_login` to show
   the `corp-idp` login form again (no silent SSO). Then configure back-channel logout once the
   FigCraft endpoint exists and check that ending the `corp-idp` session (admin "sign out") revokes
   the FigCraft session.
7. **Key rotation.**
   - Broker signing key: add a new `rsa-generated` key provider with higher priority in
     `figcraft-broker`, log in again. The ID token's `kid` is unknown to the cached JWKS, so
     `OidcProvider` re-discovers once (bounded by `key_refresh_min_interval`) and accepts it.
     Disable the old key and check that a token signed with it (captured before) is refused.
   - SAML signing key: rotate the `corp-idp` realm key, re-import the descriptor in the broker (or
     enable metadata refresh), and check that login still works and that an assertion signed with
     the removed key is refused by the broker (signature validation on).
8. **Revocation.**
   - Disable alice in `corp-idp`: a new login fails at the IdP; FigCraft creates no session.
   - Disable the brokered user in `figcraft-broker`: a new login fails at the broker.
   - Suspend the FigCraft principal: `complete_login` returns `PrincipalInactive` even though the
     broker succeeds.
   - Remove alice from `designers`: on the next login the `groups` claim no longer carries it (the
     mapping consequences are in [identity-mapping-policy.md](identity-mapping-policy.md)).
9. **Tear down:** `docker rm -f figcraft-kc-<rand>`; delete the d-prefixed fixture principals.

## What has been validated already

- The OIDC half is implemented and tested against a local fake provider: discovery, PKCE, state
  and nonce binding, ID-token validation with an RS256/ES256 allowlist, unknown-`kid` re-discovery
  and `(issuer, subject)` mapping (S4-001, `crates/cloud-auth/tests/oidc_flow.rs`).
- Nothing SAML-specific has been run yet.

## Remaining steps

1. Owner decision D-003 (initial OIDC provider for end-to-end testing) should name Keycloak as the
   broker for this validation, and the image must be digest-pinned and reviewed
   ([dependency-images.md](dependency-images.md)).
2. Write the gated integration test of steps 5 to 8 (`crates/cloud-auth/tests/saml_broker.rs`)
   with a small HTML form parser for the IdP login form and the SAML POST binding (bounded input,
   no script execution), and a setup script that performs steps 1 to 4 with `kcadm.sh`.
3. Run it once on a developer machine (the Keycloak image is about 450 MB and was not pulled for
   this change) and record the result in the tracker.
4. Add FigCraft's back-channel logout endpoint (OIDC Back-Channel Logout 1.0: verify the logout
   token's signature, `iss`, `aud`, `events` and `sid`/`sub`, then revoke the matching sessions)
   and validate step 6's back-channel part.
5. Wire the group claim into the mapping of [identity-mapping-policy.md](identity-mapping-policy.md)
   once its decisions are confirmed.
