import { ApiClient, type Fetcher } from "./api/client";
import type { SessionInfo } from "./api/types";

export interface Call {
  method: string;
  /** Path below `/api/v1`, query included. */
  path: string;
  body: unknown;
  headers: Record<string, string>;
}

export interface Reply {
  status?: number;
  body?: unknown;
}

export type Handler = Reply | ((call: Call) => Reply | Promise<Reply>);

export class FakeApi {
  readonly calls: Call[] = [];
  private readonly handlers = new Map<string, Handler>();

  /** `route` is `"METHOD /path"`, matched on the path without its query. */
  on(route: string, handler: Handler): this {
    this.handlers.set(route, handler);
    return this;
  }

  readonly fetcher: Fetcher = async (input, init) => {
    const method = init?.method ?? "GET";
    const path = input.replace(/^\/api\/v1/, "");
    const headers = Object.fromEntries(
      Object.entries((init?.headers ?? {}) as Record<string, string>),
    );
    const body =
      typeof init?.body === "string"
        ? (JSON.parse(init.body) as unknown)
        : undefined;
    const call: Call = { method, path, body, headers };
    this.calls.push(call);
    const handler = this.handlers.get(`${method} ${path.split("?")[0]}`);
    if (handler === undefined) {
      return json(404, {
        error: {
          code: "not_found",
          message: "The requested resource was not found.",
          request_id: "r-test",
        },
      });
    }
    const reply = typeof handler === "function" ? await handler(call) : handler;
    return json(reply.status ?? 200, reply.body);
  };

  client(): ApiClient {
    return new ApiClient({ fetcher: this.fetcher });
  }

  count(route: string): number {
    return this.calls.filter(
      (call) => `${call.method} ${call.path.split("?")[0]}` === route,
    ).length;
  }
}

export function json(status: number, body: unknown): Response {
  if (status === 204 || body === undefined) {
    return new Response(null, { status });
  }
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export function apiError(status: number, code: string, message: string): Reply {
  return { status, body: { error: { code, message, request_id: "r-test" } } };
}

export const TENANT_ID = "11111111-1111-4111-8111-111111111111";
export const WORKSPACE_ID = "22222222-2222-4222-8222-222222222222";
export const PROJECT_ID = "33333333-3333-4333-8333-333333333333";
export const DOCUMENT_ID = "44444444-4444-4444-8444-444444444444";
export const PRINCIPAL_ID = "55555555-5555-4555-8555-555555555555";

export function session(overrides: Partial<SessionInfo> = {}): SessionInfo {
  return {
    credential: "browser_session",
    principal_id: PRINCIPAL_ID,
    tenant: { id: TENANT_ID, role: "tenant_owner", permissions: [] },
    session_expires_in: 3600,
    bootstrap_required: false,
    deployment: "single",
    ...overrides,
  };
}

export const tenantList = {
  items: [
    {
      id: TENANT_ID,
      slug: "acme",
      name: "Acme Design",
      state: "active",
      role: "tenant_owner",
    },
  ],
  next_cursor: null,
};
