import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { App } from "../App";
import { invitationLink, shareEditorUrl } from "../api/people";
import {
  DOCUMENT_ID,
  FakeApi,
  PRINCIPAL_ID,
  TENANT_ID,
  apiError,
  methodsOidc,
  session,
  tenantList,
} from "../testing";

const OTHER = "66666666-6666-4666-8666-666666666666";
const THIRD = "77777777-7777-4777-8777-777777777777";
const TEAM = "88888888-8888-4888-8888-888888888888";
const T = `/tenants/${TENANT_ID}`;

function page<T>(items: T[]) {
  return { body: { items, next_cursor: null } };
}

function base(role: string) {
  return new FakeApi()
    .on("GET /auth/session", {
      body: session({ tenant: { id: TENANT_ID, role, permissions: [] } }),
    })
    .on("GET /tenants", { body: tenantList });
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
    principal_id: THIRD,
    display_name: "Cy Viewer",
    role: "viewer",
    state: "suspended",
  },
];

function peopleApi(role: string) {
  return base(role)
    .on("GET " + T + "/members", page(members))
    .on(
      "GET " + T + "/teams",
      page([{ id: TEAM, slug: "design", name: "Design", state: "active" }]),
    )
    .on(
      "GET " + T + "/teams/" + TEAM + "/members",
      page([
        {
          principal_id: OTHER,
          display_name: "Bo Builder",
          role: "team_manager",
        },
      ]),
    )
    .on(
      "GET " + T + "/invitations",
      page([
        {
          id: "99999999-9999-4999-8999-999999999999",
          team_id: TEAM,
          email: "new@acme.test",
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
  await screen.findByRole("heading", { name: "People" });
}

describe("members", () => {
  it("lists members by name with roles and lets an owner change a role", async () => {
    const api = peopleApi("tenant_owner").on(
      "PATCH " + T + "/members/" + OTHER,
      { status: 204 },
    );
    await openPeople(api);
    const table = await screen.findByRole("table", {
      name: "Members of this organisation",
    });
    expect(
      within(table).getByRole("rowheader", { name: /Ada Admin/ }),
    ).toBeTruthy();
    expect(within(table).getByText("(you)")).toBeTruthy();
    expect(within(table).getByText("Suspended")).toBeTruthy();
    expect(table.textContent).not.toContain(OTHER);
    const select = within(table).getByLabelText(
      "Role of Bo Builder",
    ) as HTMLSelectElement;
    await userEvent.selectOptions(select, "tenant_admin");
    await waitFor(() =>
      expect(api.calls.find((c) => c.method === "PATCH")?.body).toEqual({
        role: "tenant_admin",
      }),
    );
  });

  it("an admin cannot touch owners or grant ownership", async () => {
    const api = peopleApi("tenant_admin");
    await openPeople(api);
    const table = await screen.findByRole("table", {
      name: "Members of this organisation",
    });
    expect(within(table).queryByLabelText("Role of Ada Admin")).toBeNull();
    const select = within(table).getByLabelText("Role of Bo Builder");
    const owner = within(select).getByRole("option", {
      name: "Owner",
    }) as HTMLOptionElement;
    expect(owner.disabled).toBe(true);
    expect(
      within(table).queryByRole("button", { name: "Remove Ada Admin" }),
    ).toBeNull();
  });

  it("a plain member sees the list with no controls and an explanation", async () => {
    const api = peopleApi("editor");
    await openPeople(api);
    const table = await screen.findByRole("table", {
      name: "Members of this organisation",
    });
    expect(within(table).queryAllByRole("combobox")).toHaveLength(0);
    expect(within(table).queryByRole("button")).toBeNull();
    expect(
      screen.getByText(/Only owners and admins can change roles/),
    ).toBeTruthy();
  });

  it("suspends and removes with a confirmation, surfacing a refusal", async () => {
    const api = peopleApi("tenant_owner")
      .on("PATCH " + T + "/members/" + OTHER, { status: 204 })
      .on(
        "DELETE " + T + "/members/" + OTHER,
        apiError(
          409,
          "conflict",
          "The request conflicts with the current state.",
        ),
      );
    await openPeople(api);
    await userEvent.click(
      await screen.findByRole("button", { name: "Suspend Bo Builder" }),
    );
    await waitFor(() =>
      expect(
        api.calls.some(
          (c) =>
            c.method === "PATCH" &&
            JSON.stringify(c.body) === '{"state":"suspended"}',
        ),
      ).toBe(true),
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Remove Bo Builder" }),
    );
    const dialog = screen.getByRole("dialog", { name: "Remove member" });
    expect(
      within(dialog).getByText(/lose access to everything here/),
    ).toBeTruthy();
    await userEvent.click(
      within(dialog).getByRole("button", { name: "Remove" }),
    );
    expect((await screen.findByRole("alert")).textContent).toContain(
      "conflicts with the current state",
    );
  });

  it("filters by name", async () => {
    await openPeople(peopleApi("tenant_owner"));
    await userEvent.type(await screen.findByLabelText("Find a member"), "cy");
    const table = screen.getByRole("table", {
      name: "Members of this organisation",
    });
    expect(within(table).queryByText(/Bo Builder/)).toBeNull();
    expect(
      within(table).getByRole("rowheader", { name: /Cy Viewer/ }),
    ).toBeTruthy();
  });
});

describe("tabs", () => {
  it("moves between tabs with the arrow keys and keeps one tab stop", async () => {
    await openPeople(peopleApi("tenant_owner"));
    const tabs = screen.getAllByRole("tab");
    expect(tabs.map((tab) => tab.getAttribute("tabindex"))).toEqual([
      "0",
      "-1",
      "-1",
    ]);
    tabs[0]?.focus();
    await userEvent.keyboard("{ArrowRight}");
    expect(
      screen.getByRole("tab", { name: "Teams" }).getAttribute("aria-selected"),
    ).toBe("true");
    expect(document.activeElement).toBe(
      screen.getByRole("tab", { name: "Teams" }),
    );
    await userEvent.keyboard("{End}");
    expect(document.activeElement).toBe(
      screen.getByRole("tab", { name: "Invitations" }),
    );
    await userEvent.keyboard("{ArrowRight}");
    expect(document.activeElement).toBe(
      screen.getByRole("tab", { name: "Members" }),
    );
    const panel = screen.getByRole("tabpanel");
    expect(panel.getAttribute("aria-labelledby")).toBe(
      screen.getByRole("tab", { name: "Members" }).id,
    );
  });
});

describe("teams", () => {
  it("shows a team's members by name and lets a team manager change roles", async () => {
    const api = peopleApi("editor").on(
      "PATCH " + T + "/teams/" + TEAM + "/members/" + OTHER,
      { status: 204 },
    );
    // The caller is not the manager here: read-only.
    await openPeople(api);
    await userEvent.click(screen.getByRole("tab", { name: "Teams" }));
    await userEvent.click(
      await screen.findByRole("button", { name: /Design/ }),
    );
    const table = await screen.findByRole("table", {
      name: "Members of Design",
    });
    expect(within(table).getByText("Bo Builder")).toBeTruthy();
    expect(within(table).queryByRole("combobox")).toBeNull();
    expect(screen.queryByLabelText("New team")).toBeNull();
  });

  it("an owner creates a team, adds a person and removes one", async () => {
    const created: unknown[] = [];
    const api = peopleApi("tenant_owner")
      .on("POST " + T + "/teams", (call) => {
        created.push(call.body);
        return {
          status: 201,
          body: {
            id: TEAM,
            slug: "brand-team",
            name: "Brand team",
            state: "active",
          },
        };
      })
      .on("POST " + T + "/teams/" + TEAM + "/members", { status: 201 })
      .on("DELETE " + T + "/teams/" + TEAM + "/members/" + OTHER, {
        status: 204,
      });
    await openPeople(api);
    await userEvent.click(screen.getByRole("tab", { name: "Teams" }));
    await userEvent.type(
      await screen.findByLabelText("New team"),
      "Brand team",
    );
    await userEvent.click(screen.getByRole("button", { name: "Add team" }));
    await waitFor(() =>
      expect(created).toEqual([{ name: "Brand team", slug: "brand-team" }]),
    );
    await userEvent.click(
      await screen.findByRole("button", { name: /Design/ }),
    );
    await screen.findByRole("table", { name: "Members of Design" });
    // Only people not already in the team can be added, shown by name.
    const add = screen.getByLabelText("Add to team") as HTMLSelectElement;
    expect(Array.from(add.options).map((option) => option.text)).toEqual([
      "Choose a person…",
      "Ada Admin",
    ]);
    await userEvent.selectOptions(add, PRINCIPAL_ID);
    await userEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() =>
      expect(
        api.calls.find(
          (c) =>
            c.path.endsWith("/teams/" + TEAM + "/members") &&
            c.method === "POST",
        )?.body,
      ).toEqual({ principal_id: PRINCIPAL_ID, role: "viewer" }),
    );
    await userEvent.click(
      screen.getByRole("button", { name: "Remove Bo Builder from team" }),
    );
    await waitFor(() =>
      expect(
        api.count("DELETE " + T + "/teams/" + TEAM + "/members/" + OTHER),
      ).toBe(1),
    );
  });
});

describe("invitations", () => {
  it("creates an invitation and shows the link once, with the token in the fragment", async () => {
    const api = peopleApi("tenant_admin").on(
      "POST " + T + "/teams/" + TEAM + "/invitations",
      (call) => ({
        status: 201,
        body: {
          id: "99999999-9999-4999-8999-999999999998",
          token: "tok_ABC-123_xyz",
          echoed: call.body,
        },
      }),
    );
    await openPeople(api);
    await userEvent.click(screen.getByRole("tab", { name: "Invitations" }));
    await userEvent.type(
      await screen.findByLabelText("Email address"),
      "new.person@acme.test",
    );
    await userEvent.selectOptions(screen.getByLabelText("Team"), TEAM);
    await userEvent.selectOptions(screen.getByLabelText("Team role"), "editor");
    await userEvent.click(
      screen.getByRole("button", { name: "Create invitation" }),
    );
    const status = await screen.findByRole("status");
    expect(status.textContent).toContain(
      `${window.location.origin}/invite#token=tok_ABC-123_xyz`,
    );
    expect(status.textContent).toContain("won't be shown again");
    expect(
      api.calls.find(
        (c) => c.method === "POST" && c.path.endsWith("/invitations"),
      )?.body,
    ).toEqual({ email: "new.person@acme.test", role: "editor" });
    expect(
      screen.getByRole("list", { name: "Pending invitations" }).textContent,
    ).toContain("new@acme.test");
  });

  it("revokes a pending invitation and surfaces a refusal", async () => {
    const api = peopleApi("tenant_owner").on(
      "DELETE " +
        T +
        "/teams/" +
        TEAM +
        "/invitations/99999999-9999-4999-8999-999999999999",
      apiError(403, "forbidden", "This action is not allowed."),
    );
    await openPeople(api);
    await userEvent.click(screen.getByRole("tab", { name: "Invitations" }));
    await userEvent.click(
      await screen.findByRole("button", {
        name: "Revoke invitation for new@acme.test",
      }),
    );
    expect((await screen.findByRole("alert")).textContent).toContain(
      "This action is not allowed.",
    );
  });

  it("tells non-admins to ask an admin", async () => {
    await openPeople(peopleApi("editor"));
    await userEvent.click(screen.getByRole("tab", { name: "Invitations" }));
    expect(
      await screen.findByText(/Only owners and admins can invite people/),
    ).toBeTruthy();
    expect(screen.queryByLabelText("Email address")).toBeNull();
  });

  it("asks for a team first when none exists", async () => {
    const api = peopleApi("tenant_owner").on("GET " + T + "/teams", page([]));
    await openPeople(api);
    await userEvent.click(screen.getByRole("tab", { name: "Invitations" }));
    expect(await screen.findByText(/Create a team first/)).toBeTruthy();
  });
});

describe("accepting an invitation", () => {
  it("keeps the fragment token out of the URL, accepts once signed in and joins", async () => {
    window.history.replaceState(null, "", "/invite#token=tok_ABC-123_xyz");
    const accepted: unknown[] = [];
    const api = new FakeApi()
      .on("GET /auth/session", { body: session({ tenant: null }) })
      .on("GET /tenants", { body: { items: [], next_cursor: null } })
      .on("POST /invitations/accept", (call) => {
        accepted.push(call.body);
        return {
          body: {
            tenant_id: TENANT_ID,
            team_id: TEAM,
            team_role: "viewer",
            joined_tenant: true,
          },
        };
      });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "You've been invited" }),
    ).toBeTruthy();
    expect(window.location.hash).toBe("");
    expect(window.location.pathname).toBe("/invite");
    await userEvent.click(
      screen.getByRole("button", { name: "Accept invitation" }),
    );
    expect((await screen.findByRole("status")).textContent).toContain(
      "joined the organisation",
    );
    expect(accepted).toEqual([{ token: "tok_ABC-123_xyz" }]);
    expect(window.sessionStorage.getItem("figcraft.invite")).toBeNull();
  });

  it("survives the sign-in round trip through session storage", async () => {
    window.history.replaceState(null, "", "/invite#token=tok_ABC-123_xyz");
    const api = new FakeApi()
      .on("GET /auth/session", apiError(401, "unauthenticated", "x"))
      .on("GET /auth/methods", { body: methodsOidc });
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByRole("button", { name: "Sign in instead" }),
    );
    const link = await screen.findByRole("link", {
      name: "Sign in with your organisation account",
    });
    expect(link.getAttribute("href")).toBe(
      "/api/v1/auth/login?return_to=%2Finvite",
    );
    expect(window.location.href).not.toContain("tok_");
    expect(window.sessionStorage.getItem("figcraft.invite")).toBe(
      "tok_ABC-123_xyz",
    );
  });

  it("explains an unusable invitation without revealing why", async () => {
    window.sessionStorage.setItem("figcraft.invite", "tok_ABC-123_xyz");
    window.history.replaceState(null, "", "/invite");
    const api = new FakeApi()
      .on("GET /auth/session", { body: session({ tenant: null }) })
      .on("GET /tenants", { body: { items: [], next_cursor: null } })
      .on(
        "POST /invitations/accept",
        apiError(404, "not_found", "The requested resource was not found."),
      );
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByRole("button", { name: "Accept invitation" }),
    );
    expect((await screen.findByRole("alert")).textContent).toContain(
      "different email address",
    );
    expect(
      (
        screen.getByRole("button", {
          name: "Accept invitation",
        }) as HTMLButtonElement
      ).disabled,
    ).toBe(false);
  });

  it("rejects malformed tokens and a link with no token", async () => {
    window.history.replaceState(null, "", "/invite#token=%3Cscript%3E");
    const api = new FakeApi()
      .on("GET /auth/session", { body: session({ tenant: null }) })
      .on("GET /tenants", { body: { items: [], next_cursor: null } });
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", {
        name: "Invitation link incomplete",
      }),
    ).toBeTruthy();
    expect(window.sessionStorage.getItem("figcraft.invite")).toBeNull();
    expect(window.location.hash).toBe("");
  });
});

describe("link builders", () => {
  it("builds the same editor address the server does", () => {
    expect(
      shareEditorUrl(
        "http://127.0.0.1:8080/",
        "b1300000-0000-4000-8000-000000000001",
        "abc-_XYZ",
      ),
    ).toBe(
      "http://127.0.0.1:8080/edit/#join=ws%3A%2F%2F127.0.0.1%3A8080%2Fws%2Fv1%2Fcollab%3Ffile%3Db1300000-0000-4000-8000-000000000001%26token%3Dabc-_XYZ",
    );
    expect(
      shareEditorUrl("https://figcraft.example", DOCUMENT_ID, "t")?.startsWith(
        "https://figcraft.example/edit/#join=wss%3A%2F%2Ffigcraft.example%2Fws%2Fv1%2Fcollab",
      ),
    ).toBe(true);
    expect(shareEditorUrl("ftp://x", DOCUMENT_ID, "t")).toBeNull();
    expect(shareEditorUrl("not a url", DOCUMENT_ID, "t")).toBeNull();
    expect(invitationLink("https://figcraft.example", "a_b-c")).toBe(
      "https://figcraft.example/invite#token=a_b-c",
    );
  });
});
