import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { App } from "../App";
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
const TEAM = "88888888-8888-4888-8888-888888888888";
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
    must_change_password: false,
  },
  {
    principal_id: OTHER,
    login: "bo",
    display_name: "Bo Builder",
    role: "editor",
    member_state: "active",
    state: "active",
    locked: false,
    must_change_password: true,
  },
];

function adminApi() {
  return new FakeApi()
    .on("GET /auth/session", { body: session() })
    .on("GET /tenants", { body: tenantList })
    .on("GET " + T + "/members", page(members))
    .on("GET " + T + "/users", page(users))
    .on(
      "GET " + T + "/teams",
      page([{ id: TEAM, slug: "design", name: "Design", state: "active" }]),
    )
    .on(
      "GET " + T + "/invitations",
      page([
        {
          id: "99999999-9999-4999-8999-999999999999",
          team_id: TEAM,
          email: null,
          role: "viewer",
          invited_by: PRINCIPAL_ID,
          expires_in_secs: 3600,
        },
      ]),
    );
}

async function openPeople(api: FakeApi) {
  window.history.replaceState(null, "", "/people");
  render(<App client={api.client()} />);
  return await screen.findByRole("table", {
    name: "Members of this organisation",
  });
}

describe("temporary passwords set by an administrator", () => {
  it("adds a person with a generated temporary password shown once", async () => {
    const api = adminApi().on("POST " + T + "/users", {
      status: 201,
      body: {
        principal_id: "p",
        login: "dee",
        role: "editor",
        must_change_password: true,
        generated_password: "Gen-Pass-123",
      },
    });
    await openPeople(api);
    await userEvent.click(screen.getByTestId("addperson-open"));
    await userEvent.type(screen.getByTestId("addperson-name"), "Dee");
    await userEvent.type(screen.getByTestId("addperson-login"), "dee");
    await userEvent.click(screen.getByTestId("addperson-how-generate"));
    await userEvent.click(screen.getByTestId("addperson-submit"));
    const box = await screen.findByTestId("onetime-password");
    expect(screen.getByTestId("onetime-password-value").textContent).toBe(
      "Gen-Pass-123",
    );
    expect(box.textContent).toContain("won't be shown again");
    expect(box.textContent).toContain("must choose their own");
    expect(
      api.calls.find((c) => c.method === "POST" && c.path.endsWith("/users"))
        ?.body,
    ).toEqual({
      login: "dee",
      display_name: "Dee",
      role: "editor",
      generate_password: true,
    });
  });

  it("adds a person with a typed temporary password and never echoes it", async () => {
    const api = adminApi().on("POST " + T + "/users", {
      status: 201,
      body: {
        principal_id: "p",
        login: "dee",
        role: "editor",
        must_change_password: true,
      },
    });
    await openPeople(api);
    await userEvent.click(screen.getByTestId("addperson-open"));
    await userEvent.type(screen.getByTestId("addperson-name"), "Dee");
    await userEvent.type(screen.getByTestId("addperson-login"), "dee");
    await userEvent.click(screen.getByTestId("addperson-how-typed"));
    expect(
      (screen.getByTestId("addperson-submit") as HTMLButtonElement).disabled,
    ).toBe(true);
    await userEvent.type(
      screen.getByTestId("addperson-temp-password"),
      "typed-temp-pass",
    );
    await userEvent.click(screen.getByTestId("addperson-submit"));
    expect(
      (
        await screen.findByText(
          /Dee was added with the temporary password you typed/,
        )
      ).getAttribute("role"),
    ).toBe("status");
    expect(document.body.textContent).not.toContain("typed-temp-pass");
    expect(
      api.calls.find((c) => c.method === "POST" && c.path.endsWith("/users"))
        ?.body,
    ).toMatchObject({ initial_password: "typed-temp-pass" });
  });

  it("shows who still has a temporary password and sets a new one", async () => {
    const api = adminApi().on("POST " + T + "/users/" + OTHER + "/password", {
      body: { must_change_password: true, generated_password: "New-Temp-9" },
    });
    const table = await openPeople(api);
    expect(within(table).getByTestId("must-change-badge").textContent).toBe(
      "Must change password",
    );
    // Owners are never offered a password reset.
    expect(
      within(table).queryByRole("button", {
        name: "Set a new password for Ada Admin",
      }),
    ).toBeNull();
    await userEvent.click(
      within(table).getByRole("button", {
        name: "Set a new password for Bo Builder",
      }),
    );
    const dialog = screen.getByRole("dialog", {
      name: "Set a new password for Bo Builder",
    });
    expect(
      within(dialog).getByText(/must choose their own at next sign-in/),
    ).toBeTruthy();
    await userEvent.click(within(dialog).getByTestId("setpw-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("onetime-password-value").textContent).toBe(
        "New-Temp-9",
      ),
    );
    expect(api.calls.find((c) => c.path.endsWith("/password"))?.body).toEqual({
      generate: true,
    });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("types a temporary password for someone and shows a refusal", async () => {
    const api = adminApi().on(
      "POST " + T + "/users/" + OTHER + "/password",
      apiError(403, "forbidden", "This action is not allowed."),
    );
    const table = await openPeople(api);
    await userEvent.click(
      within(table).getByRole("button", {
        name: "Set a new password for Bo Builder",
      }),
    );
    await userEvent.click(screen.getByTestId("setpw-typed"));
    await userEvent.type(
      screen.getByTestId("setpw-password"),
      "typed-temp-pass",
    );
    await userEvent.click(screen.getByTestId("setpw-submit"));
    expect((await screen.findByTestId("setpw-error")).textContent).toContain(
      "This action is not allowed.",
    );
    expect(api.calls.find((c) => c.path.endsWith("/password"))?.body).toEqual({
      password: "typed-temp-pass",
    });
    expect(screen.getByRole("dialog")).toBeTruthy();
  });
});

describe("link-only invitations", () => {
  it("creates one without an email and lists it as link-only", async () => {
    const api = adminApi().on("POST " + T + "/teams/" + TEAM + "/invitations", {
      status: 201,
      body: {
        id: "99999999-9999-4999-8999-999999999998",
        token: "tok_ABC-123_xyz",
      },
    });
    window.history.replaceState(null, "", "/people");
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByRole("tab", { name: "Invitations" }),
    );
    await userEvent.selectOptions(await screen.findByLabelText("Team"), TEAM);
    await userEvent.click(
      screen.getByRole("button", { name: "Create invitation" }),
    );
    expect((await screen.findByRole("status")).textContent).toContain(
      "Link-only invitation created.",
    );
    expect(
      api.calls.find(
        (c) => c.method === "POST" && c.path.endsWith("/invitations"),
      )?.body,
    ).toEqual({ role: "viewer" });
    expect(
      screen.getByRole("list", { name: "Pending invitations" }).textContent,
    ).toContain("Link-only invitation");
    expect(
      screen.getByRole("button", {
        name: "Revoke invitation for link-only invitation",
      }),
    ).toBeTruthy();
  });
});

describe("must change password at sign-in", () => {
  function mustChangeApi(reply: Parameters<FakeApi["on"]>[1]) {
    const state = { changed: false };
    const api = new FakeApi()
      .on("GET /auth/session", () => ({
        body: session({ must_change_password: !state.changed }),
      }))
      .on("GET /tenants", { body: tenantList })
      .on("GET /auth/methods", { body: methodsPassword })
      .on("POST /me/password", async (call) => {
        const result = typeof reply === "function" ? await reply(call) : reply;
        if ((result.status ?? 200) < 300) state.changed = true;
        return result;
      });
    return api;
  }

  it("shows only the change-password screen until a new password is chosen", async () => {
    const api = mustChangeApi({ body: { revoked_sessions: 0 } });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Choose your own password" }),
    ).toBeTruthy();
    expect(screen.getByTestId("mustchange-intro").textContent).toContain(
      "temporary password",
    );
    // The workspace is not reachable meanwhile: no navigation, no tenant requests.
    expect(screen.queryByRole("navigation", { name: "Main" })).toBeNull();
    expect(api.count("GET /tenants")).toBe(0);
    await userEvent.type(screen.getByTestId("mustchange-current"), "Temp-123");
    await userEvent.type(
      screen.getByTestId("mustchange-new"),
      "my own long passphrase",
    );
    await userEvent.type(
      screen.getByTestId("mustchange-confirm"),
      "my own long passphrase",
    );
    await userEvent.click(screen.getByTestId("mustchange-submit"));
    expect(
      await screen.findByRole("heading", { name: "Your files" }),
    ).toBeTruthy();
    expect(api.calls.find((c) => c.path === "/me/password")?.body).toEqual({
      current_password: "Temp-123",
      new_password: "my own long passphrase",
    });
  });

  it("explains a wrong temporary password and a weak new one, and can sign out", async () => {
    const replies = [
      apiError(403, "forbidden", "This action is not allowed."),
      {
        status: 400,
        body: {
          error: {
            code: "invalid_request",
            reason: "password_policy",
            message: "Use at least 12 characters.",
            request_id: "r",
          },
        },
      },
    ];
    const api = mustChangeApi(
      () => replies.shift() ?? { body: { revoked_sessions: 0 } },
    ).on("POST /auth/logout", { status: 204 });
    render(<App client={api.client()} />);
    await userEvent.type(
      await screen.findByTestId("mustchange-current"),
      "wrong",
    );
    await userEvent.type(screen.getByTestId("mustchange-new"), "short");
    await userEvent.type(screen.getByTestId("mustchange-confirm"), "short");
    await userEvent.click(screen.getByTestId("mustchange-submit"));
    expect((await screen.findByTestId("mustchange-error")).textContent).toBe(
      "The temporary password isn't right.",
    );
    await userEvent.type(screen.getByTestId("mustchange-current"), "Temp-123");
    await userEvent.click(screen.getByTestId("mustchange-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("mustchange-error").textContent).toBe(
        "Use at least 12 characters.",
      ),
    );
    await userEvent.click(
      within(screen.getByTestId("mustchange-form")).getByRole("button", {
        name: "Sign out",
      }),
    );
    expect(await screen.findByTestId("signin-password-form")).toBeTruthy();
  });

  it("does not send mismatched passwords", async () => {
    const api = mustChangeApi({ body: { revoked_sessions: 0 } });
    render(<App client={api.client()} />);
    await userEvent.type(
      await screen.findByTestId("mustchange-current"),
      "Temp-123",
    );
    await userEvent.type(
      screen.getByTestId("mustchange-new"),
      "one passphrase",
    );
    await userEvent.type(
      screen.getByTestId("mustchange-confirm"),
      "two passphrase",
    );
    await userEvent.click(screen.getByTestId("mustchange-submit"));
    expect((await screen.findByTestId("mustchange-error")).textContent).toBe(
      "The two new passwords don't match.",
    );
    expect(api.count("POST /me/password")).toBe(0);
  });
});
