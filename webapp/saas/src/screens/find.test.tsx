import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { App } from "../App";
import { MAX_PROJECTS, searchNames } from "../api/search";
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

const WS2 = "99999999-9999-4999-8999-999999999999";
const PROJECT2 = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

function page<T>(items: T[]) {
  return { body: { items, next_cursor: null } };
}

const ws = (id: string, name: string, state = "active") => ({
  id,
  name,
  state,
  owning_team_id: null,
});
const project = (
  id: string,
  workspace_id: string,
  name: string,
  state = "active",
) => ({ id, workspace_id, name, state });
const file = (
  id: string,
  project_id: string,
  workspace_id: string,
  name: string,
  state = "active",
) => ({ id, project_id, workspace_id, name, state });

function searchApi(role = "tenant_owner") {
  return new FakeApi()
    .on("GET /auth/session", {
      body: session({ tenant: { id: TENANT_ID, role, permissions: [] } }),
    })
    .on("GET /tenants", { body: tenantList })
    .on(
      "GET /workspaces",
      page([ws(WORKSPACE_ID, "Brand"), ws(WS2, "Archived brand", "archived")]),
    )
    .on(
      "GET /workspaces/" + WORKSPACE_ID + "/projects",
      page([
        project(PROJECT_ID, WORKSPACE_ID, "Website"),
        project(PROJECT2, WORKSPACE_ID, "Brand guide", "archived"),
      ]),
    )
    .on(
      "GET /projects/" + PROJECT_ID + "/documents",
      page([
        file(DOCUMENT_ID, PROJECT_ID, WORKSPACE_ID, "Homepage"),
        file(
          "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
          PROJECT_ID,
          WORKSPACE_ID,
          "Brand colours",
          "archived",
        ),
      ]),
    );
}

describe("search", () => {
  it("finds workspaces, projects and files by name and links to each", async () => {
    window.history.replaceState(null, "", "/search?q=BRAND");
    render(<App client={searchApi().client()} />);
    const list = await screen.findByRole("list", { name: "Search results" });
    expect((await screen.findByTestId("search-summary")).textContent).toBe(
      "4 results for “BRAND”.",
    );
    const links = within(list).getAllByRole("link");
    expect(links.map((l) => l.textContent)).toEqual(
      ["Brand", "Brand guide", "Brand colours", "Archived brand"].sort(
        (a, b) =>
          links.map((l) => l.textContent).indexOf(a) -
          links.map((l) => l.textContent).indexOf(b),
      ),
    );
    expect(
      within(list).getByRole("link", { name: "Brand" }).getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}`);
    expect(
      within(list)
        .getByRole("link", { name: "Brand guide" })
        .getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}?project=${PROJECT2}`);
    expect(
      within(list)
        .getByRole("link", { name: "Brand colours" })
        .getAttribute("href"),
    ).toBe(
      `/workspace/${WORKSPACE_ID}/file/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb`,
    );
    expect(within(list).getAllByText("Archived").length).toBe(3);
  });

  it("puts the term in the address, reruns on submit and says when nothing matches", async () => {
    window.history.replaceState(null, "", "/search");
    const api = searchApi();
    render(<App client={api.client()} />);
    await userEvent.type(await screen.findByTestId("search-input"), "home");
    await userEvent.click(screen.getByTestId("search-submit"));
    expect(window.location.search).toBe("?q=home");
    const list = await screen.findByRole("list", { name: "Search results" });
    expect(
      within(list).getByRole("link", { name: "Homepage" }).getAttribute("href"),
    ).toBe(`/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`);
    const field = screen.getByTestId("search-input");
    await userEvent.clear(field);
    await userEvent.type(field, "zzz");
    await userEvent.click(screen.getByTestId("search-submit"));
    await waitFor(() =>
      expect(screen.getByTestId("search-summary").textContent).toBe(
        "No results for “zzz”.",
      ),
    );
  });

  it("does nothing for an empty term and shows an error when the lists fail", async () => {
    window.history.replaceState(null, "", "/search");
    const api = searchApi();
    render(<App client={api.client()} />);
    await screen.findByTestId("search-input");
    expect(api.count("GET /workspaces")).toBe(0);
    api.on(
      "GET /workspaces",
      apiError(
        503,
        "temporarily_unavailable",
        "The service is temporarily unavailable.",
      ),
    );
    await userEvent.type(screen.getByTestId("search-input"), "x");
    await userEvent.click(screen.getByTestId("search-submit"));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "temporarily unavailable",
    );
  });

  it("keeps going past a workspace that cannot be read and says results may be incomplete", async () => {
    const api = searchApi().on(
      "GET /workspaces/" + WORKSPACE_ID + "/projects",
      apiError(403, "forbidden", "x"),
    );
    const result = await searchNames(api.client(), "brand");
    expect(result.hits.map((h) => h.name)).toEqual(["Brand", "Archived brand"]);
    expect(result.truncated).toBe(true);
  });

  it("reads a bounded number of projects", async () => {
    const many = Array.from({ length: MAX_PROJECTS + 5 }, (_, index) =>
      project(
        `00000000-0000-4000-8000-${String(index).padStart(12, "0")}`,
        WORKSPACE_ID,
        `P${index}`,
      ),
    );
    const api = new FakeApi()
      .on("GET /workspaces", page([ws(WORKSPACE_ID, "Big")]))
      .on("GET /workspaces/" + WORKSPACE_ID + "/projects", page(many));
    for (const item of many)
      api.on("GET /projects/" + item.id + "/documents", page([]));
    const result = await searchNames(api.client(), "nothing");
    expect(result.scannedProjects).toBe(MAX_PROJECTS);
    expect(result.truncated).toBe(true);
    expect(api.calls.filter((c) => c.path.includes("/documents")).length).toBe(
      MAX_PROJECTS,
    );
  });
});

describe("settings", () => {
  const initial = {
    password_enabled: true,
    oidc_enabled: false,
    open_registration: false,
    require_totp: false,
  };
  function settingsApi(role = "tenant_owner") {
    return searchApi(role).on(
      "GET /tenants/" + TENANT_ID + "/account-settings",
      { body: initial },
    );
  }

  it("is in the navigation for owners and admins only", async () => {
    window.history.replaceState(null, "", "/search");
    const owner = render(<App client={searchApi("tenant_admin").client()} />);
    expect(await screen.findByRole("link", { name: "Settings" })).toBeTruthy();
    owner.unmount();
    render(<App client={searchApi("editor").client()} />);
    await screen.findByRole("link", { name: "Search" });
    expect(screen.queryByRole("link", { name: "Settings" })).toBeNull();
  });

  it("shows and saves the sign-in methods", async () => {
    window.history.replaceState(null, "", "/settings");
    const api = settingsApi().on(
      "PUT /tenants/" + TENANT_ID + "/account-settings",
      (call) => ({ body: call.body }),
    );
    render(<App client={api.client()} />);
    const save = (await screen.findByTestId(
      "settings-save",
    )) as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    await userEvent.click(screen.getByTestId("settings-open-registration"));
    await userEvent.click(screen.getByTestId("settings-oidc"));
    expect(save.disabled).toBe(false);
    await userEvent.click(save);
    expect((await screen.findByText("Saved.")).getAttribute("role")).toBe(
      "status",
    );
    expect(api.calls.find((c) => c.method === "PUT")?.body).toEqual({
      password_enabled: true,
      oidc_enabled: true,
      open_registration: true,
      require_totp: false,
    });
  });

  it("will not save with no method on, and open registration follows passwords", async () => {
    window.history.replaceState(null, "", "/settings");
    const api = settingsApi();
    api.on("GET /tenants/" + TENANT_ID + "/account-settings", {
      body: { ...initial, open_registration: true },
    });
    render(<App client={api.client()} />);
    await userEvent.click(await screen.findByTestId("settings-password"));
    expect(screen.getByTestId("settings-warning").textContent).toContain(
      "At least one sign-in method must stay on",
    );
    expect(
      (screen.getByTestId("settings-save") as HTMLButtonElement).disabled,
    ).toBe(true);
    const open = screen.getByTestId(
      "settings-open-registration",
    ) as HTMLInputElement;
    expect(open.disabled).toBe(true);
    expect(open.checked).toBe(false);
  });

  it("is read-only for members who are not admins, and shows a refusal", async () => {
    window.history.replaceState(null, "", "/settings");
    render(<App client={settingsApi("editor").client()} />);
    const password = (await screen.findByTestId(
      "settings-password",
    )) as HTMLInputElement;
    expect(password.matches(":disabled")).toBe(true);
    expect(screen.queryByTestId("settings-save")).toBeNull();
    expect(
      screen.getByText(/Only owners and admins can change these/),
    ).toBeTruthy();
  });

  it("surfaces the server's refusal when saving", async () => {
    window.history.replaceState(null, "", "/settings");
    const api = settingsApi().on(
      "PUT /tenants/" + TENANT_ID + "/account-settings",
      apiError(400, "invalid_request", "The request is invalid."),
    );
    render(<App client={api.client()} />);
    await userEvent.click(await screen.findByTestId("settings-oidc"));
    await userEvent.click(screen.getByTestId("settings-save"));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "The request is invalid.",
    );
  });
});
