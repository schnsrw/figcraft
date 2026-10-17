import { useId, useState } from "react";

import { workspaces } from "../api/workspaces";
import {
  CreateForm,
  EmptyState,
  ErrorNotice,
  LoadingNotice,
  PageHeader,
  StateBadge,
  initialOf,
  relativeTime,
  tint,
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

  const create = async (name: string) => {
    await workspaces.create(client, name);
    reload();
  };

  return (
    <>
      <PageHeader
        title="Your files"
        description="Pick up where you left off, or open a workspace to see its projects and files."
      />

      {tenant !== null && <RecentFiles tenantId={tenant.id} />}

      <section className="section" aria-labelledby="workspaces-heading">
        <div className="section-head">
          <h2 id="workspaces-heading">Workspaces</h2>
          {list.kind === "ready" &&
            list.value.items.some((item) => item.state === "archived") && (
              <label className="check" htmlFor={archivedId}>
                <input
                  id={archivedId}
                  type="checkbox"
                  checked={showArchived}
                  onChange={(event) => setShowArchived(event.target.checked)}
                />
                Show archived
              </label>
            )}
        </div>

        {list.kind === "loading" && (
          <LoadingNotice label="Loading workspaces…" />
        )}
        {list.kind === "error" && (
          <ErrorNotice error={list.error} retry={reload} />
        )}
        {list.kind === "ready" && (
          <WorkspaceCards
            items={list.value.items.filter(
              (item) => showArchived || item.state !== "archived",
            )}
            hiddenArchived={
              !showArchived &&
              list.value.items.some((item) => item.state === "archived")
            }
            canCreate={canCreate}
            truncated={list.value.truncated}
            onCreate={create}
          />
        )}
      </section>

      {canCreate && list.kind === "ready" && list.value.items.length > 0 && (
        <section
          aria-labelledby="new-workspace-heading"
          className="section card-form"
        >
          <h2 id="new-workspace-heading">New workspace</h2>
          <CreateForm
            label="Workspace name"
            buttonLabel="Create workspace"
            successLabel={(name) => `Workspace “${name}” created.`}
            onCreate={create}
          />
        </section>
      )}
    </>
  );
}

function WorkspaceCards({
  items,
  hiddenArchived,
  canCreate,
  truncated,
  onCreate,
}: {
  items: Awaited<ReturnType<typeof workspaces.list>>["items"];
  hiddenArchived: boolean;
  canCreate: boolean;
  truncated: boolean;
  onCreate: (name: string) => Promise<void>;
}) {
  if (items.length === 0) {
    if (hiddenArchived) {
      return (
        <EmptyState
          glyph="⌂"
          title="Everything here is archived"
          text="Turn on “Show archived” to see your archived workspaces and restore one."
        />
      );
    }
    return canCreate ? (
      <EmptyState
        glyph="+"
        title="Create your first workspace"
        text="A workspace holds the projects and files your team works on together. Name it after your team or your product."
      >
        <CreateForm
          label="Workspace name"
          buttonLabel="Create workspace"
          successLabel={(name) => `Workspace “${name}” created.`}
          onCreate={onCreate}
        />
      </EmptyState>
    ) : (
      <EmptyState
        glyph="…"
        title="You haven't been added to a workspace yet"
        text="Ask an administrator to add you. Once you are in, your team's projects and files show up here."
      />
    );
  }
  return (
    <>
      <ul className="cards" aria-label="Workspaces">
        {items.map((workspace) => (
          <li className="card" key={workspace.id}>
            <div
              className="tile small"
              style={{ background: tint(workspace.id) }}
              aria-hidden="true"
            >
              {initialOf(workspace.name)}
            </div>
            <div className="card-body">
              <Link className="card-title" to={`/workspace/${workspace.id}`}>
                {workspace.name}
              </Link>
              <div className="card-meta">
                <StateBadge state={workspace.state} />
                <span>Open to see projects and files</span>
              </div>
            </div>
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
    <section className="section" aria-labelledby="recent-heading">
      <div className="section-head">
        <h2 id="recent-heading">Recent files</h2>
        <button
          type="button"
          className="button small"
          onClick={() => {
            clearRecents(tenantId);
            setRecents([]);
          }}
        >
          Clear recents
        </button>
      </div>
      <ul className="recents" aria-label="Recent files">
        {recents.map((recent) => (
          <li className="card" key={recent.documentId}>
            <div
              className="tile small"
              style={{ background: tint(recent.documentId) }}
              aria-hidden="true"
            >
              {initialOf(recent.name)}
            </div>
            <div className="card-body">
              <Link
                className="card-title"
                to={`/workspace/${recent.workspaceId}/file/${recent.documentId}`}
              >
                {recent.name}
              </Link>
              <span className="card-meta">
                Opened {relativeTime(Math.floor(recent.at / 1000))}
              </span>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}
