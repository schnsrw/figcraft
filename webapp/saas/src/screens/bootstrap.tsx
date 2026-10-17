import { type FormEvent, useId, useState } from "react";

import { ApiError } from "../api/client";
import { useSession } from "../session";

/** What a refused setup token looks like to the person typing it. */
export function bootstrapProblem(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.code === "forbidden") {
      return "That setup token wasn't accepted. Check that you copied the whole token from the operator, and that it hasn't already been used.";
    }
    if (error.code === "unauthenticated") {
      return "Your sign-in has expired. Sign in again, then enter the token.";
    }
    if (error.code === "rate_limited") {
      return "Too many attempts. Wait a minute and try again.";
    }
    return error.message;
  }
  return "The token could not be checked. Try again.";
}

/**
 * First run: the deployment has no administrator yet. The person who signed in enters the setup
 * token the operator printed (`figcraft-cloud --print-bootstrap-token`) and becomes the owner.
 */
export function BootstrapScreen() {
  const { client, refresh, signOut } = useSession();
  const tokenId = useId();
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await client.post("/auth/bootstrap/complete", { token: token.trim() });
      await refresh();
    } catch (problem) {
      setBusy(false);
      setError(bootstrapProblem(problem));
    }
  };

  return (
    <section className="panel narrow" aria-labelledby="bootstrap-heading">
      <h1 id="bootstrap-heading">Set up your workspace</h1>
      <p className="intro">
        This deployment doesn't have an administrator yet. You're signed in, so
        you can claim it: enter the setup token printed by the operator and you
        become the owner.
      </p>
      <form className="form" onSubmit={(event) => void submit(event)}>
        <div className="field">
          <label htmlFor={tokenId}>Setup token</label>
          <input
            id={tokenId}
            type="password"
            autoComplete="off"
            spellCheck={false}
            autoCapitalize="off"
            value={token}
            maxLength={128}
            required
            onChange={(event) => setToken(event.target.value)}
            aria-describedby={`${tokenId}-hint`}
            aria-invalid={error !== null}
          />
          <p id={`${tokenId}-hint`} className="muted small">
            The operator prints it with{" "}
            <code>figcraft-cloud --print-bootstrap-token</code>. It works once.
          </p>
        </div>
        {error !== null && (
          <p role="alert" className="notice error">
            {error}
          </p>
        )}
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            disabled={busy || token.trim() === ""}
          >
            {busy ? "Checking…" : "Become the owner"}
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
