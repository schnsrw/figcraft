import type { ApiClient } from "./client";

/** `GET /me`. */
export interface Profile {
  principal_id: string;
  display_name: string;
  color: string;
  color_chosen: boolean;
  palette: string[];
  tenant_id?: string;
  /** Only when the person has a password account in the selected workspace. */
  login?: string | null;
  must_change_password?: boolean;
}

/** One live browser session (never a token). Times are Unix seconds. */
export interface SessionSummary {
  id: string;
  created_at: number;
  last_seen_at: number;
  idle_expires_at: number;
  absolute_expires_at: number;
  current: boolean;
}

export const me = {
  profile: (client: ApiClient) => client.get<Profile>("/me"),
  update: (
    client: ApiClient,
    change: { display_name?: string; color?: string },
  ) => client.patch<Profile>("/me", change),
  changePassword: (
    client: ApiClient,
    currentPassword: string,
    newPassword: string,
  ) =>
    client.post<{ revoked_sessions: number }>("/me/password", {
      current_password: currentPassword,
      new_password: newPassword,
    }),
  sessions: (client: ApiClient) =>
    client.get<{ items: SessionSummary[] }>("/me/sessions"),
  revokeSession: (client: ApiClient, id: string) =>
    client.delete<void>(`/me/sessions/${id}`),
  revokeOthers: (client: ApiClient) =>
    client.post<{ revoked: number }>("/me/sessions/revoke-others"),
};
