import { type FormEvent, useId, useState } from "react";

import { me } from "../api/account";
import type { ApiError } from "../api/client";
import { ErrorNotice, LoadingNotice, asApiError, useLoad } from "../components";
import { useSession } from "../session";
import { passwordProblem } from "./signin";

/** The signed-in person's own account: profile, password and browser sessions. */
export function AccountScreen() {
  return (
    <>
      <h1>Your account</h1>
      <p className="intro">
        How you appear to others, your password, and where you're signed in.
      </p>
      <div className="account-grid">
        <ProfileSection />
        <PasswordSection />
        <SessionsSection />
      </div>
    </>
  );
}

function ProfileSection() {
  const { client } = useSession();
  const [profile, , setProfile] = useLoad(() => me.profile(client), [client]);
  const nameId = useId();
  const [name, setName] = useState<string | null>(null);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [saved, setSaved] = useState(false);

  if (profile.kind === "loading")
    return <LoadingNotice label="Loading profile…" />;
  if (profile.kind === "error") return <ErrorNotice error={profile.error} />;
  const current = profile.value;
  const shown = name ?? current.display_name;

  const save = async (change: { display_name?: string; color?: string }) => {
    setProblem(null);
    setSaved(false);
    try {
      setProfile(await me.update(client, change));
      setName(null);
      setSaved(true);
    } catch (error) {
      setProblem(asApiError(error));
    }
  };

  return (
    <section aria-labelledby="profile-heading" data-testid="profile-section">
      <h2 id="profile-heading">Profile</h2>
      {current.login !== undefined && current.login !== null && (
        <p className="muted">
          Signed in as{" "}
          <strong data-testid="profile-login">{current.login}</strong>
        </p>
      )}
      <form
        className="inline-form"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          void save({ display_name: shown.trim() });
        }}
      >
        <div className="field grow">
          <label htmlFor={nameId}>Display name</label>
          <input
            id={nameId}
            data-testid="profile-name"
            value={shown}
            maxLength={64}
            required
            onChange={(event) => {
              setSaved(false);
              setName(event.target.value);
            }}
          />
        </div>
        <button
          type="submit"
          className="button primary"
          data-testid="profile-save"
          disabled={
            shown.trim() === "" || shown.trim() === current.display_name
          }
        >
          Save name
        </button>
      </form>
      <fieldset className="swatches">
        <legend>Your colour in shared files</legend>
        <div role="radiogroup" aria-label="Your colour" className="swatch-row">
          {current.palette.map((colour) => (
            <button
              key={colour}
              type="button"
              role="radio"
              aria-checked={colour === current.color}
              aria-label={colour}
              data-testid={`colour-${colour}`}
              className="swatch"
              style={{ background: colour }}
              onClick={() => void save({ color: colour })}
            />
          ))}
        </div>
      </fieldset>
      {problem !== null && <ErrorNotice error={problem} />}
      {saved && (
        <p role="status" className="notice ok">
          Saved.
        </p>
      )}
    </section>
  );
}

function PasswordSection() {
  const { client } = useSession();
  const currentId = useId();
  const newId = useId();
  const confirmId = useId();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [done, setDone] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setDone(null);
    if (next !== confirm) {
      setProblem("The two new passwords don't match.");
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      const result = await me.changePassword(client, current, next);
      setDone(result.revoked_sessions);
      setCurrent("");
      setNext("");
      setConfirm("");
    } catch (error) {
      const failure = asApiError(error);
      setProblem(
        failure.status === 403
          ? "Your current password isn't right."
          : passwordProblem(failure),
      );
      setCurrent("");
    }
    setBusy(false);
  };

  return (
    <section aria-labelledby="password-heading" data-testid="password-section">
      <h2 id="password-heading">Change password</h2>
      <form className="form" onSubmit={(event) => void submit(event)}>
        <div className="field">
          <label htmlFor={currentId}>Current password</label>
          <input
            id={currentId}
            data-testid="password-current"
            type="password"
            autoComplete="current-password"
            value={current}
            maxLength={1024}
            required
            onChange={(event) => setCurrent(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor={newId}>New password</label>
          <input
            id={newId}
            data-testid="password-new"
            type="password"
            autoComplete="new-password"
            value={next}
            maxLength={1024}
            required
            onChange={(event) => setNext(event.target.value)}
            aria-invalid={problem !== null}
          />
        </div>
        <div className="field">
          <label htmlFor={confirmId}>Repeat the new password</label>
          <input
            id={confirmId}
            data-testid="password-confirm"
            type="password"
            autoComplete="new-password"
            value={confirm}
            maxLength={1024}
            required
            onChange={(event) => setConfirm(event.target.value)}
            aria-invalid={problem !== null}
          />
        </div>
        {problem !== null && (
          <p role="alert" className="notice error" data-testid="password-error">
            {problem}
          </p>
        )}
        {done !== null && (
          <p role="status" className="notice ok" data-testid="password-done">
            Password changed.{" "}
            {done === 0
              ? "You weren't signed in anywhere else."
              : `${done} other session${done === 1 ? " was" : "s were"} signed out.`}
          </p>
        )}
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            data-testid="password-submit"
            disabled={busy || current === "" || next === ""}
          >
            {busy ? "Changing…" : "Change password"}
          </button>
        </div>
      </form>
    </section>
  );
}

function when(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString();
}

function SessionsSection() {
  const { client, signOut } = useSession();
  const [list, reload] = useLoad(() => me.sessions(client), [client]);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  if (list.kind === "loading")
    return <LoadingNotice label="Loading sessions…" />;
  if (list.kind === "error")
    return <ErrorNotice error={list.error} retry={reload} />;
  const sessions = list.value.items;
  const others = sessions.filter((item) => !item.current).length;

  const run = async (action: () => Promise<string | null>) => {
    setProblem(null);
    setNotice(null);
    try {
      setNotice(await action());
    } catch (error) {
      setProblem(asApiError(error));
    }
    reload();
  };

  return (
    <section aria-labelledby="sessions-heading" data-testid="sessions-section">
      <div className="toolbar">
        <h2 id="sessions-heading">Where you're signed in</h2>
        {others > 0 && (
          <button
            type="button"
            className="button"
            data-testid="sessions-revoke-others"
            onClick={() =>
              void run(async () => {
                const result = await me.revokeOthers(client);
                return `Signed out of ${result.revoked} other session${result.revoked === 1 ? "" : "s"}.`;
              })
            }
          >
            Sign out everywhere else
          </button>
        )}
      </div>
      {problem !== null && <ErrorNotice error={problem} />}
      {notice !== null && (
        <p role="status" className="notice ok">
          {notice}
        </p>
      )}
      <ul className="list" aria-label="Sessions">
        {sessions.map((item) => (
          <li key={item.id}>
            <div className="row-main">
              <span className="row-title">
                {item.current ? "This browser" : "Another browser"}
              </span>
              <span className="muted small">
                Signed in {when(item.created_at)} · last active{" "}
                {when(item.last_seen_at)}
              </span>
            </div>
            <button
              type="button"
              className="button"
              aria-label={
                item.current
                  ? "Sign out of this browser"
                  : `Sign out the session from ${when(item.created_at)}`
              }
              onClick={() =>
                void (item.current
                  ? signOut()
                  : run(async () => {
                      await me.revokeSession(client, item.id);
                      return "Signed out.";
                    }))
              }
            >
              Sign out
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}
