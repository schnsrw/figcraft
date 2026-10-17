import type { ApiClient } from "./client";

/** `GET/PUT /tenants/{t}/account-settings`. */
export interface AccountSettings {
  password_enabled: boolean;
  oidc_enabled: boolean;
  open_registration: boolean;
  require_totp: boolean;
}

export const settings = {
  get: (client: ApiClient, tenant: string) =>
    client.get<AccountSettings>(`/tenants/${tenant}/account-settings`),
  put: (client: ApiClient, tenant: string, value: AccountSettings) =>
    client.put<AccountSettings>(`/tenants/${tenant}/account-settings`, value),
};
