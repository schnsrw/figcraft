import { useId, useState } from "react";

import { workspaces } from "../api/workspaces";
import {
  CreateForm,
  ErrorNotice,
  LoadingNotice,
  StateBadge,
  useLoad,
} from "../components";
import { clearRecents, readRecents } from "../recents";
import { Link } from "../router";
import { hasRole, useSession } from "../session";

/** Tenant roles that may create workspaces (the server decides; this only hides the form). */
export const WORKSPACE_CREATORS = ["tenant_owner", "tenant_admin"] as const;

export function HomeScreen() {
  const { client, state } = useSession();
  const tenant = state.kind === "ready" ? state.tenant : null;
  const canCreate = hasRole(tenant?.role ?? null, WORKSPACE_CREATORS);
  const [list, reload] = useLoad(() => workspaces.list(client), [client]);
  const [showArchived, setShowArchived] = useState(false);
  const archivedId = useId();

  return (
    <>
      <h1>Your files</h1>
      <p className="intro">
        Pick up where you left off, or open a workspace to see its projects and
        files.
      </p>

      {tenant !== null && <RecentFiles tenantId={tenant.id} />}

      <div className="toolbar">
        <h2>Workspaces</h2>
        <label className="check" htmlFor={archivedId}>
          <input
            id={archivedId}
            type="checkbox"
            checked={showArchived}
            onChange={(event) => setShowArchived(event.target.checked)}
          />
          Show archived
        </label>
      </div>

      {list.kind === "loading" && <LoadingNotice label="Loading workspaces…" />}
      {list.kind === "error" && (
        <ErrorNotice error={list.error} retry={reload} />
      )}
      {list.kind === "ready" && (
        <WorkspaceList
          items={list.value.items.filter(
            (item) => showArchived || item.state !== "archived",
          )}
          hiddenArchived={
            !showArchived &&
            list.value.items.some((item) => item.state === "archived")
          }
          canCreate={canCreate}
          truncated={list.value.truncated}
        />
      )}

      {canCreate && (
        <section aria-labelledby="new-workspace-heading" className="new-block">
          <h2 id="new-workspace-heading">New workspace</h2>
          <CreateForm
            label="Workspace name"
            buttonLabel="Create workspace"
            onCreate={async (name) => {
              await workspaces.create(client, name);
              reload();
            }}
          />
        </section>
      )}
    </>
  );
}

function WorkspaceList({
  items,
  hiddenArchived,
  canCreate,
  truncated,
}: {
  items: Awaited<ReturnType<typeof workspaces.list>>["items"];
  hiddenArchived: boolean;
  canCreate: boolean;
  truncated: boolean;
}) {
  if (items.length === 0) {
    return (
      <div className="notice">
        <p>
          {hiddenArchived
            ? "Everything here is archived. Turn on “Show archived” to see it."
            : canCreate
              ? "You don't have any workspaces yet. Create your first one below."
              : "You haven't been added to a workspace yet. Ask an administrator to add you."}
        </p>
      </div>
    );
  }
  return (
    <>
      <ul className="list" aria-label="Workspaces">
        {items.map((workspace) => (
          <li key={workspace.id}>
            <div className="row-main">
              <Link to={`/workspace/${workspace.id}`}>{workspace.name}</Link>
            </div>
            <StateBadge state={workspace.state} />
          </li>
        ))}
      </ul>
      {truncated && (
        <p className="muted small">
          Showing the first workspaces only. Use a more specific link to reach
          the rest.
        </p>
      )}
    </>
  );
}

function RecentFiles({ tenantId }: { tenantId: string }) {
  const [recents, setRecents] = useState(() => readRecents(tenantId));
  if (recents.length === 0) return null;
  return (
    <section aria-labelledby="recent-heading">
      <div className="toolbar">
        <h2 id="recent-heading">Recent files</h2>
        <button
          type="button"
          className="button"
          onClick={() => {
            clearRecents(tenantId);
            setRecents([]);
          }}
        >
          Clear recents
        </button>
      </div>
      <ul className="list" aria-label="Recent files">
        {recents.map((recent) => (
          <li key={recent.documentId}>
            <div className="row-main">
              <Link
                to={`/workspace/${recent.workspaceId}/file/${recent.documentId}`}
              >
                {recent.name}
              </Link>
              <span className="muted small">
                Opened {new Date(recent.at).toLocaleString()}
              </span>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}
