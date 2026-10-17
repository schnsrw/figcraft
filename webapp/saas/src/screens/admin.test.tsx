import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { App } from "../App";
import { resetLink } from "../api/people";
import {
  FakeApi,
  PRINCIPAL_ID,
  TENANT_ID,
  apiError,
  methodsPassword,
  session,
  tenantList,
} from "../testing";

const OTHER = "66666666-6666-4666-8666-666666666666";
const OIDC_ONLY = "77777777-7777-4777-8777-777777777777";
const T = `/tenants/${TENANT_ID}`;

function page<T>(items: T[]) {
  return { body: { items, next_cursor: null } };
}

const members = [
  {
    principal_id: PRINCIPAL_ID,
    display_name: "Ada Admin",
    role: "tenant_owner",
    state: "active",
  },
  {
    principal_id: OTHER,
    display_name: "Bo Builder",
    role: "editor",
    state: "active",
  },
  {
    principal_id: OIDC_ONLY,
    display_name: "Cy Federated",
    role: "viewer",
    state: "active",
  },
];
const users = [
  {
    principal_id: PRINCIPAL_ID,
    login: "ada",
    display_name: "Ada Admin",
    role: "tenant_owner",
    member_state: "active",
    state: "active",
    locked: false,
  },
  {
    principal_id: OTHER,
    login: "bo",
    display_name: "Bo Builder",
    role: "editor",
    member_state: "active",
    state: "active",
    locked: true,
  },
];

function adminApi(role = "tenant_owner") {
  return new FakeApi()
    .on("GET /auth/session", {
      body: session({ tenant: { id: TENANT_ID, role, permissions: [] } }),
    })
    .on("GET /tenants", { body: tenantList })
    .on("GET " + T + "/members", page(members))
    .on("GET " + T + "/users", page(users))
    .on("GET " + T + "/teams", page([]));
}

async function openPeople(api: FakeApi) {
  window.history.replaceState(null, "", "/people");
  render(<App client={api.client()} />);
  return await screen.findByRole("table", {
    name: "Members of this organisation",
  });
}

describe("running the team from the browser", () => {
  it("shows logins and lock state for password accounts and offers account actions only for them", async () => {
    const table = await openPeople(adminApi());
    expect(
      within(table).getByRole("columnheader", { name: "Login" }),
    ).toBeTruthy();
    expect(within(table).getByText("bo")).toBeTruthy();
    expect(within(table).getByText("Locked")).toBeTruthy();
    expect(
      within(table).getByRole("button", {
        name: "Reset password for Bo Builder",
      }),
    ).toBeTruthy();
    expect(
      within(table).getByRole("button", {
        name: "Sign Bo Builder out everywhere",
      }),
    ).toBeTruthy();
    // A member who signs in through the identity provider has no password to reset.
    expect(
      within(table).queryByRole("button", {
        name: "Reset password for Cy Federated",
      }),
    ).toBeNull();
    expect(
      within(table).getByRole("button", { name: "Disable Cy Federated" }),
    ).toBeTruthy();
  });

  it("adds a person and shows the one-time setup link", async () => {
    const api = adminApi().on("POST " + T + "/users", (call) => ({
      status: 201,
      body: {
        principal_id: "p",
        login: (call.body as { login: string }).login,
        role: "editor",
        setup_token: "setup_TOKEN-1",
        expires_in_secs: 86_400,
      },
    }));
    await openPeople(api);
    await userEvent.click(screen.getByTestId("addperson-open"));
    await userEvent.type(screen.getByTestId("addperson-name"), "Dee Designer");
    await userEvent.type(screen.getByTestId("addperson-login"), "dee");
    await userEvent.selectOptions(
      screen.getByTestId("addperson-role"),
      "commenter",
    );
    await userEvent.click(screen.getByTestId("addperson-submit"));
    const link = await screen.findByTestId("onetime-link");
    expect(link.textContent).toContain("Dee Designer was added.");
    expect(link.textContent).toContain("works once and expires in 24 hours");
    expect(screen.getByTestId("onetime-link-url").textContent).toBe(
      `${window.location.origin}/reset#token=setup_TOKEN-1`,
    );
    expect(
      api.calls.find((c) => c.method === "POST" && c.path.endsWith("/users"))
        ?.body,
    ).toEqual({
      login: "dee",
      display_name: "Dee Designer",
      role: "commenter",
    });
    // The form closed and the list reloads.
    expect(screen.queryByTestId("addperson-form")).toBeNull();
  });

  it("shows why adding a person failed", async () => {
    const api = adminApi().on(
      "POST " + T + "/users",
      apiError(
        409,
        "conflict",
        "The request conflicts with the current state.",
      ),
    );
    await openPeople(api);
    await userEvent.click(screen.getByTestId("addperson-open"));
    await userEvent.type(screen.getByTestId("addperson-name"), "Bo");
    await userEvent.type(screen.getByTestId("addperson-login"), "bo");
    await userEvent.click(screen.getByTestId("addperson-submit"));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "conflicts with the current state",
    );
  });

  it("an admin cannot offer the owner role", async () => {
    await openPeople(adminApi("tenant_admin"));
    await userEvent.click(screen.getByTestId("addperson-open"));
    const owner = within(screen.getByTestId("addperson-role")).getByRole(
      "option",
      { name: "Owner" },
    ) as HTMLOptionElement;
    expect(owner.disabled).toBe(true);
  });

  it("only owners and admins see the add form and account actions", async () => {
    const table = await openPeople(adminApi("editor"));
    expect(screen.queryByTestId("addperson-open")).toBeNull();
    expect(within(table).queryByRole("button")).toBeNull();
  });

  it("issues a reset link, disables and re-enables a password account, and signs a person out everywhere", async () => {
    const api = adminApi()
      .on("POST " + T + "/users/" + OTHER + "/reset-link", {
        status: 201,
        body: {
          reset_token: "reset_TOKEN-2",
          expires_in_secs: 3600,
          purpose: "reset",
        },
      })
      .on("POST " + T + "/users/" + OTHER + "/disable", { status: 204 })
      .on("POST " + T + "/users/" + OTHER + "/enable", { status: 204 })
      .on("POST " + T + "/users/" + OTHER + "/revoke-sessions", {
        body: { revoked: 3 },
      });
    const table = await openPeople(api);
    await userEvent.click(
      within(table).getByRole("button", {
        name: "Reset password for Bo Builder",
      }),
    );
    const link = await screen.findByTestId("onetime-link");
    expect(link.textContent).toContain("expires in 1 hour");
    expect(screen.getByTestId("onetime-link-url").textContent).toBe(
      resetLink(window.location.origin, "reset_TOKEN-2"),
    );
    await userEvent.click(
      within(table).getByRole("button", {
        name: "Sign Bo Builder out everywhere",
      }),
    );
    expect(
      (
        await screen.findByText("Bo Builder was signed out of 3 sessions.")
      ).getAttribute("role"),
    ).toBe("status");
    await userEvent.click(
      within(table).getByRole("button", { name: "Disable Bo Builder" }),
    );
    await waitFor(() =>
      expect(api.count("POST " + T + "/users/" + OTHER + "/disable")).toBe(1),
    );
    // Once the list says disabled, the same control enables again.
    api.on(
      "GET " + T + "/users",
      page([users[0], { ...users[1], state: "disabled", locked: false }]),
    );
    await userEvent.click(
      within(table).getByRole("button", { name: "Disable Bo Builder" }),
    );
    await userEvent.click(
      await within(table).findByRole("button", { name: "Enable Bo Builder" }),
    );
    await waitFor(() =>
      expect(api.count("POST " + T + "/users/" + OTHER + "/enable")).toBe(1),
    );
  });

  it("falls back to member suspension for people without a password account", async () => {
    const api = adminApi().on("PATCH " + T + "/members/" + OIDC_ONLY, {
      status: 204,
    });
    const table = await openPeople(api);
    await userEvent.click(
      within(table).getByRole("button", { name: "Disable Cy Federated" }),
    );
    await waitFor(() =>
      expect(api.calls.find((c) => c.method === "PATCH")?.body).toEqual({
        state: "suspended",
      }),
    );
  });

  it("works when the accounts list is unavailable", async () => {
    const api = adminApi().on(
      "GET " + T + "/users",
      apiError(404, "not_found", "x"),
    );
    const table = await openPeople(api);
    expect(
      within(table).queryByRole("columnheader", { name: "Login" }),
    ).toBeNull();
    expect(
      within(table).getByRole("button", { name: "Disable Bo Builder" }),
    ).toBeTruthy();
  });
});

describe("choosing a password from a link", () => {
  async function fill(password: string, confirm: string) {
    await userEvent.type(await screen.findByTestId("reset-password"), password);
    await userEvent.type(screen.getByTestId("reset-confirm"), confirm);
    await userEvent.click(screen.getByTestId("reset-submit"));
  }

  function resetApi(reply: Parameters<FakeApi["on"]>[1]) {
    return new FakeApi()
      .on("GET /auth/session", apiError(401, "unauthenticated", "x"))
      .on("GET /auth/methods", { body: methodsPassword })
      .on("POST /auth/password/reset", reply)
      .on("POST /auth/logout", { status: 204 });
  }

  it("sets the password from the fragment token, hides the token and continues to sign-in", async () => {
    window.history.replaceState(null, "", "/reset#token=setup_TOKEN-1");
    const api = resetApi({ body: { tenant_id: TENANT_ID } });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Choose your password" }),
    ).toBeTruthy();
    expect(window.location.hash).toBe("");
    expect(window.location.href).not.toContain("setup_TOKEN");
    await fill("a long passphrase", "a long passphrase");
    expect((await screen.findByTestId("reset-done")).textContent).toContain(
      "sign in with it now",
    );
    expect(
      api.calls.find((c) => c.path === "/auth/password/reset")?.body,
    ).toEqual({ token: "setup_TOKEN-1", new_password: "a long passphrase" });
    expect(window.sessionStorage.getItem("figcraft.reset")).toBeNull();
    await userEvent.click(screen.getByTestId("reset-continue"));
    expect(await screen.findByTestId("signin-password-form")).toBeTruthy();
  });

  it("does not send mismatched passwords and shows the policy sentence", async () => {
    window.history.replaceState(null, "", "/reset#token=setup_TOKEN-1");
    const api = resetApi({
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
    await fill("one passphrase", "other passphrase");
    expect((await screen.findByTestId("reset-error")).textContent).toBe(
      "The two passwords don't match.",
    );
    expect(api.count("POST /auth/password/reset")).toBe(0);
    await userEvent.clear(screen.getByTestId("reset-password"));
    await userEvent.clear(screen.getByTestId("reset-confirm"));
    await userEvent.type(screen.getByTestId("reset-password"), "short");
    await userEvent.type(screen.getByTestId("reset-confirm"), "short");
    await userEvent.click(screen.getByTestId("reset-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("reset-error").textContent).toBe(
        "Use at least 12 characters.",
      ),
    );
  });

  it("explains a used, expired or replaced link in one sentence", async () => {
    window.history.replaceState(null, "", "/reset#token=setup_TOKEN-1");
    const api = resetApi(
      apiError(404, "not_found", "The requested resource was not found."),
    );
    render(<App client={api.client()} />);
    await fill("a long passphrase", "a long passphrase");
    expect((await screen.findByTestId("reset-error")).textContent).toContain(
      "Ask your administrator for a new link",
    );
  });

  it("rejects a malformed or missing token", async () => {
    window.history.replaceState(null, "", "/reset#token=%3Cscript%3E");
    const api = resetApi({ body: {} });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Link incomplete" }),
    ).toBeTruthy();
    expect(window.sessionStorage.getItem("figcraft.reset")).toBeNull();
  });

  it("also works for a person who is already signed in", async () => {
    window.history.replaceState(null, "", "/reset#token=setup_TOKEN-1");
    const api = new FakeApi()
      .on("GET /auth/session", { body: session() })
      .on("GET /tenants", { body: tenantList });
    render(<App client={api.client()} />);
    expect(await screen.findByTestId("reset-form")).toBeTruthy();
  });
});
