import { type FormEvent, useEffect, useId, useState } from "react";

import { people } from "../api/people";
import { asApiError } from "../components";
import { Link, useRouter } from "../router";
import { useSession } from "../session";
import { passwordProblem } from "./signin";

const KEY = "figcraft.reset";
const MAX_TOKEN = 128;
const TOKEN = /^[A-Za-z0-9_-]+$/;

/**
 * A setup or password-reset link carries its token in the URL fragment (`/reset#token=…`), which
 * is never sent to a server or logged. On arrival the token moves to session storage (so a reload
 * keeps working) and is removed from the address bar and history entry.
 */
export function captureResetToken(): void {
  const match = /^#token=([^&]*)$/.exec(window.location.hash);
  if (match === null) return;
  let token: string;
  try {
    token = decodeURIComponent(match[1] ?? "");
  } catch {
    token = "";
  }
  if (token !== "" && token.length <= MAX_TOKEN && TOKEN.test(token)) {
    try {
      window.sessionStorage.setItem(KEY, token);
    } catch {
      // Without storage the token only lasts until the page is reloaded.
    }
  }
  window.history.replaceState(null, "", window.location.pathname);
}

function readResetToken(): string | null {
  try {
    const token = window.sessionStorage.getItem(KEY);
    return token !== null && TOKEN.test(token) && token.length <= MAX_TOKEN
      ? token
      : null;
  } catch {
    return null;
  }
}

function clearResetToken(): void {
  try {
    window.sessionStorage.removeItem(KEY);
  } catch {
    // Nothing to clear.
  }
}

/** Choose a password from a setup or reset link. Works signed in or signed out. */
export function ResetScreen() {
  const { client, signOut } = useSession();
  const { navigate } = useRouter();
  const newId = useId();
  const confirmId = useId();
  const [token, setToken] = useState<string | null>(() => {
    captureResetToken();
    return readResetToken();
  });
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);

  // A link opened again while this page is already showing (same tab) is picked up too.
  useEffect(() => {
    if (token === null) {
      captureResetToken();
      setToken(readResetToken());
    }
  }, [token]);

  if (done) {
    return (
      <section className="panel narrow" aria-labelledby="reset-heading">
        <h1 id="reset-heading">Your password is set</h1>
        <p className="intro" role="status" data-testid="reset-done">
          You can sign in with it now. Any earlier sign-ins were ended.
        </p>
        <button
          type="button"
          className="button primary"
          data-testid="reset-continue"
          onClick={() => {
            void signOut().then(() => navigate("/", { replace: true }));
          }}
        >
          Go to sign-in
        </button>
      </section>
    );
  }

  if (token === null) {
    return (
      <section className="panel narrow" aria-labelledby="reset-heading">
        <h1 id="reset-heading">Link incomplete</h1>
        <p className="intro">
          This page needs the full link you were sent. Open it again, or ask
          your administrator for a new one.
        </p>
        <Link className="button primary" to="/">
          Go to sign-in
        </Link>
      </section>
    );
  }

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    if (next !== confirm) {
      setProblem("The two passwords don't match.");
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await people.resetPassword(client, token, next);
      clearResetToken();
      setDone(true);
    } catch (error) {
      const failure = asApiError(error);
      setProblem(
        failure.status === 404
          ? "This link can't be used. Links work once and expire after a short time, and a newer link replaces an older one. Ask your administrator for a new link."
          : passwordProblem(failure),
      );
      setNext("");
      setConfirm("");
      setBusy(false);
    }
  };

  return (
    <section className="panel narrow" aria-labelledby="reset-heading">
      <h1 id="reset-heading">Choose your password</h1>
      <p className="intro">
        Pick a password for your account. You'll use it, with your login, to
        sign in.
      </p>
      <form
        className="form"
        onSubmit={(event) => void submit(event)}
        data-testid="reset-form"
      >
        <div className="field">
          <label htmlFor={newId}>New password</label>
          <input
            id={newId}
            data-testid="reset-password"
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
          <label htmlFor={confirmId}>Repeat the password</label>
          <input
            id={confirmId}
            data-testid="reset-confirm"
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
          <p role="alert" className="notice error" data-testid="reset-error">
            {problem}
          </p>
        )}
        <button
          type="submit"
          className="button primary"
          data-testid="reset-submit"
          disabled={busy || next === ""}
        >
          {busy ? "Saving…" : "Set password"}
        </button>
      </form>
    </section>
  );
}
