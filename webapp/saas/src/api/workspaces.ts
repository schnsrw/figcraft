import type { ApiClient } from "./client";
import type {
  CloudDocument,
  CollabLink,
  Page,
  Project,
  Workspace,
} from "./types";

/** What the caller may do, as the server reports it (a convenience; the server still decides). */
export interface WorkspaceCan {
  create_project: boolean;
  rename: boolean;
  archive: boolean;
  restore: boolean;
  manage_members: boolean;
}

export interface ProjectCan {
  create_document: boolean;
  rename: boolean;
  archive: boolean;
  restore: boolean;
}

export interface DocumentCan {
  edit: boolean;
  rename: boolean;
  archive: boolean;
  restore: boolean;
  share: boolean;
}

export type WorkspaceWithCan = Workspace & { can?: WorkspaceCan };
export type ProjectWithCan = Project & { can?: ProjectCan };
export type DocumentWithCan = CloudDocument & { can?: DocumentCan };

/** Pages followed for one list, so a runaway cursor can never loop. */
export const MAX_PAGES = 10;
export const PAGE_SIZE = 50;

/** Every item of a paged list (up to {@link MAX_PAGES} pages) and whether more exist. */
export async function loadAll<T>(
  client: ApiClient,
  path: string,
): Promise<{ items: T[]; truncated: boolean }> {
  const items: T[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < MAX_PAGES; page += 1) {
    const separator = path.includes("?") ? "&" : "?";
    const query: string =
      cursor === null
        ? `limit=${PAGE_SIZE}`
        : `limit=${PAGE_SIZE}&cursor=${encodeURIComponent(cursor)}`;
    const result: Page<T> = await client.get<Page<T>>(
      `${path}${separator}${query}`,
    );
    items.push(...result.items);
    if (result.next_cursor === null) return { items, truncated: false };
    cursor = result.next_cursor;
  }
  return { items, truncated: true };
}

export const workspaces = {
  list: (client: ApiClient) => loadAll<WorkspaceWithCan>(client, "/workspaces"),
  get: (client: ApiClient, id: string) =>
    client.get<WorkspaceWithCan>(`/workspaces/${id}`),
  create: (client: ApiClient, name: string) =>
    client.post<Workspace>("/workspaces", { name }),
  rename: (client: ApiClient, id: string, name: string) =>
    client.patch<Workspace>(`/workspaces/${id}`, { name }),
  archive: (client: ApiClient, id: string) =>
    client.post<Workspace>(`/workspaces/${id}/archive`),
  restore: (client: ApiClient, id: string) =>
    client.post<Workspace>(`/workspaces/${id}/restore`),
};

export const projects = {
  list: (client: ApiClient, workspaceId: string) =>
    loadAll<ProjectWithCan>(client, `/workspaces/${workspaceId}/projects`),
  get: (client: ApiClient, id: string) =>
    client.get<ProjectWithCan>(`/projects/${id}`),
  create: (client: ApiClient, workspaceId: string, name: string) =>
    client.post<Project>(`/workspaces/${workspaceId}/projects`, { name }),
  rename: (client: ApiClient, id: string, name: string) =>
    client.patch<Project>(`/projects/${id}`, { name }),
  archive: (client: ApiClient, id: string) =>
    client.post<Project>(`/projects/${id}/archive`),
  restore: (client: ApiClient, id: string) =>
    client.post<Project>(`/projects/${id}/restore`),
};

export const documents = {
  list: (client: ApiClient, projectId: string) =>
    loadAll<CloudDocument>(client, `/projects/${projectId}/documents`),
  get: (client: ApiClient, id: string) =>
    client.get<DocumentWithCan>(`/documents/${id}`),
  create: (client: ApiClient, projectId: string, name: string) =>
    client.post<CloudDocument>(`/projects/${projectId}/documents`, { name }),
  rename: (client: ApiClient, id: string, name: string) =>
    client.patch<CloudDocument>(`/documents/${id}`, { name }),
  archive: (client: ApiClient, id: string) =>
    client.post<CloudDocument>(`/documents/${id}/archive`),
  restore: (client: ApiClient, id: string) =>
    client.post<CloudDocument>(`/documents/${id}/restore`),
  collabLink: (client: ApiClient, id: string) =>
    client.get<CollabLink>(`/documents/${id}/collab-link`),
};

/**
 * Where the browser may go to open the editor: the server's `editor_url`, accepted only when it is
 * the editor page (`/edit/`) with a `#join=` fragment on this same origin. Anything else is
 * `null` and the caller shows the link instead of navigating. With `fileName` the address becomes
 * `#name=<file name>&join=<invite>`.
 */
export function safeEditorUrl(
  editorUrl: string,
  origin: string = window.location.origin,
  fileName?: string,
): string | null {
  let url: URL;
  try {
    url = new URL(editorUrl, origin);
  } catch {
    return null;
  }
  if (
    url.origin !== origin ||
    url.pathname !== "/edit/" ||
    url.search !== "" ||
    !url.hash.startsWith("#join=")
  ) {
    return null;
  }
  // The file's name goes before the invite link, so a new file opens under the name the workspace
  // gave it instead of the editor's placeholder. The editor reads at most 80 characters.
  const name = fileName?.trim().slice(0, 80);
  if (name) {
    const link = url.hash.slice("#join=".length);
    url.hash = `#name=${encodeURIComponent(name)}&join=${link}`;
  }
  return url.href;
}
