/**
 * Recently opened files, remembered in this browser only (no server state, nothing about other
 * people). Entries are validated when read, so a tampered or stale value can never break the page
 * or inject markup; names are rendered as text.
 */
import { isUuid } from "./router";

export interface Recent {
  documentId: string;
  workspaceId: string;
  name: string;
  /** Unix milliseconds. */
  at: number;
}

export const MAX_RECENTS = 12;
const MAX_NAME = 255;

function key(tenant: string): string {
  return `figcraft.recents.${tenant}`;
}

export function readRecents(tenant: string): Recent[] {
  let raw: string | null;
  try {
    raw = window.localStorage.getItem(key(tenant));
  } catch {
    return [];
  }
  if (raw === null || raw.length > 64 * 1024) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const out: Recent[] = [];
  for (const item of parsed) {
    if (typeof item !== "object" || item === null) continue;
    const entry = item as Record<string, unknown>;
    if (
      typeof entry.documentId === "string" &&
      typeof entry.workspaceId === "string" &&
      isUuid(entry.documentId) &&
      isUuid(entry.workspaceId) &&
      typeof entry.name === "string" &&
      entry.name !== "" &&
      typeof entry.at === "number" &&
      Number.isFinite(entry.at)
    ) {
      out.push({
        documentId: entry.documentId,
        workspaceId: entry.workspaceId,
        name: entry.name.slice(0, MAX_NAME),
        at: entry.at,
      });
    }
    if (out.length >= MAX_RECENTS) break;
  }
  return out;
}

function write(tenant: string, recents: Recent[]): void {
  try {
    window.localStorage.setItem(key(tenant), JSON.stringify(recents));
  } catch {
    // Storage may be blocked or full; recents are a convenience.
  }
}

/** Put `entry` first (replacing an older entry for the same file) and keep the newest few. */
export function addRecent(tenant: string, entry: Recent): void {
  const rest = readRecents(tenant).filter(
    (recent) => recent.documentId !== entry.documentId,
  );
  write(tenant, [entry, ...rest].slice(0, MAX_RECENTS));
}

export function removeRecent(tenant: string, documentId: string): void {
  write(
    tenant,
    readRecents(tenant).filter((recent) => recent.documentId !== documentId),
  );
}

export function clearRecents(tenant: string): void {
  try {
    window.localStorage.removeItem(key(tenant));
  } catch {
    // Nothing to clear.
  }
}
