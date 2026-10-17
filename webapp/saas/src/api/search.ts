import type { ApiClient } from "./client";
import { documents, projects, workspaces } from "./workspaces";

export interface SearchHit {
  kind: "workspace" | "project" | "file";
  id: string;
  name: string;
  workspaceId: string;
  workspaceName: string;
  projectId?: string;
  projectName?: string;
  archived: boolean;
}

export interface SearchResult {
  hits: SearchHit[];
  /** Projects whose files were read. */
  scannedProjects: number;
  /** True when the caps below stopped the scan early, so results may be incomplete. */
  truncated: boolean;
}

/** The scan reads at most this many workspaces and projects, so one search is a bounded number of requests. */
export const MAX_WORKSPACES = 25;
export const MAX_PROJECTS = 60;
export const MAX_HITS = 100;

function matches(name: string, needle: string): boolean {
  return name.toLowerCase().includes(needle);
}

/**
 * Find workspaces, projects and files by name. There is no server-side search yet, so this reads
 * the lists the caller can already see (workspaces, then their projects, then their files) and
 * filters by a case-insensitive substring. It is bounded ({@link MAX_WORKSPACES},
 * {@link MAX_PROJECTS}, {@link MAX_HITS}) and says so when it stops early.
 */
export async function searchNames(
  client: ApiClient,
  term: string,
  shouldStop: () => boolean = () => false,
): Promise<SearchResult> {
  const needle = term.trim().toLowerCase();
  const result: SearchResult = {
    hits: [],
    scannedProjects: 0,
    truncated: false,
  };
  if (needle === "") return result;

  const spaces = await workspaces.list(client);
  if (spaces.truncated || spaces.items.length > MAX_WORKSPACES) {
    result.truncated = true;
  }
  let projectBudget = MAX_PROJECTS;
  for (const space of spaces.items.slice(0, MAX_WORKSPACES)) {
    if (shouldStop()) return result;
    const archivedSpace = space.state !== "active";
    if (matches(space.name, needle)) {
      result.hits.push({
        kind: "workspace",
        id: space.id,
        name: space.name,
        workspaceId: space.id,
        workspaceName: space.name,
        archived: archivedSpace,
      });
    }
    if (archivedSpace) continue;
    let list;
    try {
      list = await projects.list(client, space.id);
    } catch {
      // A workspace that can't be read right now is skipped; the scan goes on.
      result.truncated = true;
      continue;
    }
    for (const project of list.items) {
      if (shouldStop()) return result;
      if (matches(project.name, needle)) {
        result.hits.push({
          kind: "project",
          id: project.id,
          name: project.name,
          workspaceId: space.id,
          workspaceName: space.name,
          projectId: project.id,
          projectName: project.name,
          archived: project.state !== "active",
        });
      }
      if (project.state !== "active") continue;
      if (projectBudget <= 0) {
        result.truncated = true;
        continue;
      }
      projectBudget -= 1;
      result.scannedProjects += 1;
      let files;
      try {
        files = await documents.list(client, project.id);
      } catch {
        result.truncated = true;
        continue;
      }
      for (const file of files.items) {
        if (matches(file.name, needle)) {
          result.hits.push({
            kind: "file",
            id: file.id,
            name: file.name,
            workspaceId: space.id,
            workspaceName: space.name,
            projectId: project.id,
            projectName: project.name,
            archived: file.state !== "active",
          });
        }
      }
      if (result.hits.length >= MAX_HITS) {
        result.truncated = true;
        return result;
      }
    }
  }
  return result;
}
