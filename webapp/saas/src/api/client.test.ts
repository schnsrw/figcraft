import { describe, expect, it } from "vitest";

import {
  ApiClient,
  ApiError,
  idempotencyKey,
  parseErrorEnvelope,
} from "./client";
import { FakeApi, apiError } from "../testing";

describe("ApiClient", () => {
  it("sends same-origin JSON requests with the tenant header and no redirects", async () => {
    const api = new FakeApi().on("POST /things", (call) => ({
      status: 201,
      body: { echoed: call.body },
    }));
    const client = api.client();
    client.setTenant("t-1");
    const result = await client.post<{ echoed: unknown }>(
      "/things",
      { a: 1 },
      { headers: { "Idempotency-Key": "k" } },
    );
    expect(result).toEqual({ echoed: { a: 1 } });
    const call = api.calls[0];
    expect(call?.headers["X-FigCraft-Tenant"]).toBe("t-1");
    expect(call?.headers["Content-Type"]).toBe("application/json");
    expect(call?.headers["Idempotency-Key"]).toBe("k");
  });

  it("omits the tenant header when none is chosen and returns undefined for 204", async () => {
    const api = new FakeApi().on("DELETE /things/1", { status: 204 });
    const client = api.client();
    expect(await client.delete("/things/1")).toBeUndefined();
    expect(api.calls[0]?.headers["X-FigCraft-Tenant"]).toBeUndefined();
  });

  it("turns the error envelope into an ApiError with code, fixed message and request id", async () => {
    const api = new FakeApi().on(
      "GET /x",
      apiError(
        409,
        "conflict",
        "The request conflicts with the current state.",
      ),
    );
    const error = await api
      .client()
      .get("/x")
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({
      status: 409,
      code: "conflict",
      message: "The request conflicts with the current state.",
      requestId: "r-test",
    });
  });

  it("keeps bootstrap_required as its own code", async () => {
    const api = new FakeApi().on(
      "GET /x",
      apiError(
        403,
        "bootstrap_required",
        "This deployment must complete bootstrap first.",
      ),
    );
    await expect(api.client().get("/x")).rejects.toMatchObject({
      code: "bootstrap_required",
      status: 403,
    });
  });

  it("reports an unreachable service as network, status 0", async () => {
    const client = new ApiClient({
      fetcher: () => Promise.reject(new TypeError("Failed to fetch")),
    });
    await expect(client.get("/x")).rejects.toMatchObject({
      code: "network",
      status: 0,
    });
  });

  it("never trusts unknown codes, long messages or non-JSON bodies", () => {
    const odd = parseErrorEnvelope(500, {
      error: { code: "<script>", message: "m".repeat(5000), request_id: 7 },
    });
    expect(odd.code).toBe("unknown");
    expect(odd.message.length).toBe(200);
    expect(odd.requestId).toBeNull();
    expect(parseErrorEnvelope(503, "gateway timeout").message).toBe(
      "The service is temporarily unavailable.",
    );
    expect(parseErrorEnvelope(401, null).message).toBe(
      "Authentication is required.",
    );
  });

  it("refuses paths that do not start with a slash", async () => {
    await expect(new FakeApi().client().get("things")).rejects.toMatchObject({
      code: "invalid_request",
    });
  });

  it("makes distinct 32-character idempotency keys", () => {
    const a = idempotencyKey();
    expect(a).toMatch(/^[0-9a-f]{32}$/);
    expect(idempotencyKey()).not.toBe(a);
  });
});
