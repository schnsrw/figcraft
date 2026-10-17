import { type FormEvent, type MouseEvent, useId, useState } from "react";

import { ApiError } from "../api/client";
import type { Deployment, SessionInfo, TenantSummary } from "../api/types";
import { Link } from "../router";
import { useSession } from "../session";

/** What a failed sign-in attempt tells the person, from the fixed categories of the answer. */
export function signInProblem(status: number): string {
  if (status === 503) {
    return "Sign-in is not available right now. The identity provider is not configured or cannot be reached. Ask the person who runs this deployment to check it, then try again.";
  }
  if (status === 429) {
    return "Too many sign-in attempts. Wait a minute and try again.";
  }
  if (status === 0) {
    return "The service could not be reached. Check your connection and try again.";
  }
  return "Sign-in could not be started. Try again, and if it keeps failing contact the person who runs this deployment.";
}

export interface SignInProbe {
  (href: string): Promise<{ ok: true } | { ok: false; status: number }>;
}

/**
 * Ask the server to begin sign-in without following it: a redirect means the identity provider is
 * configured (the browser then follows the real navigation), anything else is a problem we can
 * explain here instead of showing a bare error document.
 */
export const probeSignIn: SignInProbe = async (href) => {
  try {
    const response = await fetch(href, {
      method: "GET",
      redirect: "manual",
      credentials: "same-origin",
      cache: "no-store",
    });
    if (response.type === "opaqueredirect" || response.status === 303) {
      return { ok: true };
    }
    return { ok: false, status: response.status };
  } catch {
    return { ok: false, status: 0 };
  }
};

export function SignInButton({
  href,
  probe = probeSignIn,
  navigate = (target: string) => window.location.assign(target),
  children = "Sign in",
}: {
  href: string;
  probe?: SignInProbe;
  navigate?: (target: string) => void;
  children?: string;
}) {
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const describe = useId();

  const start = async (event: MouseEvent<HTMLAnchorElement>) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setProblem(null);
    const result = await probe(href);
    if (result.ok) {
      navigate(href);
      return;
    }
    setBusy(false);
    setProblem(signInProblem(result.status));
  };

  return (
    <div className="stack">
      <a
        className="button primary"
        href={href}
        onClick={(event) => void start(event)}
        aria-disabled={busy}
        aria-describedby={problem === null ? undefined : describe}
      >
        {busy ? "Opening sign-in…" : children}
      </a>
      {problem !== null && (
        <p id={describe} role="alert" className="notice error">
          {problem}
        </p>
      )}
    </div>
  );
}

export function SignedOutScreen({
  productName,
  signInHref,
  deepLink,
}: {
  productName: string;
  signInHref: string;
  /** The visitor followed a link to a specific workspace or file. */
  deepLink: boolean;
}) {
  return (
    <section className="panel narrow" aria-labelledby="signed-out-heading">
      <h1 id="signed-out-heading">Sign in to {productName}</h1>
      <p className="intro">
        {deepLink
          ? "You need to sign in to open this link. You will come back to it afterwards."
          : "Sign in with your organisation account to open your files, projects and team."}
      </p>
      <SignInButton href={signInHref} />
      <p className="muted small">
        Setting this deployment up for the first time? Sign in with the account
        that should become the administrator, then enter the setup token the
        operator printed.
      </p>
    </section>
  );
}

export function UnavailableScreen({ error }: { error: ApiError }) {
  const { refresh } = useSession();
  const offline = error.code === "network";
  return (
    <section className="panel narrow" aria-labelledby="unavailable-heading">
      <h1 id="unavailable-heading">
        {offline ? "Can't reach the service" : "Service unavailable"}
      </h1>
      <p role="alert" className="notice error">
        {offline
          ? "The service could not be reached. Check your connection."
          : error.message}
        {error.requestId !== null && (
          <>
            {" "}
            <span className="muted small">Reference {error.requestId}</span>
          </>
        )}
      </p>
      <button
        type="button"
        className="button primary"
        onClick={() => void refresh()}
      >
        Try again
      </button>
    </section>
  );
}

export function UnsupportedScreen({
  credential,
}: {
  credential: SessionInfo["credential"];
}) {
  const { signOut } = useSession();
  return (
    <section className="panel narrow" aria-labelledby="unsupported-heading">
      <h1 id="unsupported-heading">This page is for signed-in people</h1>
      <p className="intro">
        {credential === "share_link"
          ? "You opened a shared link, which gives access to one file only. Open the link from the file's share dialog to use it."
          : "This browser is using a service credential, which cannot browse the workspace."}
      </p>
      <button type="button" className="button" onClick={() => void signOut()}>
        Clear this session
      </button>
    </section>
  );
}

export function NotFoundScreen() {
  return (
    <section className="panel narrow" aria-labelledby="not-found-heading">
      <h1 id="not-found-heading">Page not found</h1>
      <p className="intro">
        That link doesn't match anything here. It may be mistyped or outdated.
      </p>
      <Link className="button primary" to="/">
        Go to your files
      </Link>
    </section>
  );
}

export function TenantPicker({ tenants }: { tenants: TenantSummary[] }) {
  const { chooseTenant } = useSession();
  return (
    <section className="panel narrow" aria-labelledby="tenant-heading">
      <h1 id="tenant-heading">Choose an organisation</h1>
      <p className="intro">
        Your account belongs to more than one organisation.
      </p>
      <ul className="choice-list">
        {tenants.map((tenant) => (
          <li key={tenant.id}>
            <button
              type="button"
              className="button wide"
              onClick={() => void chooseTenant(tenant.id)}
            >
              <span>{tenant.name}</span>
              {tenant.role !== undefined && (
                <span className="muted small">{roleLabel(tenant.role)}</span>
              )}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

export function NoTenantScreen({ deployment }: { deployment: Deployment }) {
  const { client, chooseTenant } = useSession();
  const nameId = useId();
  const slugId = useId();
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const canCreate = deployment === "multi";

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const created = await client.post<TenantSummary>("/tenants", {
        name: name.trim(),
        slug: slug.trim() === "" ? slugify(name) : slug.trim(),
      });
      await chooseTenant(created.id);
    } catch (problem) {
      setBusy(false);
      setError(
        problem instanceof ApiError
          ? problem.message
          : "The organisation could not be created.",
      );
    }
  };

  return (
    <section className="panel narrow" aria-labelledby="no-tenant-heading">
      <h1 id="no-tenant-heading">You're not in an organisation yet</h1>
      <p className="intro">
        {canCreate
          ? "Create your own organisation, or ask an administrator to invite you to theirs."
          : "Ask an administrator to invite you. If you were sent an invitation link, open it now."}
      </p>
      {canCreate && (
        <form className="form" onSubmit={(event) => void submit(event)}>
          <div className="field">
            <label htmlFor={nameId}>Organisation name</label>
            <input
              id={nameId}
              value={name}
              maxLength={120}
              required
              onChange={(event) => setName(event.target.value)}
            />
          </div>
          <div className="field">
            <label htmlFor={slugId}>Short name (optional)</label>
            <input
              id={slugId}
              value={slug}
              maxLength={63}
              pattern="[a-z0-9][a-z0-9-]*"
              onChange={(event) => setSlug(event.target.value)}
              aria-describedby={`${slugId}-hint`}
            />
            <p id={`${slugId}-hint`} className="muted small">
              Lowercase letters, numbers and hyphens. Used in links.
            </p>
          </div>
          {error !== null && (
            <p role="alert" className="notice error">
              {error}
            </p>
          )}
          <button
            type="submit"
            className="button primary"
            disabled={busy || name.trim() === ""}
          >
            {busy ? "Creating…" : "Create organisation"}
          </button>
        </form>
      )}
    </section>
  );
}

export function slugify(name: string): string {
  const slug = name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40);
  return slug === "" ? "organisation" : slug;
}

const ROLE_LABELS: Record<string, string> = {
  tenant_owner: "Owner",
  tenant_admin: "Admin",
  team_manager: "Team manager",
  workspace_admin: "Workspace admin",
  editor: "Editor",
  commenter: "Commenter",
  viewer: "Viewer",
};

export function roleLabel(role: string | null | undefined): string {
  if (role === null || role === undefined) return "No role";
  return ROLE_LABELS[role] ?? "Member";
}

/** The signed-in person's menu: who they act as, organisation switching and sign-out. */
export function AccountMenu() {
  const { state, signOut, chooseTenant } = useSession();
  if (state.kind === "loading" || state.kind === "signed_out") return null;
  if (state.kind === "unavailable") return null;
  const tenants = state.kind === "ready" ? state.tenants : [];
  const current = state.kind === "ready" ? state.tenant.id : null;
  const currentName = tenants.find((tenant) => tenant.id === current)?.name;
  const switchable =
    state.kind === "ready" &&
    state.session.deployment !== "single" &&
    tenants.length > 1;
  return (
    <div className="account" role="group" aria-label="Account">
      {currentName !== undefined && (
        <span className="org-name">
          {switchable ? (
            <select
              aria-label="Organisation"
              value={current ?? ""}
              onChange={(event) => void chooseTenant(event.target.value)}
            >
              {tenants.map((tenant) => (
                <option key={tenant.id} value={tenant.id}>
                  {tenant.name}
                </option>
              ))}
            </select>
          ) : (
            currentName
          )}
        </span>
      )}
      <button type="button" className="button" onClick={() => void signOut()}>
        Sign out
      </button>
    </div>
  );
}
