# Enterprise SAML through an OIDC identity broker

Status: **validated end to end against Keycloak 26.4.2 on 2026-10-17** (login, logout, broker and
SAML key rotation, revocation, OIDC back-channel logout ending FigCraft sessions, the SAML group
reaching FigCraft as a bounded `groups` claim; `crates/cloud-auth/tests/keycloak_saml.rs`). The
image is pinned by digest as a test-only image. What remains is listed under
[remaining steps](#remaining-steps).

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
5. Groups, when used, are sent as stable identifiers in a `groups` ID-token claim (an array of
   strings; a mapper from a SAML attribute). FigCraft keeps at most 32 names of at most 256
   characters each (and only as many as fit the 4 KiB stored-claims budget), records
   `groups_truncated` when it drops any, and grants nothing from them.
6. OIDC Back-Channel Logout 1.0 to `POST /api/v1/auth/backchannel-logout` when the broker
   session ends (including SAML SLO from the IdP), with `sub` in the logout token (see
   [FigCraft's back-channel logout endpoint](#figcrafts-back-channel-logout-endpoint)).
7. Key rotation keeps the old key in JWKS for at least one token lifetime after the new one
   becomes active.

## FigCraft's back-channel logout endpoint

`POST /api/v1/auth/backchannel-logout`, form body `logout_token=<JWT>`
(`apps/figcraft-cloud/src/api/auth_routes.rs`, validation in
`OidcProvider::backchannel_logout`, `crates/cloud-auth/src/oidc.rs`). Register it with the broker
as the client's back-channel logout URL (Keycloak: "Backchannel logout URL", "Backchannel logout
session required" on).

- Server to server: no cookies, no same-origin check, served while bootstrap is pending, rate
  limited with the other auth routes. `Content-Type` must be
  `application/x-www-form-urlencoded`; the body is capped at 32 KiB and must hold exactly one
  `logout_token` of at most 16 KiB.
- The token is a JWS signed by a key in the issuer's JWKS (RS256 or ES256 only, no `crit`; an
  unknown `kid` triggers the same rate-limited one-time key refresh as ID tokens). `iss` is the
  configured issuer; `aud` is our client id (a string, or an array holding only it); `events`
  holds `http://schemas.openid.net/event/backchannel-logout` with an object value; `iat` is at
  most 2 minutes old and not in the future (plus the configured skew, default 60 s); `exp` and
  `nbf`, when present, hold; `nonce` is absent; `jti` is present (at most 256 characters); `sub`
  and/or `sid` are present.
- Replay: each `(issuer, jti)` is remembered (SHA-256, in memory per process, at most 10,000
  entries) until the token could no longer pass the `iat` check; a second use is refused. A full
  cache of live entries refuses new tokens (fail closed; the broker retries). Across several API
  replicas a replay to another replica is accepted, which at worst revokes the same sessions
  again within the 2-minute freshness window.
- Mapping: `sub` → `identity_links (issuer, subject)` → principal; every unrevoked session of
  that principal is revoked with reason `logout` (`sessions::revoke_all_for_identity`). Sessions
  are not tied to the issuer or the provider session that created them, so `sid` is validated
  but not used to pick sessions, and no migration was needed. A token with only `sid` is refused
  (`400`), so brokers must include `sub` (Keycloak always does).
- Answers: `200` (empty, `Cache-Control: no-store`) for every valid token, whether or not the
  identity exists or had sessions; `400 invalid_request` for any invalid, replayed or `sub`-less
  token; `503` when no OIDC provider is configured or the store is unreachable (the `jti` is
  forgotten again so the broker's retry is accepted). Nothing about the token is logged.

## Validation topology

One disposable Keycloak container with two realms:

```
test client (FigCraft OidcProvider + cookie-keeping reqwest "browser")
   │ OIDC (code + PKCE S256)
   ▼
realm "figcraft-broker"  ── OIDC issuer for FigCraft, client "figcraft-cloud" (confidential)
   │  identity provider "corp-saml" (SAML SP; browser flow redirects straight to it)
   │  SAML AuthnRequest: redirect binding; SAML Response: POST binding, signed assertion
   ▼
realm "corp-idp"         ── plays the customer's SAML IdP; SAML client = the broker's SP
                            entity id; user "alice" (group "designers")
```

FigCraft side: `OidcProvider::discover(OidcConfig { allow_insecure_loopback: true,
..OidcConfig::new("http://127.0.0.1:<port>/realms/figcraft-broker", "figcraft-cloud",
Some(secret), "http://127.0.0.1:47123/api/v1/auth/oidc/callback") })`, then `begin_login` /
`complete_login` against the dev PostgreSQL. Nothing listens on the callback port: the test's
browser stops at the redirect to it and hands `code` and `state` to `complete_login`. HTTP to
loopback is allowed only with `allow_insecure_loopback`, which exists for exactly this kind of
test.

## Validation run (2026-10-17)

Executed end to end on the owner's development machine; every step below passed, on consecutive runs
against the same container (the test resets the realms it changes, so reruns pass).

| Item | Value |
|---|---|
| Broker image | `quay.io/keycloak/keycloak:26.4.2`, `sha256:3617b09bb4b7510a8d8d9b9fc5707399e2d70688dbcc2f8fb013a144829be1b9` (multi-arch index; ran linux/arm64, 720 MB unpacked), Keycloak 26.4.2 on OpenJDK 21.0.9 |
| Container runtime | Docker Engine 29.8.0, macOS (Darwin 25.3.0) |
| Mode | `start-dev` (H2 in the container, plain HTTP on a random loopback port) |
| Test | `crates/cloud-auth/tests/keycloak_saml.rs` (`saml_federation_through_the_keycloak_broker`), about 4 to 10 s per run |
| Database | the shared dev PostgreSQL (runtime role with row-level security on; admin role only to suspend the principal) |

### 1. Start the broker

The image is pinned by digest (`KEYCLOAK_TEST_IMAGE` in `deploy/saas/env.example`,
[dependency-images.md](dependency-images.md)):

```sh
IMAGE=quay.io/keycloak/keycloak:26.4.2@sha256:3617b09bb4b7510a8d8d9b9fc5707399e2d70688dbcc2f8fb013a144829be1b9
docker pull "$IMAGE"
docker run -d --name figcraft-c4-keycloak -p 127.0.0.1::8080 \
  -e KC_BOOTSTRAP_ADMIN_USERNAME=admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=<random> \
  "$IMAGE" start-dev
docker port figcraft-c4-keycloak 8080        # → 127.0.0.1:<port>
```

Ready when `http://127.0.0.1:<port>/realms/master/.well-known/openid-configuration` answers 200
(about 10 s).

### 2. Configure the realms (admin REST API, no UI)

A setup script (Python standard library only, run from the host) gets an admin token with
`POST /realms/master/protocol/openid-connect/token` (`grant_type=password`,
`client_id=admin-cli`) and sends these requests to `/admin/realms` (`KC` =
`http://127.0.0.1:<port>`):

| Request | Body (essentials) |
|---|---|
| `POST /admin/realms` | `{"realm":"corp-idp","enabled":true}` |
| `POST /corp-idp/groups` | `{"name":"designers"}` |
| `POST /corp-idp/users` | `{"username":"alice","email":"alice@corp.example","emailVerified":true,"enabled":true,"firstName":"e4-alice","lastName":"Corp"}` |
| `PUT /corp-idp/users/<id>/reset-password` | `{"type":"password","value":<random>,"temporary":false}` |
| `PUT /corp-idp/users/<id>/groups/<designers>` | – |
| `POST /corp-idp/clients` | `clientId` = `KC/realms/figcraft-broker` (the SP entity id), `protocol: saml`, `redirectUris` = `[ACS]` with ACS = `KC/realms/figcraft-broker/broker/corp-saml/endpoint`; attributes `saml_assertion_consumer_url_post=ACS`, `saml.assertion.signature=true`, `saml.server.signature=true`, `saml.signature.algorithm=RSA_SHA256`, `saml.client.signature=false`, `saml_name_id_format=persistent`, `saml.force.name.id.format=true`; protocol mappers `saml-user-property-mapper` for `email`, `firstName`, `lastName` and `saml-group-membership-mapper` → attribute `memberOf` |
| `GET /corp-idp/keys` | read the active RS256 `certificate` |
| `POST /admin/realms` | `{"realm":"figcraft-broker","enabled":true}` |
| `POST /figcraft-broker/clients` | `{"clientId":"figcraft-cloud","publicClient":false,"standardFlowEnabled":true,"directAccessGrantsEnabled":false,"implicitFlowEnabled":false,"redirectUris":["http://127.0.0.1:47123/api/v1/auth/oidc/callback"],"attributes":{"pkce.code.challenge.method":"S256"}}`; then `GET …/clients/<id>/client-secret` |
| `PUT /figcraft-broker/authentication/config/<review-profile config>` | the built-in "first broker login" flow's `idp-review-profile` execution: `update.profile.on.first.login=off` (the assertion carries the profile; "Create User If Unique" then creates the brokered user) |
| `POST /figcraft-broker/identity-provider/instances` | `alias: corp-saml`, `providerId: saml`, `trustEmail: true`, `firstBrokerLoginFlowAlias: "first broker login"`, config `entityId=KC/realms/figcraft-broker`, `idpEntityId=KC/realms/corp-idp`, `singleSignOnServiceUrl` and `singleLogoutServiceUrl` = `KC/realms/corp-idp/protocol/saml`, `nameIDPolicyFormat=urn:oasis:names:tc:SAML:2.0:nameid-format:persistent`, `principalType=SUBJECT`, `postBindingResponse=true`, `postBindingAuthnRequest=false`, `wantAssertionsSigned=true`, `validateSignature=true`, `signingCertificate=<corp-idp cert>`, `syncMode=FORCE` |
| `POST /figcraft-broker/identity-provider/instances/corp-saml/mappers` | `saml-user-attribute-idp-mapper` for `email`, `firstName`, `lastName`, and `memberOf` → user attribute `groups` |
| `POST /figcraft-broker/authentication/executions/<identity-provider-redirector>/config` | `{"alias":"to-corp-saml","config":{"defaultProvider":"corp-saml"}}` (browser flow goes straight to the SAML IdP) |

The test itself then configures the `figcraft-cloud` client (idempotent): a protocol mapper
`e4-groups` (`oidc-usermodel-attribute-mapper`, user attribute `groups` → multivalued ID-token
claim `groups`), `frontchannelLogout = false`, and the attributes `backchannel.logout.url` (its
loopback listener, reached from the container as `http://host.docker.internal:<port>/…`;
override with `FIGCRAFT_TEST_KEYCLOAK_BACKCHANNEL_HOST` and `FIGCRAFT_TEST_KEYCLOAK_BACKCHANNEL_BIND`)
and `backchannel.logout.session.required = true`.

The descriptor is not imported with `fromUrl`: inside the container the IdP is
`localhost:8080`, while the browser reaches it on the published port, so the SSO URL and the
signing certificate are set explicitly. The script writes the issuer, client secret, alice's
password and the admin password to a mode-600 JSON file outside the repository.

### 3. Run the test

```sh
FIGCRAFT_TEST_KEYCLOAK_URL=http://127.0.0.1:<port> \
FIGCRAFT_TEST_KEYCLOAK_SETUP=<setup.json> \
FIGCRAFT_TEST_DATABASE_URL_FILE=<runtime url file> \
FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE=<admin url file> \
cargo test -p figcraft-cloud-auth --test keycloak_saml -- --nocapture --test-threads=1
```

Without `FIGCRAFT_TEST_KEYCLOAK_URL` the test prints a skip line and passes, so it costs nothing
in CI. The test's browser is a `reqwest` client built with `tls_backend_preconfigured` (explicit
rustls/ring configuration), no automatic redirects, no proxy, bodies capped at 512 KiB, at most
20 steps per login and never leaving the broker's origin. It keeps cookies per path (both realms
set same-named cookies under `/realms/<realm>/`; the `Secure` flag is ignored on loopback HTTP),
submits the `kc-form-login` form with alice's credentials once, and auto-submits any page
carrying `SAMLRequest`/`SAMLResponse` hidden inputs (the POST binding). No script runs.

### 4. What the test proves

1. **Login.** `begin_login` → broker → SAML AuthnRequest → `corp-idp` password form → signed SAML
   Response by POST → broker ACS → first broker login creates the user → callback →
   `complete_login` creates a principal. `identity_links.issuer` is the broker issuer,
   `subject` is the broker user id, that user's federated identity is `corp-saml` with a
   persistent NameID (`G-…`), `verified_metadata` has `email = alice@corp.example` with
   `email_verified = true` (trust email), `groups = ["designers"]` (corp-idp group → SAML
   `memberOf` → broker user attribute → ID-token `groups` claim; no `groups_truncated`), and the
   principal is named `e4-alice Corp`.
2. **SSO and no duplicates.** A second login in the same browser completes without the IdP and
   resolves to the same principal (`new_principal = false`).
3. **Logout.** `sessions::logout` ends a FigCraft session (it no longer resolves). Ending only
   the broker session (admin sign-out) makes the next login run SAML again but silently on the
   IdP session; ending the `corp-idp` session too brings back the IdP password form. Same
   principal each time.
   **Back-channel logout:** with two live FigCraft sessions for the principal, ending the broker
   session (admin sign-out) makes Keycloak POST a logout token (RS256, `sub` and `sid`) to the
   test's listener, which hands it to `OidcProvider::backchannel_logout`: it returns 2 revoked,
   neither session resolves any more, both carry `revoke_reason = logout`, and submitting the same
   logout token again is refused (`InvalidLogoutToken`, replayed `jti`). The listener stands in
   for `POST /api/v1/auth/backchannel-logout`, whose HTTP behaviour (form parsing, `200`/`400`/
   `503`, nothing revealed) is covered with an in-test ES256 provider in
   `apps/figcraft-cloud/src/api/auth_routes_tests.rs`.
4. **Broker key rotation.** A new `rsa-generated` key provider with priority 1000 becomes the
   active RS256 key; its `kid` is not in the JWKS the provider cached at discovery. The next
   login with the *same* `OidcProvider` succeeds, which needs the unknown-`kid` JWKS refresh
   (without it the verifier fails with no matching key, as `oidc_flow.rs` shows).
5. **SAML signing-key rotation.** Rotating the `corp-idp` key makes the broker refuse the next
   assertion (Keycloak event `IDENTITY_PROVIDER_RESPONSE_ERROR`, `invalid_signature`; no callback,
   no FigCraft login) until the broker's `signingCertificate` is updated to the new certificate;
   then login works again for the same principal.
6. **Revocation.** Alice disabled at `corp-idp`: the IdP shows "Account is disabled", no SAML
   response, no callback. The brokered user disabled at `figcraft-broker`: the SAML response
   arrives but the broker refuses (`user_disabled`), no callback. The FigCraft principal
   suspended: the broker succeeds but `complete_login` returns `PrincipalInactive`.

### 5. Tear down

```sh
docker rm -f figcraft-c4-keycloak
docker rmi "$IMAGE"
```

The back-channel logout and groups run (2026-10-17, same digest, container
`figcraft-g4-keycloak`) passed twice in a row against one container (7.1 s and 2.3 s), then the
container and image were removed.

Fixture principals are named `e4-alice Corp` under the container's issuer
(`http://127.0.0.1:<port>/realms/figcraft-broker`, unique per container port); they are left
suspended and are never matched by a later container.

## What has been validated

- The OIDC half against a local fake provider: discovery, PKCE, state and nonce binding,
  ID-token validation with an RS256/ES256 allowlist, unknown-`kid` re-discovery and
  `(issuer, subject)` mapping (S4-001, `crates/cloud-auth/tests/oidc_flow.rs`).
- The brokered SAML path end to end against Keycloak 26.4.2 (above): login, SSO, logout of the
  FigCraft, broker and IdP sessions, back-channel logout from the broker ending FigCraft
  sessions, the SAML group as a recorded `groups` claim, broker and SAML key rotation,
  revocation at all three layers.
- Logout-token validation offline with an in-test ES256 signer (`oidc.rs` unit tests): wrong,
  extra, empty or missing `aud`, wrong `iss`, missing or wrong `events`, old, future, missing or
  non-numeric `iat`, expired `exp`, future `nbf`, `nonce` present, missing, empty or over-long
  `jti`, neither `sub` nor `sid`, HS256, `none`, `crit`, another key, a tampered payload, extra
  segments and over-long tokens are all refused; an unknown `kid` asks for one key refresh; the
  replay cache refuses a second use, keys by issuer, forgets on storage failure and stays
  bounded.

## Remaining steps

1. **Group binding.** The `groups` claim is recorded in `identity_links.verified_metadata` only.
   Binding groups to tenants or teams is the directory work of
   [identity-mapping-policy.md](identity-mapping-policy.md) (D5/D6); nothing is granted from it.
2. **Replay protection across replicas.** The `jti` cache is per process; a shared store
   (PostgreSQL) would make a replay to another replica fail too. Low impact (a replay only
   revokes the same principal's sessions again within 2 minutes).
3. **Overlapping keys.** The run checks that a new broker key is picked up; it does not capture
   an ID token signed by the old key and check that it is refused after the old key is disabled
   (needs the raw token, which `complete_login` does not expose). The SAML rotation is a hard
   cut-over in this run; production should list both certificates (comma-separated
   `signingCertificate`) or use metadata refresh during the overlap.
4. **Setup script in the repository.** The digest is pinned (`deploy/saas/env.example`,
   [dependency-images.md](dependency-images.md)); the setup script described in step 2 still
   lives outside the repository and should move next to the deploy files before this run is
   repeated in CI.
5. **SP-initiated SLO through the end-session endpoint** (`id_token_hint`) instead of admin
   sign-out, once FigCraft keeps the ID token for that purpose.
