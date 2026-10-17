# External identity mapping policy

Status: **defined, pending owner confirmation** (S4-024). Every decision the owner must confirm is
marked **[OWNER DECISION Dn]** and listed again in [Decisions to confirm](#decisions-to-confirm).
Until a decision is confirmed, the default written next to it is what the code does or will do.

This document says how an identity asserted by an external identity provider (OIDC directly, or
SAML through the broker described in [saml-federation.md](saml-federation.md)) becomes a FigCraft
principal, and how organisations, verified email domains and group claims may map that principal
into tenants and teams. It refines the "Enterprise federation and provisioning" rules in
[data-tenancy-integration.md](data-tenancy-integration.md) and the authorization model in
[security.md](security.md).

## Principles

1. **Identity is `(issuer, subject)`, nothing else.** The only key that names a person is the pair
   of the provider's exact issuer URL and the token's `sub` claim (`identity_links`, primary key
   `(issuer, subject)`; `cloud-auth::oidc` and `cloud-storage::oidc_logins::resolve_identity`).
   Email, name, username, `oid`/`upn`, SAML `NameID` and every other claim are attributes, stored
   in `identity_links.verified_metadata` for display and audit; they never select a principal.
2. **Claims never grant directly.** No claim can make anyone a tenant member, owner or admin by
   itself. A claim can only *match* a mapping that a tenant administrator configured in FigCraft,
   and the mapping (with its limits) is what grants. A tenant identifier, slug or role in a token
   is ignored.
3. **Deny by default.** A principal with no membership sees no tenant. Every mapping rule below is
   off until a tenant administrator turns it on for their own tenant.
4. **One tenant's configuration never affects another.** Mappings live in the tenant (tenant-owned
   tables under row-level security, S4-014) and only ever add or remove memberships in that tenant.
5. **Every automatic change is audited** with the rule that caused it, the claim values that
   matched (not the token), the issuer and the login's request id.

## External identities

| Case | Behaviour |
|---|---|
| First login with a new `(issuer, subject)` | A new `user` principal is created with the token's display name, and the link is stored (implemented, S4-001). The principal has no memberships. |
| Same person, new provider | A new `(issuer, subject)` is a new principal. Linking two identities to one principal is an explicit, authenticated action by the person (sign in with both in one session), never automatic by matching email. **[OWNER DECISION D1]** default: account linking is not offered until a later milestone; support merges principals on request with audit. |
| Provider reissues a subject (tenant moved, user recreated) | Treated as a new person. The old principal keeps its history and is deprovisioned by the rules below. |
| Email changes at the provider | Only the stored attribute changes. Memberships are unaffected. Invitations already accepted stay accepted. |
| Unverified email | Stored as unverified; never used for invitation matching or domain rules. `email_verified` must be `true` (OIDC) or the broker must assert verification for the realm (SAML). |
| Principal suspended or deleted in FigCraft | Login is refused (`PrincipalInactive`) whatever the provider says. |

Allowed issuers are configured by the operator (`OIDC_ISSUER_URL` today; a list later). An issuer
that is not configured cannot log in at all, so a tenant cannot bring an arbitrary provider without
operator review. **[OWNER DECISION D2]** default: per-tenant ("bring your own IdP") issuers are
added by the operator after review, not self-served by tenant admins.

## Organisations

An *organisation* claim (Keycloak organisations, Entra `tid`, Google `hd`, Okta org, a SAML
`Organization` attribute) is metadata about the provider's directory. It is handled exactly like a
group claim: a tenant may configure "issuer X, organisation claim value Y" as a **directory
binding**, and the binding is a precondition for domain and group rules (below). An organisation
claim never creates a tenant and never names one.

- A directory binding is `(tenant, issuer, claim name, claim value)`; one `(issuer, claim value)`
  binds at most one tenant (unique), so two tenants can't both claim the same directory.
- Creating a binding requires `tenant.update_policy` and, **[OWNER DECISION D3]** default, an
  operator approval (support ticket) for the first binding of each tenant, because it lets a
  directory's users join automatically.

## Verified email domains

A tenant may verify that it controls a domain (DNS TXT record `figcraft-verify=<random token>` on
`_figcraft.<domain>`, re-checked daily; a domain whose record disappears for 7 days becomes
unverified). One domain is verified by at most one tenant.

| Rule | Default |
|---|---|
| Domain verification available | On (it grants nothing by itself). |
| **Auto-join by verified domain** | **Off.** **[OWNER DECISION D4]** whether to offer it at all. If offered, a tenant admin turns it on per domain. |
| Role on auto-join | `viewer` tenant role, no team, no workspace access. Never higher, whatever the configuration. |
| Who auto-joins | A principal whose verified email is at the domain, *and* whose issuer is bound to the tenant (directory binding), *and* (when the tenant requires it) whose organisation claim matches. Email domain alone from a consumer provider (for example a personal Google account with a matching address) is not enough. |
| Domain-restricted invitations | A tenant may require that invitations be accepted only by verified emails at its domains (enforced in `accept_invitation`, which already matches the verified email to the invited address). |
| Discovery ("your colleagues use FigCraft") | Off. Never reveals that a tenant exists for a domain. |

## Group-claim-to-team mapping (proposal)

Groups come from the provider (`groups` claim, Entra group object ids, Keycloak group paths, SAML
`memberOf` via the broker). The proposal is a tenant-owned table, managed through the API by
`member.manage` holders, applied at login and by SCIM later:

```sql
CREATE TABLE team_group_mappings (
    tenant_id UUID NOT NULL REFERENCES tenants (id),
    id UUID NOT NULL,
    issuer TEXT NOT NULL,                 -- must be an issuer bound to the tenant
    claim_name TEXT NOT NULL,             -- e.g. 'groups'
    claim_value TEXT NOT NULL,            -- exact match, case-sensitive; no patterns
    team_id UUID NOT NULL,                -- team in the same tenant
    team_role TEXT NOT NULL CHECK (team_role IN ('editor', 'commenter', 'viewer')),
    created_by UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, issuer, claim_name, claim_value, team_id),
    FOREIGN KEY (tenant_id, team_id) REFERENCES teams (tenant_id, id)
);
```

Rules:

- Matching is exact string equality on stable identifiers (prefer group object ids over display
  names). No wildcards, regular expressions or prefix matches.
- A mapping can grant at most `editor` in a team. `team_manager`, every tenant role above `viewer`,
  and every sensitive permission (billing, provider keys, support, export, deletion) are **never**
  granted by a claim; they are always granted by a person with the right to grant them.
  **[OWNER DECISION D5]** default: tenant `tenant_admin`/`tenant_owner` cannot be mapped from groups.
- A mapped team membership is recorded with its source (`source = 'group:<mapping id>'`) so that
  mapped and manual memberships can be told apart. A manual membership is never removed by
  mapping reconciliation.
- At most 1024 groups are read from one token and at most 256 mappings are evaluated per tenant;
  an oversized `groups` claim is truncated and the event audited (providers that send group
  overage indicators are treated as "groups unknown": no mapped change is made at that login).
- Group mapping requires the principal to already be (or by the domain rule become) a tenant
  member; a mapping never creates a tenant membership on its own. **[OWNER DECISION D6]** default:
  groups require membership first; the alternative (a mapping may also add `viewer` tenant
  membership) is only allowed together with a directory binding.

### Reconciliation

At each login (and later each SCIM push) the mapped team memberships of the principal in that
tenant are reconciled with the current claims: missing mapped memberships are added, mapped
memberships whose group is no longer present are removed, and role changes take effect. When a
removal lowers effective access, the principal's live sessions and realtime connections in that
tenant are closed through the revocation path (S4-017). Reconciliation is idempotent and runs in
one tenant transaction.

## Conflicts

| Conflict | Resolution |
|---|---|
| Manual role and mapped role on the same team | The stronger applies (effective roles already take the strongest source). Removing the mapping leaves the manual one. |
| Two mappings put one person in one team with different roles | The stronger applies. |
| Mapping says "remove" but a manual grant exists | Only the mapped membership row is removed. |
| Group mapping points at an archived team | Ignored and audited; nothing is added. |
| Two tenants verify or bind the same domain or directory | Impossible (unique); the second request is refused with `conflict` and the operator resolves disputes. |
| A claim names a tenant or role | Ignored. |
| The provider is down or returns no groups | No mapped change at that login (absence of a claim is not "remove everything"), except under SCIM, which is authoritative. |
| Last tenant owner would be removed | Never by mapping (owners are never mapped); `LastOwner` protection stays. |

## Deprovisioning

| Trigger | Effect |
|---|---|
| Provider disables the user (login refused upstream) | New logins fail. Existing sessions continue until their idle/absolute expiry unless one of the triggers below fires. **[OWNER DECISION D7]** default idle timeout 30 minutes for federated tenants. |
| Broker back-channel logout / SAML SLO | The broker ends its session; FigCraft sessions created from it are revoked when back-channel logout is wired (S4-025 remaining step). |
| SCIM `active=false` or delete (later) | Tenant membership suspended; sessions revoked with `membership_revoked`; share links the person created stay (they belong to the document) unless the tenant policy says otherwise. |
| Group removal | Mapped team membership removed at the next login or SCIM push, sessions closed as above. |
| Tenant admin removes the member | Immediate, as today (`remove_tenant_member`, then session revocation, S4-017). |
| Directory binding removed | Mapped memberships created through it are removed; manual memberships stay. |

Pending privileged work (exports, jobs) started by a deprovisioned principal is re-authorized
before commit (S4-013) and fails closed.

## Decisions to confirm

| ID | Question | Default until confirmed |
|---|---|---|
| D1 | Offer self-service account linking across providers? | No; support-mediated merge only. |
| D2 | May tenant admins self-serve their own OIDC/SAML issuers? | No; operator adds issuers after review. |
| D3 | Does the first directory binding of a tenant need operator approval? | Yes. |
| D4 | Offer auto-join by verified domain at all? | Offer, off by default per tenant, `viewer` only, requires a directory binding. |
| D5 | Can any tenant role above `viewer` be mapped from groups? | No. |
| D6 | Can a group mapping create tenant membership? | No (only with a directory binding, as `viewer`). |
| D7 | Idle session timeout for federated tenants | 30 minutes, absolute 12 hours. |

## Implementation status

Implemented: `(issuer, subject)` principal mapping, verified metadata storage, invitation email
matching (S4-001, S4-009). Not implemented: directory bindings, domain verification, auto-join,
`team_group_mappings`, reconciliation and SCIM. They are scheduled after the owner confirms the
decisions above.
