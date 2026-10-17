import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { App } from "../App";
import { passwordProblem, waitText } from "./signin";
import { ApiError } from "../api/client";
import {
  FakeApi,
  TENANT_ID,
  apiError,
  methodsBoth,
  methodsOidc,
  methodsPassword,
  session,
  tenantList,
} from "../testing";

const unauthenticated = apiError(
  401,
  "unauthenticated",
  "Authentication is required.",
);

/** A signed-out deployment whose session becomes valid once `signedIn.value` is true. */
function passwordApi(methods: object = methodsPassword) {
  const state = { signedIn: false };
  const api = new FakeApi()
    .on("GET /auth/session", () =>
      state.signedIn ? { body: session() } : unauthenticated,
    )
    .on("GET /tenants", { body: tenantList })
    .on("GET /auth/methods", { body: methods });
  return { api, state };
}

async function fillLogin(login: string, password: string) {
  await userEvent.type(await screen.findByTestId("signin-login"), login);
  await userEvent.type(screen.getByTestId("signin-password"), password);
}

describe("password sign-in", () => {
  it("shows a login form and no organisation button when only passwords are on", async () => {
    const { api } = passwordApi();
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Sign in to FigCraft" }),
    ).toBeTruthy();
    expect(screen.getByLabelText("Login")).toBeTruthy();
    expect(screen.getByLabelText("Password")).toBeTruthy();
    expect(screen.queryByTestId("signin-oidc")).toBeNull();
    expect(
      (screen.getByTestId("signin-submit") as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("signs in with a login and password and lands signed in on the same URL", async () => {
    window.history.replaceState(null, "", "/people");
    const { api, state } = passwordApi();
    api.on("POST /auth/password/login", () => {
      state.signedIn = true;
      return { body: { principal_id: "p" } };
    });
    api.on("GET " + `/tenants/${TENANT_ID}/members`, {
      body: { items: [], next_cursor: null },
    });
    render(<App client={api.client()} />);
    await fillLogin("  ada  ", "correct horse");
    await userEvent.click(screen.getByTestId("signin-submit"));
    expect(await screen.findByRole("heading", { name: "People" })).toBeTruthy();
    expect(
      api.calls.find((c) => c.path === "/auth/password/login")?.body,
    ).toEqual({
      login: "ada",
      password: "correct horse",
    });
    expect(window.location.pathname).toBe("/people");
    // No credential is kept anywhere the page can read it back.
    expect(window.localStorage.length).toBeLessThanOrEqual(1);
    expect(JSON.stringify(Object.entries(window.sessionStorage))).not.toContain(
      "correct horse",
    );
  });

  it("answers every failure with the same sentence and clears the password", async () => {
    const { api } = passwordApi();
    api.on(
      "POST /auth/password/login",
      apiError(401, "unauthenticated", "Authentication is required."),
    );
    render(<App client={api.client()} />);
    await fillLogin("ada", "wrong");
    await userEvent.click(screen.getByTestId("signin-submit"));
    expect((await screen.findByTestId("signin-error")).textContent).toBe(
      "That login or password isn't right.",
    );
    expect(
      (screen.getByTestId("signin-password") as HTMLInputElement).value,
    ).toBe("");
    expect((screen.getByTestId("signin-login") as HTMLInputElement).value).toBe(
      "ada",
    );
  });

  it("tells a throttled person how long to wait", async () => {
    const { api } = passwordApi();
    api.on("POST /auth/password/login", {
      status: 429,
      body: {
        error: {
          code: "rate_limited",
          message: "Too many requests.",
          request_id: "r",
        },
      },
      headers: { "retry-after": "120" },
    });
    render(<App client={api.client()} />);
    await fillLogin("ada", "x");
    await userEvent.click(screen.getByTestId("signin-submit"));
    expect((await screen.findByTestId("signin-error")).textContent).toBe(
      "Too many attempts. Try again in about 2 minutes.",
    );
  });

  it("explains a workspace that turned passwords off (403)", async () => {
    const { api } = passwordApi();
    api.on(
      "POST /auth/password/login",
      apiError(403, "forbidden", "This action is not allowed."),
    );
    render(<App client={api.client()} />);
    await fillLogin("ada", "x");
    await userEvent.click(screen.getByTestId("signin-submit"));
    expect((await screen.findByTestId("signin-error")).textContent).toContain(
      "turned off for this workspace",
    );
  });

  it("offers both methods when both are on, and only the organisation button when passwords are off", async () => {
    const both = passwordApi(methodsBoth);
    const { unmount } = render(<App client={both.api.client()} />);
    expect(await screen.findByTestId("signin-oidc")).toBeTruthy();
    expect(screen.getByTestId("signin-password-form")).toBeTruthy();
    unmount();
    const oidc = passwordApi(methodsOidc);
    render(<App client={oidc.api.client()} />);
    expect(await screen.findByTestId("signin-oidc")).toBeTruthy();
    expect(screen.queryByTestId("signin-password-form")).toBeNull();
  });

  it("says so when no method is on", async () => {
    const { api } = passwordApi({ ...methodsOidc, oidc: false });
    render(<App client={api.client()} />);
    expect((await screen.findByRole("alert")).textContent).toContain(
      "No sign-in method is turned on",
    );
  });

  it("asks which workspace in a multi-tenant deployment, then offers its methods", async () => {
    const { api } = passwordApi();
    api.on("GET /auth/methods", (call) =>
      call.path.includes("tenant=acme")
        ? { body: methodsPassword }
        : apiError(404, "not_found", "The requested resource was not found."),
    );
    api.on("POST /auth/password/login", apiError(401, "unauthenticated", "x"));
    render(<App client={api.client()} />);
    await userEvent.type(await screen.findByTestId("signin-workspace"), "nope");
    await userEvent.click(screen.getByRole("button", { name: "Continue" }));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "couldn't find that workspace",
    );
    const field = screen.getByTestId("signin-workspace");
    await userEvent.clear(field);
    await userEvent.type(field, "acme");
    await userEvent.click(screen.getByRole("button", { name: "Continue" }));
    await fillLogin("ada", "x");
    await userEvent.click(screen.getByTestId("signin-submit"));
    await waitFor(() =>
      expect(
        api.calls.find((c) => c.path === "/auth/password/login")?.body,
      ).toMatchObject({ tenant: "acme", login: "ada" }),
    );
    // The (non-secret) workspace name is remembered for next time.
    expect(window.localStorage.getItem("figcraft.workspace")).toBe("acme");
  });

  it("explains a service that cannot be reached and retries", async () => {
    let up = false;
    const api = new FakeApi()
      .on("GET /auth/session", unauthenticated)
      .on("GET /auth/methods", () =>
        up
          ? { body: methodsPassword }
          : apiError(
              503,
              "temporarily_unavailable",
              "The service is temporarily unavailable.",
            ),
      );
    render(<App client={api.client()} />);
    expect((await screen.findByRole("alert")).textContent).toContain(
      "temporarily unavailable",
    );
    up = true;
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByTestId("signin-password-form")).toBeTruthy();
  });

  it("points a fresh deployment at first-run setup", async () => {
    const { api } = passwordApi({
      ...methodsPassword,
      bootstrap_required: true,
    });
    render(<App client={api.client()} />);
    const note = await screen.findByTestId("signin-first-run");
    expect(note.textContent).toContain("hasn't been set up yet");
    expect(
      screen
        .getByRole("link", { name: "Set up the first administrator" })
        .getAttribute("href"),
    ).toBe("/setup");
  });
});

describe("first-run owner setup with a password", () => {
  function setupApi(reply: Parameters<FakeApi["on"]>[1]) {
    const state = { signedIn: false };
    const api = new FakeApi()
      .on("GET /auth/session", () =>
        state.signedIn ? { body: session() } : unauthenticated,
      )
      .on("GET /tenants", { body: tenantList })
      .on("GET /auth/methods", { body: methodsPassword })
      .on("POST /auth/password/bootstrap", async (call) => {
        const result = typeof reply === "function" ? await reply(call) : reply;
        if ((result.status ?? 200) < 300) state.signedIn = true;
        return result;
      });
    return api;
  }

  async function fill(over: { token?: string; confirm?: string } = {}) {
    await userEvent.type(
      await screen.findByTestId("setup-token"),
      over.token ?? "tok-123",
    );
    await userEvent.type(screen.getByTestId("setup-name"), "Ada Admin");
    await userEvent.type(screen.getByTestId("setup-login"), "ada");
    await userEvent.type(
      screen.getByTestId("setup-password"),
      "a long passphrase",
    );
    await userEvent.type(
      screen.getByTestId("setup-confirm"),
      over.confirm ?? "a long passphrase",
    );
  }

  it("creates the owner and signs them in", async () => {
    window.history.replaceState(null, "", "/setup");
    const api = setupApi({
      body: { tenant_id: TENANT_ID, role: "tenant_owner" },
    });
    render(<App client={api.client()} />);
    await fill();
    await userEvent.click(screen.getByTestId("setup-submit"));
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
    expect(
      api.calls.find((c) => c.path === "/auth/password/bootstrap")?.body,
    ).toEqual({
      token: "tok-123",
      login: "ada",
      display_name: "Ada Admin",
      password: "a long passphrase",
    });
    expect(window.location.pathname).toBe("/");
  });

  it("does not send anything when the two passwords differ", async () => {
    window.history.replaceState(null, "", "/setup");
    const api = setupApi({ body: {} });
    render(<App client={api.client()} />);
    await fill({ confirm: "something else" });
    await userEvent.click(screen.getByTestId("setup-submit"));
    expect((await screen.findByTestId("setup-error")).textContent).toBe(
      "The two passwords don't match.",
    );
    expect(api.count("POST /auth/password/bootstrap")).toBe(0);
  });

  it("shows the server's password-policy sentence and clears the password fields", async () => {
    window.history.replaceState(null, "", "/setup");
    const api = setupApi({
      status: 400,
      body: {
        error: {
          code: "invalid_request",
          reason: "password_policy",
          message: "Use at least 12 characters.",
          request_id: "r",
        },
      },
    });
    render(<App client={api.client()} />);
    await fill();
    await userEvent.click(screen.getByTestId("setup-submit"));
    expect((await screen.findByTestId("setup-error")).textContent).toBe(
      "Use at least 12 characters.",
    );
    expect(
      (screen.getByTestId("setup-password") as HTMLInputElement).value,
    ).toBe("");
  });

  it("explains a refused token and an already-configured deployment", async () => {
    window.history.replaceState(null, "", "/setup");
    const refused = setupApi(
      apiError(403, "forbidden", "This action is not allowed."),
    );
    const first = render(<App client={refused.client()} />);
    await fill({ token: "wrong" });
    await userEvent.click(screen.getByTestId("setup-submit"));
    expect((await screen.findByTestId("setup-error")).textContent).toContain(
      "setup token wasn't accepted",
    );
    first.unmount();
    const done = setupApi(apiError(404, "not_found", "x"));
    render(<App client={done.client()} />);
    await fill();
    await userEvent.click(screen.getByTestId("setup-submit"));
    expect(
      await screen.findByRole("heading", { name: "Already set up" }),
    ).toBeTruthy();
  });

  it("is reachable from the sign-in page and tells signed-in people it is done", async () => {
    window.history.replaceState(null, "", "/setup");
    const api = new FakeApi()
      .on("GET /auth/session", { body: session() })
      .on("GET /tenants", { body: tenantList });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Already set up" }),
    ).toBeTruthy();
  });
});

describe("accepting an invitation with a password", () => {
  function inviteApi(reply: Parameters<FakeApi["on"]>[1]) {
    const state = { signedIn: false };
    const api = new FakeApi()
      .on("GET /auth/session", () =>
        state.signedIn ? { body: session() } : unauthenticated,
      )
      .on("GET /tenants", { body: tenantList })
      .on("GET /auth/methods", { body: methodsPassword })
      .on("POST /auth/password/signup", async (call) => {
        const result = typeof reply === "function" ? await reply(call) : reply;
        if ((result.status ?? 200) < 300) state.signedIn = true;
        return result;
      });
    return api;
  }

  async function fill(confirm = "a long passphrase") {
    await userEvent.type(
      await screen.findByTestId("invite-name"),
      "Bo Builder",
    );
    await userEvent.type(
      screen.getByTestId("invite-password"),
      "a long passphrase",
    );
    await userEvent.type(screen.getByTestId("invite-confirm"), confirm);
  }

  it("creates the account from the link's token and signs in", async () => {
    window.history.replaceState(null, "", "/invite#token=tok_ABC-123_xyz");
    const api = inviteApi({
      status: 201,
      body: { principal_id: "p", tenant_id: TENANT_ID },
    });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", {
        name: "You've been invited to FigCraft",
      }),
    ).toBeTruthy();
    expect(window.location.hash).toBe("");
    await fill();
    await userEvent.click(screen.getByTestId("invite-submit"));
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
    expect(
      api.calls.find((c) => c.path === "/auth/password/signup")?.body,
    ).toEqual({
      invitation: "tok_ABC-123_xyz",
      display_name: "Bo Builder",
      password: "a long passphrase",
    });
    expect(window.sessionStorage.getItem("figcraft.invite")).toBeNull();
  });

  it("explains a spent invitation, a taken login and a weak password", async () => {
    window.history.replaceState(null, "", "/invite#token=tok_ABC-123_xyz");
    const replies = [
      apiError(404, "not_found", "x"),
      apiError(409, "conflict", "x"),
      {
        status: 400,
        body: {
          error: {
            code: "invalid_request",
            reason: "password_policy",
            message: "Too short.",
            request_id: "r",
          },
        },
      },
    ];
    const api = inviteApi(() => replies.shift() ?? { body: {} });
    render(<App client={api.client()} />);
    await fill();
    for (const expected of ["can't be used", "already exists", "Too short."]) {
      await userEvent.click(screen.getByTestId("invite-submit"));
      await waitFor(() =>
        expect(screen.getByTestId("invite-error").textContent).toContain(
          expected,
        ),
      );
      await userEvent.type(
        screen.getByTestId("invite-password"),
        "a long passphrase",
      );
      await userEvent.type(
        screen.getByTestId("invite-confirm"),
        "a long passphrase",
      );
    }
  });

  it("refuses mismatched passwords locally", async () => {
    window.history.replaceState(null, "", "/invite#token=tok_ABC-123_xyz");
    const api = inviteApi({ body: {} });
    render(<App client={api.client()} />);
    await fill("different");
    await userEvent.click(screen.getByTestId("invite-submit"));
    expect((await screen.findByTestId("invite-error")).textContent).toBe(
      "The two passwords don't match.",
    );
    expect(api.count("POST /auth/password/signup")).toBe(0);
  });

  it("lets a person with an account sign in and then accept", async () => {
    window.history.replaceState(null, "", "/invite#token=tok_ABC-123_xyz");
    const state = { signedIn: false };
    const api = new FakeApi()
      .on("GET /auth/session", () =>
        state.signedIn ? { body: session({ tenant: null }) } : unauthenticated,
      )
      .on("GET /tenants", { body: { items: [], next_cursor: null } })
      .on("GET /auth/methods", { body: methodsPassword })
      .on("POST /auth/password/login", () => {
        state.signedIn = true;
        return { body: {} };
      })
      .on("POST /invitations/accept", {
        body: {
          tenant_id: TENANT_ID,
          team_id: "t",
          team_role: "viewer",
          joined_tenant: true,
        },
      });
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByRole("button", { name: "Sign in instead" }),
    );
    await fillLogin("bo", "pw-pw-pw-pw");
    await userEvent.click(screen.getByTestId("signin-submit"));
    await userEvent.click(
      await screen.findByRole("button", { name: "Accept invitation" }),
    );
    expect((await screen.findByRole("status")).textContent).toContain("joined");
  });
});

describe("wording helpers", () => {
  it("formats waits and password problems", () => {
    expect(waitText(30)).toBe("30 seconds");
    expect(waitText(1)).toBe("1 second");
    expect(waitText(61)).toBe("61 seconds");
    expect(waitText(600)).toBe("about 10 minutes");
    expect(waitText(null)).toBe("a little while");
    expect(
      passwordProblem(
        new ApiError(
          400,
          "invalid_request",
          "Use 12 characters.",
          null,
          "password_policy",
        ),
      ),
    ).toBe("Use 12 characters.");
    expect(
      passwordProblem(
        new ApiError(400, "invalid_request", "The request is invalid."),
      ),
    ).toBe("Check the fields and try again.");
  });
});
