import { type FormEvent, useId, useState } from "react";

import { me } from "../api/account";
import { asApiError } from "../components";
import { useSession } from "../session";
import { passwordProblem } from "./signin";

/**
 * An administrator set this account's password, so it is temporary: until the person chooses their
 * own, the server refuses everything but changing it (and signing out).
 */
export function MustChangePasswordScreen() {
  const { client, refresh, signOut } = useSession();
  const currentId = useId();
  const newId = useId();
  const confirmId = useId();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    if (next !== confirm) {
      setProblem("The two new passwords don't match.");
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await me.changePassword(client, current, next);
      await refresh();
    } catch (error) {
      const failure = asApiError(error);
      setProblem(
        failure.status === 403
          ? "The temporary password isn't right."
          : passwordProblem(failure),
      );
      setCurrent("");
      setBusy(false);
    }
  };

  return (
    <section className="panel narrow" aria-labelledby="mustchange-heading">
      <h1 id="mustchange-heading">Choose your own password</h1>
      <p className="intro" data-testid="mustchange-intro">
        Your administrator gave you a temporary password. Choose a new one
        before you continue.
      </p>
      <form
        className="form"
        onSubmit={(event) => void submit(event)}
        data-testid="mustchange-form"
      >
        <div className="field">
          <label htmlFor={currentId}>Temporary password</label>
          <input
            id={currentId}
            data-testid="mustchange-current"
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
            data-testid="mustchange-new"
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
            data-testid="mustchange-confirm"
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
          <p
            role="alert"
            className="notice error"
            data-testid="mustchange-error"
          >
            {problem}
          </p>
        )}
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            data-testid="mustchange-submit"
            disabled={busy || current === "" || next === ""}
          >
            {busy ? "Saving…" : "Set my password"}
          </button>
          <button
            type="button"
            className="button"
            onClick={() => void signOut()}
          >
            Sign out
          </button>
        </div>
      </form>
    </section>
  );
}
