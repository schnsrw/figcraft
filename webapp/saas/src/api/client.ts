/**
 * A small typed client for the FigCraft Cloud `/api/v1` API.
 *
 * - Same-origin only: requests go to relative URLs with cookies (the session cookie is
 *   `HttpOnly`, so no credential ever passes through script).
 * - Every failure becomes an {@link ApiError} carrying the server's stable `code`, its fixed
 *   public `message` (safe to show) and the request id for support. A transport failure is code
 *   `network`, status `0`. Nothing from a response body other than those three fields is kept.
 * - The selected tenant (multi-tenant deployments) travels in `X-FigCraft-Tenant`.
 */

export type Fetcher = (input: string, init?: RequestInit) => Promise<Response>;

/** Stable error codes the API answers with, plus the client-side `network` and `unknown`. */
export type ApiErrorCode =
  | "invalid_request"
  | "unauthenticated"
  | "forbidden"
  | "not_found"
  | "conflict"
  | "precondition_failed"
  | "resource_inactive"
  | "quota_exceeded"
  | "rate_limited"
  | "temporarily_unavailable"
  | "internal"
  | "invalid_cursor"
  | "bootstrap_required"
  | "network"
  | "unknown";

const KNOWN_CODES: ReadonlySet<string> = new Set<ApiErrorCode>([
  "invalid_request",
  "unauthenticated",
  "forbidden",
  "not_found",
  "conflict",
  "precondition_failed",
  "resource_inactive",
  "quota_exceeded",
  "rate_limited",
  "temporarily_unavailable",
  "internal",
  "invalid_cursor",
  "bootstrap_required",
]);

const MAX_MESSAGE = 200;
const MAX_REQUEST_ID = 64;

export class ApiError extends Error {
  readonly status: number;
  readonly code: ApiErrorCode;
  readonly requestId: string | null;

  constructor(
    status: number,
    code: ApiErrorCode,
    message: string,
    requestId: string | null = null,
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.requestId = requestId;
  }
}

/** The envelope `{"error":{"code","message","request_id"}}`, validated field by field. */
export function parseErrorEnvelope(status: number, body: unknown): ApiError {
  const error =
    typeof body === "object" && body !== null && "error" in body
      ? (body as { error: unknown }).error
      : null;
  if (typeof error === "object" && error !== null) {
    const record = error as Record<string, unknown>;
    const code =
      typeof record.code === "string" && KNOWN_CODES.has(record.code)
        ? (record.code as ApiErrorCode)
        : "unknown";
    const message =
      typeof record.message === "string" && record.message !== ""
        ? record.message.slice(0, MAX_MESSAGE)
        : fallbackMessage(status);
    const requestId =
      typeof record.request_id === "string" && record.request_id !== ""
        ? record.request_id.slice(0, MAX_REQUEST_ID)
        : null;
    return new ApiError(status, code, message, requestId);
  }
  return new ApiError(status, "unknown", fallbackMessage(status));
}

function fallbackMessage(status: number): string {
  if (status === 401) return "Authentication is required.";
  if (status === 403) return "This action is not allowed.";
  if (status === 404) return "The requested resource was not found.";
  if (status === 429) return "Too many requests.";
  if (status >= 500) return "The service is temporarily unavailable.";
  return "The request could not be completed.";
}

export interface RequestOptions {
  signal?: AbortSignal;
  /** Extra headers, e.g. `Idempotency-Key`. */
  headers?: Record<string, string>;
}

export interface ApiClientOptions {
  fetcher?: Fetcher;
  /** Prefix of every path; defaults to `/api/v1`. */
  baseUrl?: string;
  /** The tenant to act in at first (multi-tenant deployments); see `setTenant`. */
  tenant?: string | null;
}

export class ApiClient {
  private readonly fetcher: Fetcher;
  private readonly baseUrl: string;
  private tenantId: string | null;

  constructor(options: ApiClientOptions = {}) {
    this.fetcher = options.fetcher ?? ((input, init) => fetch(input, init));
    this.baseUrl = options.baseUrl ?? "/api/v1";
    this.tenantId = options.tenant ?? null;
  }

  /** Act in `id` from now on (`null`: none, for single-tenant deployments and sign-in). */
  setTenant(id: string | null): void {
    this.tenantId = id;
  }

  get<T>(path: string, options?: RequestOptions): Promise<T> {
    return this.request<T>("GET", path, undefined, options);
  }

  post<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
    return this.request<T>("POST", path, body, options);
  }

  put<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
    return this.request<T>("PUT", path, body, options);
  }

  patch<T>(path: string, body?: unknown, options?: RequestOptions): Promise<T> {
    return this.request<T>("PATCH", path, body, options);
  }

  delete<T>(path: string, options?: RequestOptions): Promise<T> {
    return this.request<T>("DELETE", path, undefined, options);
  }

  async request<T>(
    method: string,
    path: string,
    body?: unknown,
    options: RequestOptions = {},
  ): Promise<T> {
    if (!path.startsWith("/")) {
      throw new ApiError(0, "invalid_request", "The request is invalid.");
    }
    const headers: Record<string, string> = {
      Accept: "application/json",
      ...options.headers,
    };
    const tenant = this.tenantId;
    if (tenant !== null) headers["X-FigCraft-Tenant"] = tenant;
    const init: RequestInit = {
      method,
      headers,
      credentials: "same-origin",
      redirect: "manual",
      cache: "no-store",
    };
    if (options.signal !== undefined) init.signal = options.signal;
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }

    let response: Response;
    try {
      response = await this.fetcher(`${this.baseUrl}${path}`, init);
    } catch (error) {
      if (error instanceof DOMException && error.name === "AbortError") {
        throw error;
      }
      throw new ApiError(0, "network", "The service could not be reached.");
    }

    if (response.status === 204) return undefined as T;
    let parsed: unknown = null;
    const type = response.headers.get("content-type") ?? "";
    if (type.includes("json")) {
      try {
        parsed = await response.json();
      } catch {
        parsed = null;
      }
    }
    if (!response.ok) throw parseErrorEnvelope(response.status, parsed);
    return parsed as T;
  }
}

/** A random key for `Idempotency-Key`, one per user action. */
export function idempotencyKey(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}
