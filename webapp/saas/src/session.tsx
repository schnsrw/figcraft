/**
 * Who is signed in, and which organisation they are acting in.
 *
 * {@link loadSession} turns `GET /api/v1/auth/session` (and, for multi-tenant deployments,
 * `GET /api/v1/tenants`) into one of a few explicit states, so every screen handles each case
 * the API can produce:
 *
 * | API answer | State |
 * |---|---|
 * | `401` | `signed_out` |
 * | `403 bootstrap_required`, or `bootstrap_required: true` | `bootstrap_required` |
 * | `503`, network failure, any other error | `unavailable` (retryable) |
 * | a service-account or share-link credential | `unsupported` |
 * | signed in, several organisations, none chosen | `choose_tenant` |
 * | signed in, no organisation | `no_tenant` |
 * | signed in and acting in an organisation | `ready` |
 */

import {
  type ReactNode,
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { ApiClient, ApiError } from "./api/client";
import type { Page, SessionInfo, TenantSummary } from "./api/types";

export type SessionState =
  | { kind: "loading" }
  | { kind: "signed_out" }
  | { kind: "unavailable"; error: ApiError }
  | { kind: "unsupported"; credential: SessionInfo["credential"] }
  | { kind: "bootstrap_required"; session: SessionInfo }
  | { kind: "choose_tenant"; session: SessionInfo; tenants: TenantSummary[] }
  | { kind: "no_tenant"; session: SessionInfo }
  | {
      kind: "ready";
      session: SessionInfo;
      /** The organisation being acted in, with the caller's role there. */
      tenant: { id: string; role: string | null; permissions: string[] };
      tenants: TenantSummary[];
    };

export const TENANT_STORAGE_KEY = "figcraft.tenant";

function readStoredTenant(): string | null {
  try {
    return window.localStorage.getItem(TENANT_STORAGE_KEY);
  } catch {
    return null;
  }
}

function storeTenant(id: string | null): void {
  try {
    if (id === null) window.localStorage.removeItem(TENANT_STORAGE_KEY);
    else window.localStorage.setItem(TENANT_STORAGE_KEY, id);
  } catch {
    // Private mode or blocked storage: the choice just is not remembered.
  }
}

function asUnavailable(error: unknown): SessionState {
  if (error instanceof ApiError) return { kind: "unavailable", error };
  return {
    kind: "unavailable",
    error: new ApiError(0, "network", "The service could not be reached."),
  };
}

/**
 * Resolve the session. `chosen` is the organisation to act in for multi-tenant deployments
 * (`null`: use the remembered one, or the only one, or ask).
 */
export async function loadSession(
  client: ApiClient,
  setTenant: (id: string | null) => void,
  chosen: string | null,
): Promise<SessionState> {
  let session: SessionInfo;
  try {
    setTenant(chosen);
    session = await client.get<SessionInfo>("/auth/session");
  } catch (error) {
    if (error instanceof ApiError) {
      if (error.status === 401) return { kind: "signed_out" };
      if (error.code === "bootstrap_required") {
        return { kind: "bootstrap_required", session: EMPTY_SESSION };
      }
    }
    return asUnavailable(error);
  }
  if (session.credential !== "browser_session") {
    return { kind: "unsupported", credential: session.credential };
  }
  if (session.bootstrap_required)
    return { kind: "bootstrap_required", session };

  let tenants: TenantSummary[] = [];
  try {
    tenants = (await client.get<Page<TenantSummary>>("/tenants")).items;
  } catch (error) {
    if (!(error instanceof ApiError) || error.status !== 403) {
      return asUnavailable(error);
    }
  }
  if (session.tenant !== null) {
    return {
      kind: "ready",
      session,
      tenant: session.tenant,
      tenants,
    };
  }
  // Multi-tenant: pick the remembered organisation if it is still one of the caller's, the only
  // one, or ask.
  const active = tenants.filter((tenant) => tenant.state === "active");
  const wanted = chosen ?? readStoredTenant();
  const pick =
    active.find((tenant) => tenant.id === wanted) ??
    (active.length === 1 ? active[0] : undefined);
  if (pick !== undefined) {
    if (chosen !== pick.id) return loadSession(client, setTenant, pick.id);
  }
  if (active.length === 0) return { kind: "no_tenant", session };
  return { kind: "choose_tenant", session, tenants: active };
}

const EMPTY_SESSION: SessionInfo = {
  credential: "browser_session",
  principal_id: null,
  tenant: null,
  bootstrap_required: true,
  deployment: "single",
};

interface SessionValue {
  state: SessionState;
  client: ApiClient;
  refresh: () => Promise<void>;
  chooseTenant: (id: string) => Promise<void>;
  signOut: () => Promise<void>;
}

const SessionContext = createContext<SessionValue | null>(null);

export function SessionProvider({
  client: provided,
  children,
}: {
  /** Tests inject a client around a fake `fetch`. */
  client?: ApiClient;
  children: ReactNode;
}) {
  const tenantRef = useRef<string | null>(null);
  const client = useMemo(() => provided ?? new ApiClient(), [provided]);
  const [state, setState] = useState<SessionState>({ kind: "loading" });
  const setTenant = useCallback(
    (id: string | null) => {
      tenantRef.current = id;
      client.setTenant(id);
    },
    [client],
  );

  const load = useCallback(
    async (chosen: string | null) => {
      const next = await loadSession(client, setTenant, chosen);
      if (next.kind === "ready" && next.session.deployment !== "single") {
        storeTenant(next.tenant.id);
      }
      setState(next);
    },
    [client, setTenant],
  );

  useEffect(() => {
    let cancelled = false;
    void loadSession(client, setTenant, null).then((next) => {
      if (!cancelled) setState(next);
    });
    return () => {
      cancelled = true;
    };
  }, [client, setTenant]);

  const refresh = useCallback(() => load(tenantRef.current), [load]);
  const chooseTenant = useCallback((id: string) => load(id), [load]);
  const signOut = useCallback(async () => {
    try {
      await client.post("/auth/logout");
    } catch {
      // The server revokes on a best-effort basis; the cookie is cleared by the answer.
    }
    storeTenant(null);
    tenantRef.current = null;
    setState({ kind: "signed_out" });
  }, [client]);

  const value = useMemo(
    () => ({ state, client, refresh, chooseTenant, signOut }),
    [state, client, refresh, chooseTenant, signOut],
  );
  return (
    <SessionContext.Provider value={value}>{children}</SessionContext.Provider>
  );
}

export function useSession(): SessionValue {
  const value = useContext(SessionContext);
  if (value === null) throw new Error("SessionProvider is missing");
  return value;
}

/** Whether the caller's tenant role is one of `roles`. */
export function hasRole(
  tenantRole: string | null,
  roles: readonly string[],
): boolean {
  return tenantRole !== null && roles.includes(tenantRole);
}
