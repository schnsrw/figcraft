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

const palette = ["#e5484d", "#30a46c", "#3e63dd"];
const profile = {
  principal_id: PRINCIPAL_ID,
  display_name: "Ada Admin",
  color: "#30a46c",
  color_chosen: true,
  palette,
  tenant_id: TENANT_ID,
  login: "ada",
};
const now = Math.floor(Date.now() / 1000);
const sessions = [
  {
    id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    created_at: now - 3600,
    last_seen_at: now - 60,
    idle_expires_at: now + 3600,
    absolute_expires_at: now + 86400,
    current: true,
  },
  {
    id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    created_at: now - 7200,
    last_seen_at: now - 600,
    idle_expires_at: now + 3600,
    absolute_expires_at: now + 86400,
    current: false,
  },
];

function accountApi() {
  return new FakeApi()
    .on("GET /auth/session", { body: session() })
    .on("GET /tenants", { body: tenantList })
    .on("GET /me", { body: profile })
    .on("GET /me/sessions", { body: { items: sessions } });
}

async function open(api: FakeApi) {
  window.history.replaceState(null, "", "/account");
  render(<App client={api.client()} />);
  await screen.findByRole("heading", { name: "Your account" });
}

describe("profile", () => {
  it("shows the login and lets the person rename themselves", async () => {
    const api = accountApi().on("PATCH /me", (call) => ({
      body: {
        ...profile,
        display_name: (call.body as { display_name: string }).display_name,
      },
    }));
    await open(api);
    expect((await screen.findByTestId("profile-login")).textContent).toBe(
      "ada",
    );
    const save = screen.getByTestId("profile-save") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    const field = screen.getByTestId("profile-name");
    await userEvent.clear(field);
    await userEvent.type(field, "  Ada L.  ");
    await userEvent.click(save);
    expect((await screen.findByText("Saved.")).getAttribute("role")).toBe(
      "status",
    );
    expect(api.calls.find((c) => c.method === "PATCH")?.body).toEqual({
      display_name: "Ada L.",
    });
  });

  it("picks a colour from the palette with a radio group", async () => {
    const api = accountApi().on("PATCH /me", (call) => ({
      body: { ...profile, color: (call.body as { color: string }).color },
    }));
    await open(api);
    const group = await screen.findByRole("radiogroup", {
      name: "Your colour",
    });
    const radios = within(group).getAllByRole("radio");
    expect(radios.map((r) => r.getAttribute("aria-checked"))).toEqual([
      "false",
      "true",
      "false",
    ]);
    await userEvent.click(screen.getByTestId("colour-#3e63dd"));
    await waitFor(() =>
      expect(
        screen.getByTestId("colour-#3e63dd").getAttribute("aria-checked"),
      ).toBe("true"),
    );
    expect(api.calls.find((c) => c.method === "PATCH")?.body).toEqual({
      color: "#3e63dd",
    });
  });

  it("shows the server's refusal", async () => {
    const api = accountApi().on(
      "PATCH /me",
      apiError(400, "invalid_request", "The request is invalid."),
    );
    await open(api);
    const field = await screen.findByTestId("profile-name");
    await userEvent.type(field, "x");
    await userEvent.click(screen.getByTestId("profile-save"));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "The request is invalid.",
    );
  });
});

describe("change password", () => {
  async function fill(current: string, next: string, confirm: string) {
    await userEvent.type(
      await screen.findByTestId("password-current"),
      current,
    );
    await userEvent.type(screen.getByTestId("password-new"), next);
    await userEvent.type(screen.getByTestId("password-confirm"), confirm);
    await userEvent.click(screen.getByTestId("password-submit"));
  }

  it("changes it and says how many other sessions were signed out", async () => {
    const api = accountApi().on("POST /me/password", {
      body: { revoked_sessions: 2 },
    });
    await open(api);
    await fill("old password", "a better password", "a better password");
    expect((await screen.findByTestId("password-done")).textContent).toContain(
      "2 other sessions were signed out",
    );
    expect(api.calls.find((c) => c.path === "/me/password")?.body).toEqual({
      current_password: "old password",
      new_password: "a better password",
    });
    expect((screen.getByTestId("password-new") as HTMLInputElement).value).toBe(
      "",
    );
  });

  it("does not send mismatched passwords", async () => {
    const api = accountApi();
    await open(api);
    await fill("old", "one password", "another one");
    expect((await screen.findByTestId("password-error")).textContent).toBe(
      "The two new passwords don't match.",
    );
    expect(api.count("POST /me/password")).toBe(0);
  });

  it("explains a wrong current password and a policy failure", async () => {
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
    const api = accountApi().on(
      "POST /me/password",
      () => replies.shift() ?? { body: { revoked_sessions: 0 } },
    );
    await open(api);
    await fill("nope", "new password!!", "new password!!");
    expect((await screen.findByTestId("password-error")).textContent).toBe(
      "Your current password isn't right.",
    );
    await userEvent.type(screen.getByTestId("password-current"), "right");
    await userEvent.click(screen.getByTestId("password-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("password-error").textContent).toBe(
        "Use at least 12 characters.",
      ),
    );
  });
});

describe("sessions", () => {
  it("lists sessions, marks this browser and revokes another one", async () => {
    const api = accountApi().on(
      "DELETE /me/sessions/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
      { status: 204 },
    );
    await open(api);
    const list = await screen.findByRole("list", { name: "Sessions" });
    expect(within(list).getByText("This browser")).toBeTruthy();
    expect(within(list).getByText("Another browser")).toBeTruthy();
    expect(list.textContent).not.toContain("bbbbbbbb");
    await userEvent.click(
      within(list).getByRole("button", { name: /Sign out the session from/ }),
    );
    await waitFor(() =>
      expect(
        api.count("DELETE /me/sessions/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
      ).toBe(1),
    );
    expect((await screen.findByText("Signed out.")).getAttribute("role")).toBe(
      "status",
    );
  });

  it("signs out everywhere else", async () => {
    const api = accountApi().on("POST /me/sessions/revoke-others", {
      body: { revoked: 1 },
    });
    await open(api);
    await userEvent.click(await screen.findByTestId("sessions-revoke-others"));
    expect(
      (await screen.findByText("Signed out of 1 other session.")).getAttribute(
        "role",
      ),
    ).toBe("status");
  });

  it("signing out of this browser returns to the sign-in page", async () => {
    const api = accountApi()
      .on("POST /auth/logout", { status: 204 })
      .on("GET /auth/methods", { body: methodsPassword });
    await open(api);
    await userEvent.click(
      await screen.findByRole("button", { name: "Sign out of this browser" }),
    );
    expect(await screen.findByTestId("signin-password-form")).toBeTruthy();
  });

  it("hides the sign-out-everywhere control when there is only this browser", async () => {
    const api = accountApi().on("GET /me/sessions", {
      body: { items: [sessions[0]] },
    });
    await open(api);
    await screen.findByRole("list", { name: "Sessions" });
    expect(screen.queryByTestId("sessions-revoke-others")).toBeNull();
  });
});
