/**
 * Signed-out screens for deployments without (or beside) an identity provider:
 *
 * - {@link SignInScreen}: offers exactly the methods the workspace allows (`GET /auth/methods`):
 *   login and password, and the organisation sign-in button when a provider is configured.
 * - {@link SetupScreen}: first run. The person with the operator's setup token creates the owner
 *   account with a password (`POST /auth/password/bootstrap`), no identity provider needed.
 * - {@link InvitedSignUp}: an invited person creates their password account from the invitation
 *   link (`POST /auth/password/signup`), or signs in with an account they already have.
 *
 * Passwords are never stored, logged or put in a URL by the app; they live in the input until the
 * request is sent.
 */

import { type FormEvent, useEffect, useId, useState } from "react";

import { ApiError } from "../api/client";
import type { SignInMethods } from "../api/types";
import { asApiError } from "../components";
import { Link, useRouter } from "../router";
import { useSession } from "../session";
import { SignInButton } from "./entry";
import { bootstrapProblem } from "./bootstrap";
import { clearInviteToken, readInviteToken } from "./invite";

const WORKSPACE_KEY = "figcraft.workspace";

function storedWorkspace(): string {
  try {
    return window.localStorage.getItem(WORKSPACE_KEY) ?? "";
  } catch {
    return "";
  }
}

function rememberWorkspace(value: string): void {
  try {
    if (value === "") window.localStorage.removeItem(WORKSPACE_KEY);
    else window.localStorage.setItem(WORKSPACE_KEY, value);
  } catch {
    // The choice just is not remembered.
  }
}

/** "about 2 minutes" / "30 seconds", for a wait the server asked for. */
export function waitText(seconds: number | null): string {
  if (seconds === null || seconds <= 0) return "a little while";
  if (seconds < 90) return `${seconds} second${seconds === 1 ? "" : "s"}`;
  const minutes = Math.ceil(seconds / 60);
  return `about ${minutes} minute${minutes === 1 ? "" : "s"}`;
}

/** What a refused password sign-in tells the person. Never says which part was wrong. */
export function loginProblem(error: ApiError): string {
  switch (error.status) {
    case 401:
      return "That login or password isn't right.";
    case 429:
      return `Too many attempts. Try again in ${waitText(error.retryAfter)}.`;
    case 403:
      return "Password sign-in is turned off for this workspace. Use the organisation sign-in instead, or ask an administrator.";
    case 400:
      return "Enter your login and your password.";
    case 404:
      return "We couldn't find that workspace.";
    case 0:
      return "The service could not be reached. Check your connection and try again.";
    default:
      return error.message;
  }
}

/** The sentence for a refused new password: the server's own wording when it gave one. */
export function passwordProblem(error: ApiError): string {
  if (error.reason === "password_policy") return error.message;
  switch (error.status) {
    case 400:
      return "Check the fields and try again.";
    case 409:
      return "An account with that login already exists. Sign in instead.";
    case 404:
      return "That link or token isn't valid any more. It may have expired or already been used.";
    case 403:
      return "Password accounts are turned off for this workspace.";
    case 429:
      return `Too many attempts. Try again in ${waitText(error.retryAfter)}.`;
    case 0:
      return "The service could not be reached. Check your connection and try again.";
    default:
      return error.message;
  }
}

type MethodsState =
  | { kind: "loading" }
  | { kind: "ask_workspace"; problem: string | null }
  | { kind: "ready"; methods: SignInMethods }
  | { kind: "error"; error: ApiError };

export function SignInScreen({
  productName,
  signInHref,
  deepLink,
}: {
  productName: string;
  signInHref: string;
  /** The visitor followed a link to a specific workspace, file or invitation. */
  deepLink: boolean;
}) {
  const { client, refresh } = useSession();
  const [workspace, setWorkspace] = useState(storedWorkspace);
  const [state, setState] = useState<MethodsState>({ kind: "loading" });
  const [retry, setRetry] = useState(0);

  useEffect(() => {
    let cancelled = false;
    const reference = workspace.trim();
    const query =
      reference === "" ? "" : `?tenant=${encodeURIComponent(reference)}`;
    setState({ kind: "loading" });
    client
      .get<SignInMethods>(`/auth/methods${query}`)
      .then((methods) => {
        if (!cancelled) setState({ kind: "ready", methods });
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        const problem = asApiError(error);
        if (problem.status === 404) {
          // A multi-tenant deployment needs to know which workspace; ask.
          setState({
            kind: "ask_workspace",
            problem:
              reference === "" ? null : "We couldn't find that workspace.",
          });
        } else {
          setState({ kind: "error", error: problem });
        }
      });
    return () => {
      cancelled = true;
    };
    // `workspace` is committed by the form, not on every keystroke (see WorkspaceForm).
  }, [client, workspace, retry]);

  const heading = `Sign in to ${productName}`;

  if (state.kind === "loading") {
    return (
      <p role="status" className="muted">
        Loading sign-in options…
      </p>
    );
  }
  if (state.kind === "error") {
    return (
      <section className="panel narrow" aria-labelledby="signin-heading">
        <h1 id="signin-heading">{heading}</h1>
        <p role="alert" className="notice error">
          {state.error.code === "network"
            ? "The service could not be reached. Check your connection."
            : state.error.message}
        </p>
        <button
          type="button"
          className="button primary"
          onClick={() => setRetry((value) => value + 1)}
        >
          Try again
        </button>
      </section>
    );
  }
  if (state.kind === "ask_workspace") {
    return (
      <section className="panel narrow" aria-labelledby="signin-heading">
        <h1 id="signin-heading">{heading}</h1>
        <WorkspaceForm
          initial={workspace}
          problem={state.problem}
          onSubmit={(value) => {
            rememberWorkspace(value);
            setWorkspace(value);
          }}
        />
      </section>
    );
  }

  const { methods } = state;
  const reference = workspace.trim();
  const any = methods.password || methods.oidc;

  return (
    <section className="panel narrow" aria-labelledby="signin-heading">
      <h1 id="signin-heading">{heading}</h1>
      <p className="intro">
        {deepLink
          ? "Sign in to open this link. You will come back to it afterwards."
          : "Sign in to open your files, projects and team."}
      </p>
      {reference !== "" && (
        <p className="muted small" data-testid="signin-workspace-name">
          Workspace <strong>{reference}</strong>{" "}
          <button
            type="button"
            className="linkbutton"
            onClick={() => {
              rememberWorkspace("");
              setWorkspace("");
            }}
          >
            Change
          </button>
        </p>
      )}
      {methods.bootstrap_required === true && (
        <p className="notice" role="status" data-testid="signin-first-run">
          This workspace hasn't been set up yet.{" "}
          <Link to="/setup">Set up the first administrator</Link>
        </p>
      )}
      {methods.password && (
        <PasswordForm
          tenant={reference === "" ? null : reference}
          onDone={() => {
            if (reference !== "") rememberWorkspace(reference);
            void refresh();
          }}
        />
      )}
      {methods.password && methods.oidc && (
        <p className="muted small separator">or</p>
      )}
      {methods.oidc && (
        <SignInButton
          href={signInHref}
          testId="signin-oidc"
          label="Sign in with your organisation account"
        />
      )}
      {!any && (
        <p role="alert" className="notice error">
          No sign-in method is turned on for this workspace. Ask an
          administrator to enable one.
        </p>
      )}
      {methods.bootstrap_required !== true && (
        <p className="muted small">
          Setting this up for the first time?{" "}
          <Link to="/setup">Set up the first administrator</Link>
        </p>
      )}
    </section>
  );
}

function WorkspaceForm({
  initial,
  problem,
  onSubmit,
}: {
  initial: string;
  problem: string | null;
  onSubmit: (value: string) => void;
}) {
  const id = useId();
  const [value, setValue] = useState(initial);
  return (
    <form
      className="form"
      onSubmit={(event) => {
        event.preventDefault();
        if (value.trim() !== "") onSubmit(value.trim());
      }}
    >
      <div className="field">
        <label htmlFor={id}>Workspace</label>
        <input
          id={id}
          data-testid="signin-workspace"
          value={value}
          maxLength={64}
          autoCapitalize="off"
          spellCheck={false}
          required
          onChange={(event) => setValue(event.target.value)}
          aria-describedby={`${id}-hint`}
          aria-invalid={problem !== null}
        />
        <p id={`${id}-hint`} className="muted small">
          The short name of your workspace, from your invitation or your
          administrator.
        </p>
      </div>
      {problem !== null && (
        <p role="alert" className="notice error">
          {problem}
        </p>
      )}
      <button type="submit" className="button primary">
        Continue
      </button>
    </form>
  );
}

function PasswordForm({
  tenant,
  onDone,
}: {
  tenant: string | null;
  onDone: () => void;
}) {
  const { client } = useSession();
  const loginId = useId();
  const passwordId = useId();
  const [login, setLogin] = useState("");
  const [password, setPassword] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setProblem(null);
    try {
      await client.post("/auth/password/login", {
        ...(tenant === null ? {} : { tenant }),
        login: login.trim(),
        password,
      });
      onDone();
    } catch (error) {
      setProblem(loginProblem(asApiError(error)));
      setPassword("");
      setBusy(false);
    }
  };

  return (
    <form
      className="form"
      onSubmit={(event) => void submit(event)}
      data-testid="signin-password-form"
    >
      <div className="field">
        <label htmlFor={loginId}>Login</label>
        <input
          id={loginId}
          data-testid="signin-login"
          value={login}
          autoComplete="username"
          autoCapitalize="off"
          spellCheck={false}
          maxLength={254}
          required
          onChange={(event) => setLogin(event.target.value)}
        />
      </div>
      <div className="field">
        <label htmlFor={passwordId}>Password</label>
        <input
          id={passwordId}
          data-testid="signin-password"
          type="password"
          value={password}
          autoComplete="current-password"
          maxLength={1024}
          required
          onChange={(event) => setPassword(event.target.value)}
          aria-invalid={problem !== null}
        />
      </div>
      {problem !== null && (
        <p role="alert" className="notice error" data-testid="signin-error">
          {problem}
        </p>
      )}
      <button
        type="submit"
        className="button primary"
        data-testid="signin-submit"
        disabled={busy || login.trim() === "" || password === ""}
      >
        {busy ? "Signing in…" : "Sign in"}
      </button>
    </form>
  );
}

/** Two password fields that must match; returns the problem to show, if any. */
function NewPasswordFields({
  password,
  confirm,
  onPassword,
  onConfirm,
  testIdPrefix,
  invalid,
}: {
  password: string;
  confirm: string;
  onPassword: (value: string) => void;
  onConfirm: (value: string) => void;
  testIdPrefix: string;
  invalid: boolean;
}) {
  const passwordId = useId();
  const confirmId = useId();
  return (
    <>
      <div className="field">
        <label htmlFor={passwordId}>Password</label>
        <input
          id={passwordId}
          data-testid={`${testIdPrefix}-password`}
          type="password"
          value={password}
          autoComplete="new-password"
          maxLength={1024}
          required
          onChange={(event) => onPassword(event.target.value)}
          aria-invalid={invalid}
        />
      </div>
      <div className="field">
        <label htmlFor={confirmId}>Repeat the password</label>
        <input
          id={confirmId}
          data-testid={`${testIdPrefix}-confirm`}
          type="password"
          value={confirm}
          autoComplete="new-password"
          maxLength={1024}
          required
          onChange={(event) => onConfirm(event.target.value)}
          aria-invalid={invalid}
        />
      </div>
    </>
  );
}

const MISMATCH = "The two passwords don't match.";

/** First run: the owner account, created with a password. */
export function SetupScreen() {
  const { client, refresh } = useSession();
  const { navigate } = useRouter();
  const tokenId = useId();
  const loginId = useId();
  const nameId = useId();
  const [token, setToken] = useState("");
  const [login, setLogin] = useState("");
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [alreadyDone, setAlreadyDone] = useState(false);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    if (password !== confirm) {
      setProblem(MISMATCH);
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await client.post("/auth/password/bootstrap", {
        token: token.trim(),
        login: login.trim(),
        display_name: name.trim(),
        password,
      });
      await refresh();
      navigate("/", { replace: true });
    } catch (error) {
      const failure = asApiError(error);
      setBusy(false);
      setPassword("");
      setConfirm("");
      if (failure.status === 404) setAlreadyDone(true);
      setProblem(
        failure.status === 403
          ? bootstrapProblem(failure)
          : passwordProblem(failure),
      );
    }
  };

  if (alreadyDone) {
    return (
      <section className="panel narrow" aria-labelledby="setup-heading">
        <h1 id="setup-heading">Already set up</h1>
        <p className="intro">
          This deployment already has an administrator. Sign in instead.
        </p>
        <Link className="button primary" to="/">
          Go to sign-in
        </Link>
      </section>
    );
  }

  return (
    <section className="panel narrow" aria-labelledby="setup-heading">
      <h1 id="setup-heading">Set up the first administrator</h1>
      <p className="intro">
        Create the owner account for this workspace. You need the one-time setup
        token the operator printed; no external sign-in service is required.
      </p>
      <form
        className="form"
        onSubmit={(event) => void submit(event)}
        data-testid="setup-form"
      >
        <div className="field">
          <label htmlFor={tokenId}>Setup token</label>
          <input
            id={tokenId}
            data-testid="setup-token"
            type="password"
            autoComplete="off"
            spellCheck={false}
            autoCapitalize="off"
            value={token}
            maxLength={128}
            required
            onChange={(event) => setToken(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor={nameId}>Your name</label>
          <input
            id={nameId}
            data-testid="setup-name"
            value={name}
            autoComplete="name"
            maxLength={64}
            required
            onChange={(event) => setName(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor={loginId}>Login</label>
          <input
            id={loginId}
            data-testid="setup-login"
            value={login}
            autoComplete="username"
            autoCapitalize="off"
            spellCheck={false}
            maxLength={254}
            required
            onChange={(event) => setLogin(event.target.value)}
          />
          <p className="muted small">An email address or a short username.</p>
        </div>
        <NewPasswordFields
          password={password}
          confirm={confirm}
          onPassword={setPassword}
          onConfirm={setConfirm}
          testIdPrefix="setup"
          invalid={problem !== null}
        />
        {problem !== null && (
          <p role="alert" className="notice error" data-testid="setup-error">
            {problem}
          </p>
        )}
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            data-testid="setup-submit"
            disabled={
              busy ||
              token.trim() === "" ||
              login.trim() === "" ||
              name.trim() === "" ||
              password === ""
            }
          >
            {busy ? "Creating…" : "Create the owner account"}
          </button>
          <Link className="button" to="/">
            Back to sign-in
          </Link>
        </div>
      </form>
    </section>
  );
}

/** An invitation link opened by someone who is not signed in. */
export function InvitedSignUp({
  productName,
  signInHref,
}: {
  productName: string;
  signInHref: string;
}) {
  const { client, refresh } = useSession();
  const { navigate } = useRouter();
  const nameId = useId();
  const [token] = useState(readInviteToken);
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [signIn, setSignIn] = useState(false);

  if (signIn) {
    return (
      <>
        <SignInScreen
          productName={productName}
          signInHref={signInHref}
          deepLink
        />
        <p className="muted small centered">
          <button
            type="button"
            className="linkbutton"
            onClick={() => setSignIn(false)}
          >
            Create a new account for this invitation instead
          </button>
        </p>
      </>
    );
  }

  if (token === null) {
    return (
      <section className="panel narrow" aria-labelledby="invite-heading">
        <h1 id="invite-heading">Invitation link incomplete</h1>
        <p className="intro">
          This page needs the full link you were sent. Open the invitation link
          again, or ask the person who invited you to send a new one.
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
    if (password !== confirm) {
      setProblem(MISMATCH);
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      await client.post("/auth/password/signup", {
        invitation: token,
        display_name: name.trim(),
        password,
      });
      clearInviteToken();
      await refresh();
      navigate("/", { replace: true });
    } catch (error) {
      const failure = asApiError(error);
      setBusy(false);
      setPassword("");
      setConfirm("");
      setProblem(
        failure.status === 404
          ? "This invitation can't be used. It may have expired, been revoked or already been used. Ask the person who invited you for a new link."
          : failure.status === 409
            ? "An account for this address already exists. Sign in with it instead."
            : passwordProblem(failure),
      );
    }
  };

  return (
    <section className="panel narrow" aria-labelledby="invite-heading">
      <h1 id="invite-heading">You've been invited to {productName}</h1>
      <p className="intro">
        Choose a name and a password to create your account and join. Your login
        is the address the invitation was sent to.
      </p>
      <form
        className="form"
        onSubmit={(event) => void submit(event)}
        data-testid="invite-signup-form"
      >
        <div className="field">
          <label htmlFor={nameId}>Your name</label>
          <input
            id={nameId}
            data-testid="invite-name"
            value={name}
            autoComplete="name"
            maxLength={64}
            required
            onChange={(event) => setName(event.target.value)}
          />
        </div>
        <NewPasswordFields
          password={password}
          confirm={confirm}
          onPassword={setPassword}
          onConfirm={setConfirm}
          testIdPrefix="invite"
          invalid={problem !== null}
        />
        {problem !== null && (
          <p role="alert" className="notice error" data-testid="invite-error">
            {problem}
          </p>
        )}
        <button
          type="submit"
          className="button primary"
          data-testid="invite-submit"
          disabled={busy || name.trim() === "" || password === ""}
        >
          {busy ? "Creating…" : "Create account and join"}
        </button>
      </form>
      <p className="muted small">
        Already have an account?{" "}
        <button
          type="button"
          className="linkbutton"
          onClick={() => setSignIn(true)}
        >
          Sign in instead
        </button>
      </p>
    </section>
  );
}
