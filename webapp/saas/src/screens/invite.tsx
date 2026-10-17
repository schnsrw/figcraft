import { useState } from "react";

import type { ApiError } from "../api/client";
import { type AcceptedInvitation, people } from "../api/people";
import { asApiError } from "../components";
import { Link, useRouter } from "../router";
import { useSession } from "../session";

const KEY = "figcraft.invite";
const MAX_TOKEN = 128;
const TOKEN = /^[A-Za-z0-9_-]+$/;

/**
 * An invitation link carries its token in the URL fragment (`/invite#token=…`), which is never
 * sent to a server or written to access logs. On arrival the token is moved to session storage
 * (so it survives the sign-in round trip) and removed from the address bar and history entry.
 */
export function captureInviteToken(): void {
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
      // Without storage the link still works if the person is already signed in.
    }
  }
  window.history.replaceState(null, "", window.location.pathname);
}

export function readInviteToken(): string | null {
  try {
    const token = window.sessionStorage.getItem(KEY);
    return token !== null && TOKEN.test(token) && token.length <= MAX_TOKEN
      ? token
      : null;
  } catch {
    return null;
  }
}

function clearInviteToken(): void {
  try {
    window.sessionStorage.removeItem(KEY);
  } catch {
    // Nothing to clear.
  }
}

export function inviteProblem(error: ApiError): string {
  if (error.status === 404 || error.code === "not_found") {
    return "This invitation can't be used with this account. It may have expired, been revoked, already been used, or been sent to a different email address.";
  }
  return error.message;
}

export function InviteScreen() {
  const { client, refresh } = useSession();
  const { navigate } = useRouter();
  const [token] = useState(readInviteToken);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [done, setDone] = useState<AcceptedInvitation | null>(null);

  if (token === null && done === null) {
    return (
      <section className="panel narrow" aria-labelledby="invite-heading">
        <h1 id="invite-heading">Invitation link incomplete</h1>
        <p className="intro">
          This page needs the full link you were sent. Open the invitation link
          again, or ask the person who invited you to send a new one.
        </p>
        <Link className="button primary" to="/">
          Go to your files
        </Link>
      </section>
    );
  }

  if (done !== null) {
    return (
      <section className="panel narrow" aria-labelledby="invite-heading">
        <h1 id="invite-heading">You're in</h1>
        <p className="intro" role="status">
          {done.joined_tenant
            ? "You've joined the organisation and the team."
            : "You've been added to the team."}
        </p>
        <button
          type="button"
          className="button primary"
          onClick={() => {
            void refresh().then(() => navigate("/"));
          }}
        >
          Go to your files
        </button>
      </section>
    );
  }

  const accept = async () => {
    if (token === null) return;
    setBusy(true);
    setProblem(null);
    try {
      const accepted = await people.accept(client, token);
      clearInviteToken();
      setDone(accepted);
    } catch (error) {
      setProblem(asApiError(error));
      setBusy(false);
    }
  };

  return (
    <section className="panel narrow" aria-labelledby="invite-heading">
      <h1 id="invite-heading">You've been invited</h1>
      <p className="intro">
        Accept to join the team you were invited to. You'll use the account
        you're signed in with now.
      </p>
      {problem !== null && (
        <div role="alert" className="notice error">
          <p>{inviteProblem(problem)}</p>
        </div>
      )}
      <button
        type="button"
        className="button primary"
        disabled={busy}
        onClick={() => void accept()}
      >
        {busy ? "Joining…" : "Accept invitation"}
      </button>
    </section>
  );
}
