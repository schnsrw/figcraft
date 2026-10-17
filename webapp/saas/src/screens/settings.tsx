import { useId, useState } from "react";

import type { ApiError } from "../api/client";
import { type AccountSettings, settings } from "../api/settings";
import { ErrorNotice, LoadingNotice, asApiError, useLoad } from "../components";
import { hasRole, useReady } from "../session";
import { roleLabel } from "./entry";

/** Workspace settings for owners and admins: which sign-in methods the workspace offers. */
export function SettingsScreen() {
  const { client, tenantId, role } = useReady();
  const [loaded, reload] = useLoad(
    () => settings.get(client, tenantId),
    [client, tenantId],
  );
  const canEdit = hasRole(role, ["tenant_owner", "tenant_admin"]);

  return (
    <>
      <h1>Settings</h1>
      <p className="intro">
        Your role here is <strong>{roleLabel(role)}</strong>.{" "}
        {canEdit
          ? "You can choose how people sign in to this workspace."
          : "Only owners and admins can change these."}
      </p>
      {loaded.kind === "loading" && <LoadingNotice label="Loading settings…" />}
      {loaded.kind === "error" && (
        <ErrorNotice error={loaded.error} retry={reload} />
      )}
      {loaded.kind === "ready" && (
        <SignInSettings
          initial={loaded.value}
          canEdit={canEdit}
          onSave={async (next) => {
            await settings.put(client, tenantId, next);
            reload();
          }}
        />
      )}
      <p className="muted small">
        Libraries and notification settings aren't available yet.
      </p>
    </>
  );
}

function SignInSettings({
  initial,
  canEdit,
  onSave,
}: {
  initial: AccountSettings;
  canEdit: boolean;
  onSave: (value: AccountSettings) => Promise<void>;
}) {
  const [value, setValue] = useState(initial);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const passwordId = useId();
  const oidcId = useId();
  const openId = useId();
  const changed =
    value.password_enabled !== initial.password_enabled ||
    value.oidc_enabled !== initial.oidc_enabled ||
    value.open_registration !== initial.open_registration;
  const noMethod = !value.password_enabled && !value.oidc_enabled;

  const set = (change: Partial<AccountSettings>) => {
    setSaved(false);
    setValue((previous) => {
      const next = { ...previous, ...change };
      // Open registration is a password feature.
      if (!next.password_enabled) next.open_registration = false;
      return next;
    });
  };

  return (
    <form
      className="form"
      data-testid="settings-form"
      onSubmit={(event) => {
        event.preventDefault();
        if (busy || noMethod) return;
        setBusy(true);
        setProblem(null);
        onSave(value)
          .then(() => setSaved(true))
          .catch((error: unknown) => setProblem(asApiError(error)))
          .finally(() => setBusy(false));
      }}
    >
      <fieldset disabled={!canEdit}>
        <legend>How people sign in</legend>
        <label className="check" htmlFor={passwordId}>
          <input
            id={passwordId}
            data-testid="settings-password"
            type="checkbox"
            checked={value.password_enabled}
            onChange={(event) =>
              set({ password_enabled: event.target.checked })
            }
          />
          Login name and password
        </label>
        <label className="check" htmlFor={oidcId}>
          <input
            id={oidcId}
            data-testid="settings-oidc"
            type="checkbox"
            checked={value.oidc_enabled}
            onChange={(event) => set({ oidc_enabled: event.target.checked })}
          />
          Organisation sign-in (identity provider)
        </label>
        <label className="check" htmlFor={openId}>
          <input
            id={openId}
            data-testid="settings-open-registration"
            type="checkbox"
            checked={value.open_registration}
            disabled={!value.password_enabled}
            onChange={(event) =>
              set({ open_registration: event.target.checked })
            }
          />
          Anyone can create an account with a password
        </label>
        {!value.password_enabled && (
          <p className="muted small">
            Turn on passwords to allow people to create their own accounts.
          </p>
        )}
      </fieldset>
      {noMethod && (
        <p role="alert" className="notice error" data-testid="settings-warning">
          At least one sign-in method must stay on, or nobody could sign in.
        </p>
      )}
      {problem !== null && <ErrorNotice error={problem} />}
      {saved && (
        <p role="status" className="notice ok">
          Saved.
        </p>
      )}
      {canEdit && (
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            data-testid="settings-save"
            disabled={busy || !changed || noMethod}
          >
            {busy ? "Saving…" : "Save settings"}
          </button>
        </div>
      )}
    </form>
  );
}
