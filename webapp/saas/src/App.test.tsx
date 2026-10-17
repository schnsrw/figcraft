import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { App } from "./App";
import { DEFAULT_BRANDING, parseBranding } from "./branding";
import { SignInButton } from "./screens/entry";
import {
  DOCUMENT_ID,
  FakeApi,
  TENANT_ID,
  WORKSPACE_ID,
  apiError,
  methodsOidc,
  session,
  tenantList,
} from "./testing";

function signedIn(api: FakeApi, overrides = {}) {
  return api
    .on("GET /auth/session", { body: session(overrides) })
    .on("GET /tenants", { body: tenantList });
}

describe("session states", () => {
  it("shows the signed-in shell once the session loads", async () => {
    const api = signedIn(new FakeApi());
    render(<App client={api.client()} />);
    expect(screen.getByRole("status").textContent).toContain("Loading");
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
    expect(screen.getByRole("navigation", { name: "Main" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Sign out" })).toBeTruthy();
    expect(document.title).toBe("Files · FigCraft");
  });

  it("offers sign-in when signed out and keeps a deep link through it", async () => {
    window.history.replaceState(
      null,
      "",
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    );
    const api = new FakeApi()
      .on(
        "GET /auth/session",
        apiError(401, "unauthenticated", "Authentication is required."),
      )
      .on("GET /auth/methods", { body: methodsOidc });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Sign in to FigCraft" }),
    ).toBeTruthy();
    expect(screen.getByText(/come back to it afterwards/)).toBeTruthy();
    const link = await screen.findByRole("link", {
      name: "Sign in with your organisation account",
    });
    expect(link.getAttribute("href")).toBe(
      `/api/v1/auth/login?return_to=${encodeURIComponent(`/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`)}`,
    );
    // Nothing else is requested for a visitor.
    expect(api.calls.map((call) => call.path)).toEqual([
      "/auth/session",
      "/auth/methods",
    ]);
  });

  it("keeps a percent-encoded path inert inside return_to", async () => {
    window.history.replaceState(null, "", "/%2F%2Fevil.example");
    const api = new FakeApi()
      .on("GET /auth/session", apiError(401, "unauthenticated", "x"))
      .on("GET /auth/methods", { body: methodsOidc });
    render(<App client={api.client()} />);
    const link = await screen.findByRole("link", {
      name: "Sign in with your organisation account",
    });
    expect(link.getAttribute("href")).toBe(
      "/api/v1/auth/login?return_to=%2F%252F%252Fevil.example",
    );
  });

  it("explains a 503 and lets the person retry", async () => {
    let healthy = false;
    const api = new FakeApi()
      .on("GET /auth/session", () =>
        healthy
          ? { body: session() }
          : apiError(
              503,
              "temporarily_unavailable",
              "The service is temporarily unavailable.",
            ),
      )
      .on("GET /tenants", { body: tenantList });
    render(<App client={api.client()} />);
    expect((await screen.findByRole("alert")).textContent).toContain(
      "The service is temporarily unavailable.",
    );
    expect(screen.getByText(/Reference r-test/)).toBeTruthy();
    healthy = true;
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
  });

  it("says so when the service cannot be reached at all", async () => {
    const client = new FakeApi().client();
    const failing = new (await import("./api/client")).ApiClient({
      fetcher: () => Promise.reject(new TypeError("offline")),
    });
    void client;
    render(<App client={failing} />);
    expect(
      await screen.findByRole("heading", { name: "Can't reach the service" }),
    ).toBeTruthy();
  });

  it("asks which organisation when there are several, and remembers the choice", async () => {
    const other = "66666666-6666-4666-8666-666666666666";
    const api = new FakeApi()
      .on("GET /auth/session", (call) =>
        call.headers["X-FigCraft-Tenant"] === undefined
          ? { body: session({ deployment: "multi", tenant: null }) }
          : {
              body: session({
                deployment: "multi",
                tenant: {
                  id: call.headers["X-FigCraft-Tenant"] ?? "",
                  role: "editor",
                  permissions: [],
                },
              }),
            },
      )
      .on("GET /tenants", {
        body: {
          items: [
            {
              id: TENANT_ID,
              slug: "a",
              name: "Acme",
              state: "active",
              role: "editor",
            },
            {
              id: other,
              slug: "b",
              name: "Beta Co",
              state: "active",
              role: "viewer",
            },
          ],
          next_cursor: null,
        },
      });
    const client = api.client();
    render(<App client={client} />);
    expect(
      await screen.findByRole("heading", { name: "Choose an organisation" }),
    ).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: /Beta Co/ }));
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
    expect(window.localStorage.getItem("figcraft.tenant")).toBe(other);
  });

  it("offers to create an organisation in a multi-tenant deployment with none", async () => {
    const api = new FakeApi()
      .on("GET /auth/session", {
        body: session({ deployment: "multi", tenant: null }),
      })
      .on("GET /tenants", { body: { items: [], next_cursor: null } });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", {
        name: "You're not in an organisation yet",
      }),
    ).toBeTruthy();
    expect(screen.getByLabelText("Organisation name")).toBeTruthy();
  });

  it("tells a single-tenant non-member to ask for an invitation, with no create form", async () => {
    const api = new FakeApi()
      .on("GET /auth/session", { body: session({ tenant: null }) })
      .on("GET /tenants", { body: { items: [], next_cursor: null } });
    render(<App client={api.client()} />);
    expect(
      await screen.findByText(/Ask an administrator to invite you/),
    ).toBeTruthy();
    expect(screen.queryByLabelText("Organisation name")).toBeNull();
  });

  it("refuses to browse with a share-link or service credential", async () => {
    const api = new FakeApi().on("GET /auth/session", {
      body: session({ credential: "share_link", principal_id: null }),
    });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", {
        name: "This page is for signed-in people",
      }),
    ).toBeTruthy();
  });

  it("signs out through the API and returns to the sign-in page", async () => {
    const api = signedIn(new FakeApi()).on("POST /auth/logout", {
      status: 204,
    });
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByRole("button", { name: "Sign out" }),
    );
    expect(
      await screen.findByRole("heading", { name: "Sign in to FigCraft" }),
    ).toBeTruthy();
    expect(api.count("POST /auth/logout")).toBe(1);
  });
});

describe("first run", () => {
  function bootstrapApi(complete: Parameters<FakeApi["on"]>[1]) {
    let done = false;
    return new FakeApi()
      .on("GET /auth/session", () => ({
        body: session({
          bootstrap_required: !done,
          tenant: done ? session().tenant : null,
        }),
      }))
      .on("GET /tenants", { body: tenantList })
      .on("POST /auth/bootstrap/complete", async (call) => {
        const reply =
          typeof complete === "function" ? await complete(call) : complete;
        if ((reply.status ?? 200) < 300) done = true;
        return reply;
      });
  }

  it("asks the first signed-in person for the setup token and completes bootstrap", async () => {
    const api = bootstrapApi({
      body: { tenant_id: TENANT_ID, role: "tenant_owner" },
    });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Set up your workspace" }),
    ).toBeTruthy();
    const field = screen.getByLabelText("Setup token") as HTMLInputElement;
    expect(field.type).toBe("password");
    expect(
      (
        screen.getByRole("button", {
          name: "Become the owner",
        }) as HTMLButtonElement
      ).disabled,
    ).toBe(true);
    await userEvent.type(field, "  tok-123  ");
    await userEvent.click(
      screen.getByRole("button", { name: "Become the owner" }),
    );
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
    expect(
      api.calls.find((call) => call.path === "/auth/bootstrap/complete")?.body,
    ).toEqual({ token: "tok-123" });
  });

  it("shows a clear error for a refused token and keeps the form", async () => {
    const api = bootstrapApi(
      apiError(403, "forbidden", "This action is not allowed."),
    );
    render(<App client={api.client()} />);
    await userEvent.type(await screen.findByLabelText("Setup token"), "wrong");
    await userEvent.click(
      screen.getByRole("button", { name: "Become the owner" }),
    );
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("That setup token wasn't accepted");
    expect(
      (screen.getByLabelText("Setup token") as HTMLInputElement).getAttribute(
        "aria-invalid",
      ),
    ).toBe("true");
    expect(
      screen.getByRole("heading", { name: "Set up your workspace" }),
    ).toBeTruthy();
  });

  it("treats a bootstrap_required refusal from the session call as first run too", async () => {
    const api = new FakeApi().on(
      "GET /auth/session",
      apiError(
        403,
        "bootstrap_required",
        "This deployment must complete bootstrap first.",
      ),
    );
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Set up your workspace" }),
    ).toBeTruthy();
  });
});

describe("sign-in button", () => {
  const href = "/api/v1/auth/login?return_to=%2F";

  it("navigates once the server confirms a redirect to the identity provider", async () => {
    const navigate = vi.fn();
    render(
      <SignInButton
        href={href}
        probe={() => Promise.resolve({ ok: true })}
        navigate={navigate}
      />,
    );
    await userEvent.click(screen.getByRole("link", { name: "Sign in" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledWith(href));
  });

  it("explains an identity provider that is not configured (503) and does not navigate", async () => {
    const navigate = vi.fn();
    render(
      <SignInButton
        href={href}
        probe={() => Promise.resolve({ ok: false, status: 503 })}
        navigate={navigate}
      />,
    );
    await userEvent.click(screen.getByRole("link", { name: "Sign in" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain(
      "identity provider is not configured or cannot be reached",
    );
    expect(navigate).not.toHaveBeenCalled();
    // Keyboard users can retry from the same control.
    expect(
      screen
        .getByRole("link", { name: "Sign in" })
        .getAttribute("aria-describedby"),
    ).toBe(alert.id);
  });

  it("explains rate limiting and an unreachable service", async () => {
    const probe = vi
      .fn()
      .mockResolvedValueOnce({ ok: false, status: 429 })
      .mockResolvedValueOnce({ ok: false, status: 0 });
    render(<SignInButton href={href} probe={probe} navigate={vi.fn()} />);
    await userEvent.click(screen.getByRole("link", { name: "Sign in" }));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Too many sign-in attempts",
    );
    await userEvent.click(screen.getByRole("link", { name: "Sign in" }));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toContain(
        "could not be reached",
      ),
    );
  });

  it("can be activated from the keyboard", async () => {
    const navigate = vi.fn();
    render(
      <SignInButton
        href={href}
        probe={() => Promise.resolve({ ok: true })}
        navigate={navigate}
      />,
    );
    screen.getByRole("link", { name: "Sign in" }).focus();
    await userEvent.keyboard("{Enter}");
    await waitFor(() => expect(navigate).toHaveBeenCalledWith(href));
  });
});

describe("routing and focus", () => {
  it("shows not-found for malformed links without calling the API for them", async () => {
    window.history.replaceState(null, "", "/workspace/not-a-uuid");
    const api = signedIn(new FakeApi());
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Page not found" }),
    ).toBeTruthy();
    expect(document.title).toBe("Page not found · FigCraft");
  });

  it("navigates in-app, moves focus to the new content and keeps the URL stable", async () => {
    const api = signedIn(new FakeApi());
    render(<App client={api.client()} />);
    await screen.findByRole("heading", { name: "Your files" });
    await userEvent.click(screen.getByRole("link", { name: "People" }));
    expect(window.location.pathname).toBe("/people");
    expect(await screen.findByRole("heading", { name: "People" })).toBeTruthy();
    expect(document.activeElement?.id).toBe("main");
    window.history.back();
    await waitFor(() => expect(window.location.pathname).toBe("/"));
  });

  it("has a skip link as the first focusable control", async () => {
    const api = signedIn(new FakeApi());
    render(<App client={api.client()} />);
    await screen.findByRole("heading", { name: "Your files" });
    await userEvent.tab();
    expect(document.activeElement?.textContent).toBe("Skip to content");
  });
});

describe("branding", () => {
  it("renders the default brand without a footer", async () => {
    const api = signedIn(new FakeApi());
    const { container } = render(
      <App branding={DEFAULT_BRANDING} client={api.client()} />,
    );
    await screen.findByRole("heading", { name: "Your files" });
    expect(screen.getByLabelText("FigCraft workspace home")).toBeTruthy();
    expect(container.querySelector(".brand-mark")).not.toBeNull();
    expect(container.querySelector("footer")).toBeNull();
  });

  it("renders a validated custom brand, escaping the name", async () => {
    const branding = parseBranding({
      productName: 'Acme "Design" </script>',
      logoUrl: "/brand/logo.svg",
      supportUrl: "https://help.example.com/",
      privacyUrl: "javascript:alert(1)",
      footerText: "© Acme",
    });
    const api = signedIn(new FakeApi());
    const { container } = render(
      <App branding={branding} client={api.client()} />,
    );
    await screen.findByRole("heading", { name: "Your files" });
    expect(container.querySelector(".brand span")?.textContent).toBe(
      'Acme "Design" </script>',
    );
    expect(container.querySelector("img")?.getAttribute("src")).toBe(
      "/brand/logo.svg",
    );
    expect(
      container.querySelector('a[href="https://help.example.com/"]'),
    ).not.toBeNull();
    expect(container.innerHTML).not.toContain("javascript:");
    expect(screen.queryByText("Privacy")).toBeNull();
    expect(screen.getByText("© Acme")).toBeTruthy();
  });
});
