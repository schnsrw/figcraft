import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { App } from "../App";
import {
  DOCUMENT_ID,
  FakeApi,
  PRINCIPAL_ID,
  PROJECT_ID,
  TENANT_ID,
  WORKSPACE_ID,
  apiError,
  session,
  tenantList,
} from "../testing";

const OTHER = "66666666-6666-4666-8666-666666666666";
const TEAM = "88888888-8888-4888-8888-888888888888";
const LINK = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const T = `/tenants/${TENANT_ID}`;
const D = `/documents/${DOCUMENT_ID}`;

function page<T>(items: T[]) {
  return { body: { items, next_cursor: null } };
}

function api(over: { can?: object } = {}) {
  return new FakeApi()
    .on("GET /auth/session", { body: session() })
    .on("GET /tenants", { body: tenantList })
    .on("GET " + D, {
      body: {
        id: DOCUMENT_ID,
        workspace_id: WORKSPACE_ID,
        project_id: PROJECT_ID,
        name: "Homepage",
        state: "active",
        role: "workspace_admin",
        can_edit: true,
        can: {
          edit: true,
          rename: true,
          archive: true,
          restore: true,
          share: true,
          ...over.can,
        },
      },
    })
    .on("GET /workspaces/" + WORKSPACE_ID, {
      body: {
        id: WORKSPACE_ID,
        name: "Brand",
        state: "active",
        owning_team_id: null,
      },
    })
    .on(
      "GET " + T + "/members",
      page([
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
      ]),
    )
    .on(
      "GET " + T + "/teams",
      page([{ id: TEAM, slug: "design", name: "Design", state: "active" }]),
    )
    .on(
      "GET " + D + "/members",
      page([{ principal_id: OTHER, role: "editor" }]),
    )
    .on("GET " + D + "/team-grants", page([{ team_id: TEAM, role: "viewer" }]))
    .on("GET " + D + "/share-links", {
      body: {
        items: [
          {
            id: LINK,
            role: "viewer",
            label: "Review link",
            created_by: PRINCIPAL_ID,
            created_at: 1,
            expires_at: 4_000_000_000,
            revoked_at: null,
          },
          {
            id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            role: "editor",
            label: "Old",
            created_by: PRINCIPAL_ID,
            created_at: 1,
            expires_at: null,
            revoked_at: 5,
          },
        ],
      },
    });
}

async function openShare(fake: FakeApi) {
  window.history.replaceState(
    null,
    "",
    `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
  );
  render(<App client={fake.client()} />);
  await userEvent.click(await screen.findByRole("button", { name: "Share" }));
  return await screen.findByRole("dialog", { name: "Share “Homepage”" });
}

describe("share dialog", () => {
  it("shows people, teams and links by name and hides revoked links", async () => {
    const dialog = await openShare(api());
    const peopleList = await within(dialog).findByRole("list", {
      name: "People",
    });
    expect(within(peopleList).getByText("Bo Builder")).toBeTruthy();
    expect(dialog.textContent).not.toContain(OTHER);
    expect(
      within(
        await within(dialog).findByRole("list", { name: "Teams" }),
      ).getByText("Design"),
    ).toBeTruthy();
    const links = await within(dialog).findByRole("list", { name: "Links" });
    expect(within(links).getByText("Review link")).toBeTruthy();
    expect(within(links).queryByText("Old")).toBeNull();
    expect(
      within(dialog).getByText(
        /Anyone with a link can open this file without signing in/,
      ),
    ).toBeTruthy();
  });

  it("is offered only to people who can share", async () => {
    window.history.replaceState(
      null,
      "",
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    );
    render(<App client={api({ can: { share: false } }).client()} />);
    await screen.findByRole("heading", { name: /Homepage/ });
    expect(screen.queryByRole("button", { name: "Share" })).toBeNull();
  });

  it("changes, adds and removes direct access", async () => {
    const fake = api()
      .on("PUT " + D + "/members/" + OTHER, {
        body: { principal_id: OTHER, role: "commenter" },
      })
      .on("PUT " + D + "/members/" + PRINCIPAL_ID, { body: {} })
      .on("DELETE " + D + "/members/" + OTHER, { status: 204 });
    const dialog = await openShare(fake);
    const select = await within(dialog).findByLabelText(
      "Access for Bo Builder",
    );
    await userEvent.selectOptions(select, "commenter");
    await waitFor(() =>
      expect(
        fake.calls.find(
          (c) => c.method === "PUT" && c.path.endsWith("/members/" + OTHER),
        )?.body,
      ).toEqual({ role: "commenter" }),
    );
    // Only people without access yet can be added.
    const add = within(dialog).getByLabelText(
      "Add a person",
    ) as HTMLSelectElement;
    expect(Array.from(add.options).map((o) => o.text)).toEqual([
      "Choose…",
      "Ada Admin",
    ]);
    await userEvent.selectOptions(add, PRINCIPAL_ID);
    await userEvent.click(
      within(dialog).getByRole("button", { name: "Add to people" }),
    );
    await waitFor(() =>
      expect(fake.count("PUT " + D + "/members/" + PRINCIPAL_ID)).toBe(1),
    );
    await userEvent.click(
      within(dialog).getByRole("button", { name: "Remove Bo Builder" }),
    );
    await waitFor(() =>
      expect(fake.count("DELETE " + D + "/members/" + OTHER)).toBe(1),
    );
  });

  it("shows the server's refusal when a change is not allowed", async () => {
    const fake = api().on(
      "PUT " + D + "/members/" + OTHER,
      apiError(403, "forbidden", "This action is not allowed."),
    );
    const dialog = await openShare(fake);
    await userEvent.selectOptions(
      await within(dialog).findByLabelText("Access for Bo Builder"),
      "viewer",
    );
    expect((await within(dialog).findByRole("alert")).textContent).toContain(
      "This action is not allowed.",
    );
  });

  it("grants and removes team access", async () => {
    const fake = api().on("DELETE " + D + "/team-grants/" + TEAM, {
      status: 204,
    });
    const dialog = await openShare(fake);
    await userEvent.click(
      await within(dialog).findByRole("button", { name: "Remove Design" }),
    );
    await waitFor(() =>
      expect(fake.count("DELETE " + D + "/team-grants/" + TEAM)).toBe(1),
    );
  });

  it("creates a link, shows its address once, and revokes one", async () => {
    const fake = api()
      .on("POST " + D + "/share-links", (call) => ({
        status: 201,
        body: {
          id: LINK,
          role: "viewer",
          secret: "SECRET_abc-123",
          sent: call.body,
        },
      }))
      .on("DELETE " + D + "/share-links/" + LINK, { status: 204 });
    const dialog = await openShare(fake);
    await userEvent.type(
      await within(dialog).findByLabelText("Label (optional)"),
      "For Sam",
    );
    await userEvent.selectOptions(
      within(dialog).getByLabelText("Expires after"),
      "86400",
    );
    await userEvent.click(
      within(dialog).getByRole("button", { name: "Create link" }),
    );
    const status = await within(dialog).findByRole("status");
    expect(status.textContent).toContain("/edit/#join=");
    expect(status.textContent).toContain("SECRET_abc-123");
    expect(
      fake.calls.find(
        (c) => c.method === "POST" && c.path.endsWith("/share-links"),
      )?.body,
    ).toEqual({ role: "viewer", label: "For Sam", expires_in_secs: 86400 });
    await userEvent.click(
      within(dialog).getByRole("button", { name: "Revoke Review link" }),
    );
    await waitFor(() =>
      expect(fake.count("DELETE " + D + "/share-links/" + LINK)).toBe(1),
    );
  });

  it("does not offer an editor link to someone who cannot edit the file", async () => {
    const dialog = await openShare(api({ can: { edit: false } }));
    const access = (await within(dialog).findByLabelText(
      "Link access",
    )) as HTMLSelectElement;
    expect(
      (
        within(access).getByRole("option", {
          name: "Can edit",
        }) as HTMLOptionElement
      ).disabled,
    ).toBe(true);
  });

  it("closes with Escape and returns focus to the Share button", async () => {
    const dialog = await openShare(api());
    await within(dialog).findByRole("list", { name: "People" });
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Share" }),
    );
  });
});
