/** Small shared building blocks: async loading, error notices, a modal dialog and name forms. */

import {
  type FormEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";

import { ApiError } from "./api/client";

export type Loadable<T> =
  | { kind: "loading" }
  | { kind: "error"; error: ApiError }
  | { kind: "ready"; value: T };

export function asApiError(error: unknown): ApiError {
  if (error instanceof ApiError) return error;
  return new ApiError(0, "network", "The service could not be reached.");
}

/** Load something on mount and whenever `deps` change; `reload` runs it again. */
export function useLoad<T>(
  load: () => Promise<T>,
  deps: readonly unknown[],
): [Loadable<T>, () => void, (value: T) => void] {
  const [state, setState] = useState<Loadable<T>>({ kind: "loading" });
  const [tick, setTick] = useState(0);
  const loadRef = useRef(load);
  loadRef.current = load;

  useEffect(() => {
    let cancelled = false;
    // A reload keeps what is on screen until the new answer arrives.
    setState((previous) =>
      tick === 0 || previous.kind === "error" ? { kind: "loading" } : previous,
    );
    loadRef
      .current()
      .then((value) => {
        if (!cancelled) setState({ kind: "ready", value });
      })
      .catch((error: unknown) => {
        if (!cancelled) setState({ kind: "error", error: asApiError(error) });
      });
    return () => {
      cancelled = true;
    };
  }, [...deps, tick]);

  const reload = useCallback(() => setTick((value) => value + 1), []);
  const set = useCallback((value: T) => setState({ kind: "ready", value }), []);
  return [state, reload, set];
}

/** The message to show for a failed call, with what to do about the common cases. */
export function errorText(error: ApiError): string {
  switch (error.code) {
    case "resource_inactive":
      return "This workspace or item is suspended or archived, so it can't be changed right now.";
    case "network":
      return "The service could not be reached. Check your connection and try again.";
    case "rate_limited":
      return "Too many requests. Wait a moment and try again.";
    default:
      return error.message;
  }
}

export function ErrorNotice({
  error,
  retry,
}: {
  error: ApiError;
  retry?: () => void;
}) {
  return (
    <div role="alert" className="notice error">
      <p>{errorText(error)}</p>
      {error.requestId !== null && (
        <p className="muted small">Reference {error.requestId}</p>
      )}
      {retry !== undefined && (
        <button type="button" className="button" onClick={retry}>
          Try again
        </button>
      )}
    </div>
  );
}

export function LoadingNotice({ label = "Loading…" }: { label?: string }) {
  return (
    <p role="status" className="muted">
      {label}
    </p>
  );
}

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/**
 * A modal dialog: labelled, traps Tab inside, closes on Escape or the Cancel button, and returns
 * focus to the control that opened it.
 */
export function Modal({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const titleId = useId();
  const ref = useRef<HTMLDivElement>(null);
  const opener = useRef<Element | null>(null);

  useEffect(() => {
    opener.current = document.activeElement;
    const first = ref.current?.querySelector<HTMLElement>(
      "input, select, textarea, button",
    );
    first?.focus();
    return () => {
      if (opener.current instanceof HTMLElement) opener.current.focus();
    };
  }, []);

  const onKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      onClose();
      return;
    }
    if (event.key !== "Tab" || ref.current === null) return;
    const items = Array.from(
      ref.current.querySelectorAll<HTMLElement>(FOCUSABLE),
    );
    const first = items[0];
    const last = items[items.length - 1];
    if (first === undefined || last === undefined) return;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  return (
    <div className="modal-backdrop">
      <div
        ref={ref}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="modal"
        onKeyDown={onKeyDown}
      >
        <h2 id={titleId}>{title}</h2>
        {children}
      </div>
    </div>
  );
}

/** A one-field form in a modal: rename something, or name something new. */
export function NameDialog({
  title,
  label,
  initial = "",
  submitLabel,
  onSubmit,
  onClose,
}: {
  title: string;
  label: string;
  initial?: string;
  submitLabel: string;
  /** Resolve to close; reject to show the error and keep the dialog open. */
  onSubmit: (name: string) => Promise<void>;
  onClose: () => void;
}) {
  const id = useId();
  const [name, setName] = useState(initial);
  const [error, setError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await onSubmit(name.trim());
      onClose();
    } catch (problem) {
      setError(asApiError(problem));
      setBusy(false);
    }
  };

  return (
    <Modal title={title} onClose={onClose}>
      <form className="form" onSubmit={(event) => void submit(event)}>
        <div className="field">
          <label htmlFor={id}>{label}</label>
          <input
            id={id}
            value={name}
            maxLength={255}
            required
            onChange={(event) => setName(event.target.value)}
            aria-invalid={error !== null}
          />
        </div>
        {error !== null && <ErrorNotice error={error} />}
        <div className="actions">
          <button
            type="submit"
            className="button primary"
            disabled={busy || name.trim() === ""}
          >
            {busy ? "Saving…" : submitLabel}
          </button>
          <button type="button" className="button" onClick={onClose}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** An inline "new …" form: a labelled field and a button, errors announced below. */
export function CreateForm({
  label,
  buttonLabel,
  onCreate,
  successLabel,
  hint,
}: {
  label: string;
  buttonLabel: string;
  onCreate: (name: string) => Promise<void>;
  /** A confirmation to announce after a successful create. */
  successLabel?: (name: string) => string;
  /** Help under the field, such as an example name. */
  hint?: string;
}) {
  const id = useId();
  const [name, setName] = useState("");
  const [error, setError] = useState<ApiError | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    setDone(null);
    try {
      const created = name.trim();
      await onCreate(created);
      setName("");
      if (successLabel !== undefined) setDone(successLabel(created));
    } catch (problem) {
      setError(asApiError(problem));
    }
    setBusy(false);
  };

  return (
    <form className="inline-form" onSubmit={(event) => void submit(event)}>
      <div className="field grow">
        <label htmlFor={id}>{label}</label>
        <input
          id={id}
          value={name}
          maxLength={255}
          onChange={(event) => {
            setDone(null);
            setName(event.target.value);
          }}
          aria-invalid={error !== null}
          aria-describedby={hint === undefined ? undefined : `${id}-hint`}
        />
        {hint !== undefined && (
          <p id={`${id}-hint`} className="muted small">
            {hint}
          </p>
        )}
      </div>
      <button
        type="submit"
        className="button primary"
        disabled={busy || name.trim() === ""}
        aria-busy={busy}
      >
        {busy ? "Creating…" : buttonLabel}
      </button>
      {error !== null && <ErrorNotice error={error} />}
      {done !== null && (
        <p role="status" className="notice ok">
          {done}
        </p>
      )}
    </form>
  );
}

// ---------------------------------------------------------------------------------------------
// Presentation building blocks

const TINTS = [
  "#b7df79",
  "#8fd3c4",
  "#9bbcf2",
  "#f2c27b",
  "#e9a0b6",
  "#c3a6ec",
  "#f0a58f",
  "#a8d5ee",
];

/** A stable soft colour for `seed` (an id or a name), so the same thing always looks the same. */
export function tint(seed: string): string {
  let hash = 0;
  for (let index = 0; index < seed.length; index += 1) {
    hash = (hash * 31 + seed.charCodeAt(index)) >>> 0;
  }
  return TINTS[hash % TINTS.length] ?? "#b7df79";
}

/** The first letter of a name, uppercased ("?" for an empty one). */
export function initialOf(name: string): string {
  const letter = Array.from(name.trim())[0];
  return letter === undefined ? "?" : letter.toUpperCase();
}

/** A round initial, coloured from `color` (a person's chosen colour) or from `seed`. */
export function Avatar({
  name,
  color,
  seed,
  small = false,
}: {
  name: string;
  color?: string | undefined;
  seed?: string | undefined;
  small?: boolean;
}) {
  return (
    <span
      className={small ? "avatar small" : "avatar"}
      style={{ background: color ?? tint(seed ?? name) }}
      aria-hidden="true"
    >
      {initialOf(name)}
    </span>
  );
}

/** Title, optional description and breadcrumb on the left; the page's main actions on the right. */
export function PageHeader({
  title,
  description,
  breadcrumb,
  actions,
}: {
  title: ReactNode;
  description?: ReactNode;
  breadcrumb?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <header className="page-header">
      <div className="titles">
        {breadcrumb}
        <h1>{title}</h1>
        {description !== undefined && <p className="intro">{description}</p>}
      </div>
      {actions !== undefined && <div className="actions">{actions}</div>}
    </header>
  );
}

/** Nothing here yet: say what this is for and offer the one obvious next step. */
export function EmptyState({
  glyph = "+",
  title,
  text,
  children,
}: {
  glyph?: string;
  title: string;
  text: string;
  children?: ReactNode;
}) {
  return (
    <div className="empty" data-testid="empty-state">
      <span className="glyph" aria-hidden="true">
        {glyph}
      </span>
      <h2>{title}</h2>
      <p>{text}</p>
      {children}
    </div>
  );
}

/** "just now", "5 minutes ago", "yesterday", "3 days ago", or the date for older times. */
export function relativeTime(
  unixSeconds: number,
  nowMs: number = Date.now(),
): string {
  const seconds = Math.round(nowMs / 1000 - unixSeconds);
  if (seconds < 45) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} minute${minutes === 1 ? "" : "s"} ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} hour${hours === 1 ? "" : "s"} ago`;
  const days = Math.round(hours / 24);
  if (days === 1) return "yesterday";
  if (days < 14) return `${days} days ago`;
  return new Date(unixSeconds * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

export function StateBadge({ state }: { state: string }) {
  if (state === "active") return null;
  const label =
    state === "archived"
      ? "Archived"
      : state === "deleting"
        ? "Scheduled for deletion"
        : state === "suspended"
          ? "Suspended"
          : state;
  return <span className={`badge ${state}`}>{label}</span>;
}
