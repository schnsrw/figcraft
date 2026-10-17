/** Shapes of the `/api/v1` responses this application reads. Only fields it uses are listed. */

export type Deployment = "single" | "multi" | "embedded";

export type TenantRole =
  "tenant_owner" | "tenant_admin" | "editor" | "commenter" | "viewer";

export interface SessionTenant {
  id: string;
  role: string | null;
  permissions: string[];
}

export interface SessionInfo {
  credential: "browser_session" | "service_account" | "share_link";
  principal_id: string | null;
  tenant: SessionTenant | null;
  session_expires_in?: number;
  bootstrap_required: boolean;
  /** An administrator set the password: the session may only change it until then. */
  must_change_password?: boolean;
  deployment: Deployment;
}

export interface TenantSummary {
  id: string;
  slug: string;
  name: string;
  state: string;
  role?: string;
}

export interface Page<T> {
  items: T[];
  next_cursor: string | null;
}

export type ResourceState =
  "pending" | "active" | "suspended" | "archived" | "deleting" | "deleted";

export interface Workspace {
  id: string;
  name: string;
  state: ResourceState;
  owning_team_id: string | null;
}

export interface Project {
  id: string;
  workspace_id: string;
  name: string;
  state: ResourceState;
}

export interface CloudDocument {
  id: string;
  workspace_id: string;
  project_id: string;
  name: string;
  state: ResourceState;
  /** Present on a single-document read: the caller's role there and whether they can edit. */
  role?: string | null;
  can_edit?: boolean;
  /** Unix seconds of the last edit, rename or state change, when the server reports it. */
  updated_at?: number;
}

export interface CollabLink {
  document_id: string;
  access: "edit" | "view";
  editor_url: string;
  expires_in_secs: number;
}

/** `GET /auth/methods`: what the workspace offers on its sign-in page. */
export interface SignInMethods {
  tenant_id: string;
  password: boolean;
  oidc: boolean;
  open_registration: boolean;
  /** True while a single-tenant deployment still awaits its first owner (when the server says). */
  bootstrap_required?: boolean;
  /** Whether the server can send email; false means show copyable links only. */
  mail_configured?: boolean;
}
