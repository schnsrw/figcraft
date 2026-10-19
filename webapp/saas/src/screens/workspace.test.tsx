import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { App } from "../App";
import { readRecents } from "../recents";
import { pageLoad } from "../router";
import {
  DOCUMENT_ID,
  FakeApi,
  PROJECT_ID,
  TENANT_ID,
  WORKSPACE_ID,
  apiError,
  session,
  tenantList,
} from "../testing";

const adminCan = {
  create_project: true,
  rename: true,
  archive: true,
  restore: true,
  manage_members: true,
};
const viewerCan = {
  create_project: false,
  rename: false,
  archive: false,
  restore: false,
  manage_members: false,
};
const projectCan = {
  create_document: true,
  rename: true,
  archive: true,
  restore: true,
};
const noProjectCan = {
  create_document: false,
  rename: false,
  archive: false,
  restore: false,
};

function page<T>(items: T[]) {
  return { body: { items, next_cursor: null } };
}

function base(role = "tenant_owner") {
  return new FakeApi()
    .on("GET /auth/session", {
      body: session({ tenant: { id: TENANT_ID, role, permissions: [] } }),
    })
    .on("GET /tenants", { body: tenantList });
}

const workspaceRow = (over = {}) => ({
  id: WORKSPACE_ID,
  name: "Brand",
  state: "active",
  owning_team_id: null,
  ...over,
});
const projectRow = (over = {}) => ({
  id: PROJECT_ID,
  workspace_id: WORKSPACE_ID,
  name: "Website",
  state: "active",
  ...over,
});
const fileRow = (over = {}) => ({
  id: DOCUMENT_ID,
  workspace_id: WORKSPACE_ID,
  project_id: PROJECT_ID,
  name: "Homepage",
  state: "active",
  ...over,
});

describe("workspace home", () => {
  it("lists workspaces, hides archived ones until asked, and links to each", async () => {
    const api = base().on(
      "GET /workspaces",
      page([
        { ...workspaceRow(), can: adminCan },
        {
          ...workspaceRow({
            id: "77777777-7777-4777-8777-777777777777",
            name: "Old",
            state: "archived",
          }),
          can: adminCan,
        },
      ]),
    );
    render(<App client={api.client()} />);
    const list = await screen.findByRole("list", { name: "Workspaces" });
    expect(
      within(list).getByRole("link", { name: "Brand" }).getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}`);
    expect(within(list).queryByText("Old")).toBeNull();
    await userEvent.click(screen.getByLabelText("Show archived"));
    expect(
      within(screen.getByRole("list", { name: "Workspaces" })).getByText("Old"),
    ).toBeTruthy();
    expect(screen.getByText("Archived")).toBeTruthy();
  });

  it("lets owners and admins create a workspace and shows it", async () => {
    const created: unknown[] = [];
    const api = base()
      .on("GET /workspaces", () => page(created.map(() => workspaceRow())))
      .on("POST /workspaces", (call) => {
        created.push(call.body);
        return { status: 201, body: workspaceRow() };
      });
    render(<App client={api.client()} />);
    await screen.findByText(/Create your first workspace/);
    await userEvent.type(screen.getByLabelText("Workspace name"), "  Brand  ");
    await userEvent.click(
      screen.getByRole("button", { name: "Create workspace" }),
    );
    expect(await screen.findByRole("link", { name: "Brand" })).toBeTruthy();
    expect(created).toEqual([{ name: "Brand" }]);
  });

  it("hides workspace creation from an editor and says to ask an administrator", async () => {
    const api = base("editor").on("GET /workspaces", page([]));
    render(<App client={api.client()} />);
    expect(
      await screen.findByText(/Ask an administrator to add you/),
    ).toBeTruthy();
    expect(screen.queryByLabelText("Workspace name")).toBeNull();
  });

  it("shows an error with a retry when the list fails", async () => {
    let fail = true;
    const api = base().on("GET /workspaces", () =>
      fail
        ? apiError(
            503,
            "temporarily_unavailable",
            "The service is temporarily unavailable.",
          )
        : page([workspaceRow()]),
    );
    render(<App client={api.client()} />);
    expect((await screen.findByRole("alert")).textContent).toContain(
      "temporarily unavailable",
    );
    fail = false;
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("link", { name: "Brand" })).toBeTruthy();
  });

  it("shows recents from this browser and can clear them", async () => {
    window.localStorage.setItem(
      `figcraft.recents.${TENANT_ID}`,
      JSON.stringify([
        {
          documentId: DOCUMENT_ID,
          workspaceId: WORKSPACE_ID,
          name: "Homepage",
          at: 1_700_000_000_000,
        },
        {
          documentId: "bad",
          workspaceId: WORKSPACE_ID,
          name: "<img src=x onerror=alert(1)>",
          at: 1,
        },
      ]),
    );
    const api = base().on("GET /workspaces", page([workspaceRow()]));
    const { container } = render(<App client={api.client()} />);
    const recents = await screen.findByRole("list", { name: "Recent files" });
    expect(
      within(recents)
        .getByRole("link", { name: "Homepage" })
        .getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`);
    expect(within(recents).getAllByRole("listitem")).toHaveLength(1);
    expect(container.querySelector("img")).toBeNull();
    await userEvent.click(
      screen.getByRole("button", { name: "Clear recents" }),
    );
    expect(screen.queryByRole("list", { name: "Recent files" })).toBeNull();
    expect(readRecents(TENANT_ID)).toEqual([]);
  });
});

function workspaceApi(
  options: { can?: object; projectCan?: object; role?: string } = {},
) {
  const files: unknown[] = [
    fileRow(),
    fileRow({
      id: "88888888-8888-4888-8888-888888888888",
      name: "Pricing",
      state: "archived",
    }),
  ];
  const api = base(options.role)
    .on("GET /workspaces/" + WORKSPACE_ID, {
      body: { ...workspaceRow(), can: options.can ?? adminCan },
    })
    .on("GET /workspaces/" + WORKSPACE_ID + "/projects", page([projectRow()]))
    .on("GET /projects/" + PROJECT_ID, {
      body: { ...projectRow(), can: options.projectCan ?? projectCan },
    })
    .on("GET /projects/" + PROJECT_ID + "/documents", () => page(files));
  return { api, files };
}

describe("workspace page", () => {
  it("shows projects and the files of the first one, with actions for an administrator", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const { api } = workspaceApi();
    render(<App client={api.client()} />);
    expect(await screen.findByRole("heading", { name: /Brand/ })).toBeTruthy();
    const projectsNav = await screen.findByRole("list", { name: "Projects" });
    expect(
      within(projectsNav)
        .getByRole("link", { name: /Website/ })
        .getAttribute("aria-current"),
    ).toBe("page");
    const files = await screen.findByRole("list", { name: "Files" });
    expect(
      within(files)
        .getByRole("link", { name: "Homepage" })
        .getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`);
    expect(within(files).queryByText("Pricing")).toBeNull();
    await userEvent.click(screen.getByLabelText("More actions for Homepage"));
    expect(
      screen.getByRole("button", { name: "Rename Homepage" }),
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "Archive Homepage" }),
    ).toBeTruthy();
    await userEvent.click(screen.getByLabelText("Workspace actions"));
    expect(
      screen.getByRole("button", { name: "Rename workspace" }),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "New file" })).toBeTruthy();
  });

  it("offers only what the server says the caller can do", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const { api } = workspaceApi({
      can: viewerCan,
      projectCan: noProjectCan,
      role: "viewer",
    });
    render(<App client={api.client()} />);
    await screen.findByRole("list", { name: "Files" });
    await userEvent.click(screen.getByLabelText("More actions for Homepage"));
    for (const name of [
      "Rename workspace",
      "Archive workspace",
      "Rename project",
      "Archive project",
    ]) {
      expect(screen.queryByRole("button", { name }), name).toBeNull();
    }
    expect(screen.queryByLabelText("New file")).toBeNull();
    expect(screen.queryByLabelText("New project")).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Rename Homepage" }),
    ).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Archive Homepage" }),
    ).toBeNull();
  });

  it("creates a file, archives and restores one, and surfaces a refusal", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const { api, files } = workspaceApi();
    const createdId = "99999999-9999-4999-8999-999999999999";
    api.on("POST /projects/" + PROJECT_ID + "/documents", (call) => {
      const created = fileRow({
        id: createdId,
        name: (call.body as { name: string }).name,
      });
      files.push(created);
      return { status: 201, body: created };
    });
    const load = vi.spyOn(pageLoad, "assign").mockImplementation(() => {});
    api.on(
      "POST /documents/" + DOCUMENT_ID + "/archive",
      apiError(409, "resource_inactive", "The resource is not active."),
    );
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByRole("button", { name: "New file" }),
    );
    await userEvent.type(await screen.findByLabelText("New file"), "Logo");
    await userEvent.click(screen.getByRole("button", { name: "Add file" }));
    // The new file opens in the FigCraft editor: a full page load of its URL, remembered first.
    await waitFor(() =>
      expect(load).toHaveBeenCalledWith(
        `/workspace/${WORKSPACE_ID}/file/${createdId}`,
      ),
    );
    load.mockRestore();
    expect(window.location.pathname).toBe(`/workspace/${WORKSPACE_ID}`);
    expect(readRecents(TENANT_ID).map((r) => r.name)).toEqual(["Logo"]);
    // A refusal is shown, not swallowed.
    await userEvent.click(screen.getByLabelText("More actions for Homepage"));
    await userEvent.click(
      screen.getByRole("button", { name: "Archive Homepage" }),
    );
    expect((await screen.findByRole("alert")).textContent).toContain(
      "suspended or archived",
    );
    // Restoring an archived file.
    api.on(
      "POST /documents/88888888-8888-4888-8888-888888888888/restore",
      () => {
        files[1] = fileRow({
          id: "88888888-8888-4888-8888-888888888888",
          name: "Pricing",
        });
        return { body: {} };
      },
    );
    await userEvent.click(screen.getByLabelText("Show archived files"));
    await userEvent.click(screen.getByLabelText("More actions for Pricing"));
    await userEvent.click(
      screen.getByRole("button", { name: "Restore Pricing" }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: "Restore Pricing" }),
      ).toBeNull(),
    );
  });

  it("renames in a dialog that traps focus, closes on Escape and returns focus", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const { api, files } = workspaceApi();
    api.on("PATCH /documents/" + DOCUMENT_ID, (call) => {
      files[0] = fileRow({ name: (call.body as { name: string }).name });
      return { body: {} };
    });
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByLabelText("More actions for Homepage"),
    );
    const trigger = await screen.findByRole("button", {
      name: "Rename Homepage",
    });
    await userEvent.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Rename file" });
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    const input = within(dialog).getByLabelText(
      "File name",
    ) as HTMLInputElement;
    expect(document.activeElement).toBe(input);
    // Tab cycles inside the dialog.
    await userEvent.tab();
    await userEvent.tab();
    await userEvent.tab();
    expect(dialog.contains(document.activeElement)).toBe(true);
    // Escape cancels and focus returns to the opener.
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    // Saving renames.
    await userEvent.click(trigger);
    const field = screen.getByLabelText("File name");
    await userEvent.clear(field);
    await userEvent.type(field, "Landing");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("link", { name: "Landing" })).toBeTruthy();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("keeps the dialog open and shows the server's message when a rename is refused", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const { api } = workspaceApi();
    api.on(
      "PATCH /documents/" + DOCUMENT_ID,
      apiError(403, "forbidden", "This action is not allowed."),
    );
    render(<App client={api.client()} />);
    await userEvent.click(
      await screen.findByLabelText("More actions for Homepage"),
    );
    await userEvent.click(
      await screen.findByRole("button", { name: "Rename Homepage" }),
    );
    await userEvent.type(screen.getByLabelText("File name"), "x");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "This action is not allowed.",
    );
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("shows a clear screen for a suspended or archived workspace (409 resource_inactive)", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const api = base()
      .on(
        "GET /workspaces/" + WORKSPACE_ID,
        apiError(409, "resource_inactive", "The resource is not active."),
      )
      .on(
        "GET /workspaces/" + WORKSPACE_ID + "/projects",
        apiError(409, "resource_inactive", "x"),
      );
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", {
        name: "This workspace isn't available",
      }),
    ).toBeTruthy();
  });

  it("says not found for a workspace the caller cannot see", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const api = base()
      .on(
        "GET /workspaces/" + WORKSPACE_ID,
        apiError(404, "not_found", "The requested resource was not found."),
      )
      .on(
        "GET /workspaces/" + WORKSPACE_ID + "/projects",
        apiError(404, "not_found", "x"),
      );
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Workspace not found" }),
    ).toBeTruthy();
  });

  it("selects a project from the URL and keeps it in the address", async () => {
    const second = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    window.history.replaceState(
      null,
      "",
      `/workspace/${WORKSPACE_ID}?project=${second}`,
    );
    const { api } = workspaceApi();
    api.on(
      "GET /workspaces/" + WORKSPACE_ID + "/projects",
      page([projectRow(), projectRow({ id: second, name: "Mobile" })]),
    );
    api.on("GET /projects/" + second, {
      body: { ...projectRow({ id: second, name: "Mobile" }), can: projectCan },
    });
    api.on(
      "GET /projects/" + second + "/documents",
      page([
        fileRow({
          id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
          name: "Onboarding",
        }),
      ]),
    );
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("link", { name: "Onboarding" }),
    ).toBeTruthy();
    await userEvent.click(screen.getByRole("link", { name: /Website/ }));
    expect(window.location.search).toBe(`?project=${PROJECT_ID}`);
    expect(await screen.findByRole("link", { name: "Homepage" })).toBeTruthy();
  });
});

describe("opening files", () => {
  it("loads a file's editor page in full and remembers it", async () => {
    window.history.replaceState(null, "", `/workspace/${WORKSPACE_ID}`);
    const { api } = workspaceApi();
    const load = vi.spyOn(pageLoad, "assign").mockImplementation(() => {});
    render(<App client={api.client()} />);
    const link = await screen.findByRole("link", { name: "Homepage" });
    expect(link.getAttribute("href")).toBe(
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    );
    await userEvent.click(link);
    expect(load).toHaveBeenCalledWith(
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    );
    expect(window.location.pathname).toBe(`/workspace/${WORKSPACE_ID}`);
    expect(readRecents(TENANT_ID)[0]?.name).toBe("Homepage");
    load.mockRestore();
  });

  it("says where a file is when this app is asked to show one", async () => {
    window.history.replaceState(
      null,
      "",
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    );
    const { api } = workspaceApi();
    render(<App client={api.client()} />);
    expect(
      await screen.findByRole("heading", { name: "Files open in the editor" }),
    ).toBeTruthy();
    expect(
      screen.getByRole("link", { name: "Back to files" }).getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}`);
  });
});
