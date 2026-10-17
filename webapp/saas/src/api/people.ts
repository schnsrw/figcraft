import type { ApiClient } from "./client";
import { loadAll } from "./workspaces";

export interface Member {
  principal_id: string;
  display_name: string;
  role: string;
  state: string;
}

export interface Team {
  id: string;
  slug: string;
  name: string;
  state: string;
}

export interface TeamMember {
  principal_id: string;
  display_name: string;
  role: string;
}

export interface Invitation {
  id: string;
  team_id: string;
  email: string;
  role: string;
  invited_by: string;
  expires_in_secs: number;
}

export interface CreatedInvitation {
  id: string;
  /** Shown once; only its hash is stored. */
  token: string;
}

export interface AcceptedInvitation {
  tenant_id: string;
  team_id: string;
  team_role: string;
  joined_tenant: boolean;
}

export interface Grant {
  principal_id?: string;
  team_id?: string;
  role: string;
}

export interface ShareLink {
  id: string;
  role: "editor" | "viewer";
  label: string | null;
  created_by: string;
  created_at: number;
  expires_at: number | null;
  revoked_at: number | null;
}

export interface CreatedShareLink {
  id: string;
  role: "editor" | "viewer";
  /** Shown once. */
  secret: string;
}

/** Tenant roles a person can hold, strongest first. */
export const TENANT_ROLES = [
  "tenant_owner",
  "tenant_admin",
  "editor",
  "commenter",
  "viewer",
] as const;

export const TEAM_ROLES = [
  "team_manager",
  "editor",
  "commenter",
  "viewer",
] as const;

/** Roles a document can be shared at (direct members and team grants). */
export const DOCUMENT_ROLES = ["editor", "commenter", "viewer"] as const;

export const people = {
  members: (client: ApiClient, tenant: string) =>
    loadAll<Member>(client, `/tenants/${tenant}/members`),
  setMember: (
    client: ApiClient,
    tenant: string,
    principal: string,
    change: { role?: string; state?: string },
  ) => client.patch<void>(`/tenants/${tenant}/members/${principal}`, change),
  removeMember: (client: ApiClient, tenant: string, principal: string) =>
    client.delete<void>(`/tenants/${tenant}/members/${principal}`),

  teams: (client: ApiClient, tenant: string) =>
    loadAll<Team>(client, `/tenants/${tenant}/teams`),
  createTeam: (client: ApiClient, tenant: string, name: string, slug: string) =>
    client.post<Team>(`/tenants/${tenant}/teams`, { name, slug }),
  renameTeam: (client: ApiClient, tenant: string, team: string, name: string) =>
    client.patch<Team>(`/tenants/${tenant}/teams/${team}`, { name }),
  archiveTeam: (client: ApiClient, tenant: string, team: string) =>
    client.patch<Team>(`/tenants/${tenant}/teams/${team}`, {
      state: "archived",
    }),
  teamMembers: (client: ApiClient, tenant: string, team: string) =>
    loadAll<TeamMember>(client, `/tenants/${tenant}/teams/${team}/members`),
  addTeamMember: (
    client: ApiClient,
    tenant: string,
    team: string,
    principal: string,
    role: string,
  ) =>
    client.post<void>(`/tenants/${tenant}/teams/${team}/members`, {
      principal_id: principal,
      role,
    }),
  setTeamMemberRole: (
    client: ApiClient,
    tenant: string,
    team: string,
    principal: string,
    role: string,
  ) =>
    client.patch<void>(
      `/tenants/${tenant}/teams/${team}/members/${principal}`,
      { role },
    ),
  removeTeamMember: (
    client: ApiClient,
    tenant: string,
    team: string,
    principal: string,
  ) =>
    client.delete<void>(
      `/tenants/${tenant}/teams/${team}/members/${principal}`,
    ),

  invitations: (client: ApiClient, tenant: string) =>
    loadAll<Invitation>(client, `/tenants/${tenant}/invitations`),
  invite: (
    client: ApiClient,
    tenant: string,
    team: string,
    email: string,
    role: string,
    expiresInSecs?: number,
  ) =>
    client.post<CreatedInvitation>(
      `/tenants/${tenant}/teams/${team}/invitations`,
      expiresInSecs === undefined
        ? { email, role }
        : { email, role, expires_in_secs: expiresInSecs },
    ),
  revokeInvitation: (
    client: ApiClient,
    tenant: string,
    team: string,
    invitation: string,
  ) =>
    client.delete<void>(
      `/tenants/${tenant}/teams/${team}/invitations/${invitation}`,
    ),
  accept: (client: ApiClient, token: string) =>
    client.post<AcceptedInvitation>("/invitations/accept", { token }),
};

export const sharing = {
  members: (client: ApiClient, document: string) =>
    loadAll<Grant>(client, `/documents/${document}/members`),
  setMember: (
    client: ApiClient,
    document: string,
    principal: string,
    role: string,
  ) =>
    client.put<Grant>(`/documents/${document}/members/${principal}`, { role }),
  removeMember: (client: ApiClient, document: string, principal: string) =>
    client.delete<void>(`/documents/${document}/members/${principal}`),
  teamGrants: (client: ApiClient, document: string) =>
    loadAll<Grant>(client, `/documents/${document}/team-grants`),
  setTeamGrant: (
    client: ApiClient,
    document: string,
    team: string,
    role: string,
  ) =>
    client.put<Grant>(`/documents/${document}/team-grants/${team}`, { role }),
  removeTeamGrant: (client: ApiClient, document: string, team: string) =>
    client.delete<void>(`/documents/${document}/team-grants/${team}`),
  links: (client: ApiClient, document: string) =>
    client.get<{ items: ShareLink[] }>(`/documents/${document}/share-links`),
  createLink: (
    client: ApiClient,
    document: string,
    role: "editor" | "viewer",
    label: string | null,
    expiresInSecs: number | null,
  ) =>
    client.post<CreatedShareLink>(`/documents/${document}/share-links`, {
      role,
      ...(label === null ? {} : { label }),
      ...(expiresInSecs === null ? {} : { expires_in_secs: expiresInSecs }),
    }),
  revokeLink: (client: ApiClient, document: string, link: string) =>
    client.delete<void>(`/documents/${document}/share-links/${link}`),
};

/** The relay's WebSocket path (kept in step with the server's `RELAY_PATH`). */
export const RELAY_PATH = "/ws/v1/collab";

/**
 * The address that opens the editor on a document through a share secret:
 * `<origin>/edit/#join=<encoded ws(s) invite>`, the same shape the server builds for collaboration
 * links.
 */
export function shareEditorUrl(
  origin: string,
  documentId: string,
  secret: string,
): string | null {
  let base: URL;
  try {
    base = new URL(origin);
  } catch {
    return null;
  }
  const socket =
    base.protocol === "https:"
      ? "wss:"
      : base.protocol === "http:"
        ? "ws:"
        : null;
  if (socket === null) return null;
  const invite = `${socket}//${base.host}${RELAY_PATH}?file=${documentId}&token=${secret}`;
  return `${base.origin}/edit/#join=${encodeURIComponent(invite)}`;
}

/** A readable invitation link for the person who was invited. The token stays in the fragment. */
export function invitationLink(origin: string, token: string): string {
  return `${origin}/invite#token=${encodeURIComponent(token)}`;
}
