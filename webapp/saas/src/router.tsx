/**
 * Routing on the History API, with stable, shareable URLs.
 *
 * | URL | Screen |
 * |---|---|
 * | `/` | workspace home |
 * | `/workspace/{workspace_id}` | one workspace's projects and files |
 * | `/workspace/{workspace_id}/file/{document_id}` | one file (the editor opens from here) |
 * | `/people` | members, teams and invitations |
 * | `/invite` | accept an invitation (the token arrives in the URL fragment) |
 *
 * Identifiers must be canonical UUIDs; anything else is the not-found route, so a malformed
 * link never reaches the API. The server serves the app for every path (SPA fallback), so every
 * URL above survives a reload and can be shared.
 *
 * Deep links survive sign-in through the server's validated `return_to` (see {@link signInHref}):
 * only a same-origin absolute path is ever sent, and the server checks it again.
 */

import {
  type AnchorHTMLAttributes,
  type MouseEvent,
  type ReactNode,
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from "react";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export type Route =
  | { name: "home" }
  | { name: "workspace"; workspaceId: string }
  | { name: "file"; workspaceId: string; documentId: string }
  | { name: "people" }
  | { name: "invite" }
  | { name: "setup" }
  | { name: "account" }
  | { name: "not_found" };

export function isUuid(value: string): boolean {
  return UUID.test(value);
}

/** Parse a location path (no query or fragment). Trailing slashes are ignored. */
export function parseRoute(pathname: string): Route {
  const parts = pathname.split("/");
  if (parts[0] === "") parts.shift();
  if (parts[parts.length - 1] === "") parts.pop();
  if (parts.length === 0) return { name: "home" };
  if (parts.some((part) => part === "")) return { name: "not_found" };
  const [first, workspaceId, third, documentId, ...rest] = parts;
  if (first === "people" && parts.length === 1) return { name: "people" };
  if (first === "invite" && parts.length === 1) return { name: "invite" };
  if (first === "setup" && parts.length === 1) return { name: "setup" };
  if (first === "account" && parts.length === 1) return { name: "account" };
  if (
    first === "workspace" &&
    workspaceId !== undefined &&
    isUuid(workspaceId)
  ) {
    if (parts.length === 2) return { name: "workspace", workspaceId };
    if (
      third === "file" &&
      documentId !== undefined &&
      isUuid(documentId) &&
      rest.length === 0
    ) {
      return { name: "file", workspaceId, documentId };
    }
  }
  return { name: "not_found" };
}

export function formatRoute(route: Route): string {
  switch (route.name) {
    case "home":
      return "/";
    case "workspace":
      return `/workspace/${route.workspaceId}`;
    case "file":
      return `/workspace/${route.workspaceId}/file/${route.documentId}`;
    case "people":
      return "/people";
    case "invite":
      return "/invite";
    case "setup":
      return "/setup";
    case "account":
      return "/account";
    case "not_found":
      return "/";
  }
}

/** The server's longest accepted `return_to`. */
export const MAX_RETURN_TO = 2048;

/**
 * Whether `path` may be sent as `return_to`: a same-origin absolute path made of printable ASCII,
 * not protocol-relative, without backslashes. This mirrors the server's rule (which still checks
 * it); a path that fails here is replaced by `/` rather than sent.
 */
export function safeReturnTo(path: string): string {
  if (
    path.length === 0 ||
    path.length > MAX_RETURN_TO ||
    !path.startsWith("/") ||
    path.startsWith("//") ||
    path.startsWith("/\\") ||
    path.includes("\\") ||
    !/^[\x21-\x7e]+$/.test(path)
  ) {
    return "/";
  }
  return path;
}

/** The sign-in URL that comes back to `path` (path and query only, never a fragment). */
export function signInHref(path: string): string {
  return `/api/v1/auth/login?return_to=${encodeURIComponent(safeReturnTo(path))}`;
}

interface RouterValue {
  path: string;
  route: Route;
  navigate: (path: string, options?: { replace?: boolean }) => void;
}

const RouterContext = createContext<RouterValue | null>(null);

function currentPath(): string {
  return window.location.pathname + window.location.search;
}

export function RouterProvider({ children }: { children: ReactNode }) {
  const [path, setPath] = useState(currentPath);

  useEffect(() => {
    const onPop = () => setPath(currentPath());
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  const navigate = useCallback(
    (to: string, options?: { replace?: boolean }) => {
      // Only same-origin absolute paths are navigated to from inside the app.
      const target = safeReturnTo(to);
      if (options?.replace === true) {
        window.history.replaceState(null, "", target);
      } else {
        window.history.pushState(null, "", target);
      }
      setPath(currentPath());
      window.scrollTo?.(0, 0);
    },
    [],
  );

  const value = useMemo<RouterValue>(
    () => ({
      path,
      route: parseRoute(path.split("?")[0] ?? "/"),
      navigate,
    }),
    [path, navigate],
  );
  return (
    <RouterContext.Provider value={value}>{children}</RouterContext.Provider>
  );
}

export function useRouter(): RouterValue {
  const value = useContext(RouterContext);
  if (value === null) throw new Error("RouterProvider is missing");
  return value;
}

interface LinkProps extends Omit<
  AnchorHTMLAttributes<HTMLAnchorElement>,
  "href"
> {
  to: string;
}

/** An anchor that navigates in-app on a plain click and stays a real link otherwise. */
export function Link({ to, onClick, children, ...rest }: LinkProps) {
  const { navigate } = useRouter();
  const handle = (event: MouseEvent<HTMLAnchorElement>) => {
    onClick?.(event);
    if (
      event.defaultPrevented ||
      event.button !== 0 ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey ||
      rest.target === "_blank"
    ) {
      return;
    }
    event.preventDefault();
    navigate(to);
  };
  return (
    <a {...rest} href={to} onClick={handle}>
      {children}
    </a>
  );
}
