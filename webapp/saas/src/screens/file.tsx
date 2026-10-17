import { useEffect, useState } from "react";

import type { ApiError } from "../api/client";
import { documents, safeEditorUrl, workspaces } from "../api/workspaces";
import {
  ErrorNotice,
  LoadingNotice,
  StateBadge,
  asApiError,
  useLoad,
} from "../components";
import { goTo } from "../platform";
import { addRecent } from "../recents";
import { Link } from "../router";
import { useSession } from "../session";
import { roleLabel } from "./entry";
import { ShareDialog } from "./share";
import { WorkspaceInactive } from "./workspace";

/** One file at its stable URL: who you are on it, and the way into the editor. */
export function FileScreen({
  workspaceId,
  documentId,
}: {
  workspaceId: string;
  documentId: string;
}) {
  const { client, state } = useSession();
  const tenantId = state.kind === "ready" ? state.tenant.id : null;
  const [file, reload] = useLoad(
    () => documents.get(client, documentId),
    [client, documentId],
  );
  const [workspace] = useLoad(
    () => workspaces.get(client, workspaceId),
    [client, workspaceId],
  );
  const [opening, setOpening] = useState(false);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [manualUrl, setManualUrl] = useState<string | null>(null);
  const [sharing, setSharing] = useState(false);

  const found = file.kind === "ready" ? file.value : null;
  const mismatch = found !== null && found.workspace_id !== workspaceId;

  // Remember the file for the home page once it has loaded and belongs at this URL.
  useEffect(() => {
    if (tenantId !== null && found !== null && !mismatch) {
      addRecent(tenantId, {
        documentId: found.id,
        workspaceId: found.workspace_id,
        name: found.name,
        at: Date.now(),
      });
    }
  }, [tenantId, found, mismatch]);

  if (file.kind === "loading") return <LoadingNotice label="Loading file…" />;
  if (file.kind === "error") {
    if (file.error.code === "resource_inactive") {
      return <WorkspaceInactive error={file.error} />;
    }
    if (file.error.status === 404) return <FileNotFound />;
    return <ErrorNotice error={file.error} retry={reload} />;
  }
  if (found === null || mismatch) return <FileNotFound />;

  const workspaceName =
    workspace.kind === "ready" ? workspace.value.name : null;
  const archived = found.state !== "active";
  const canOpen = !archived && found.role !== null && found.role !== undefined;

  const open = async () => {
    setOpening(true);
    setProblem(null);
    setManualUrl(null);
    try {
      const link = await documents.collabLink(client, found.id);
      const target = safeEditorUrl(link.editor_url);
      if (target === null) {
        // The server advertised an address that is not this site's editor; never navigate to it
        // blindly. Show it so the person can decide.
        setManualUrl(link.editor_url);
        setOpening(false);
        return;
      }
      goTo(target);
    } catch (error) {
      setProblem(asApiError(error));
      setOpening(false);
    }
  };

  return (
    <>
      <nav aria-label="Breadcrumb" className="muted small">
        <Link to="/">Files</Link> /{" "}
        <Link to={`/workspace/${workspaceId}?project=${found.project_id}`}>
          {workspaceName ?? "Workspace"}
        </Link>{" "}
        / <span aria-current="page">{found.name}</span>
      </nav>
      <h1>
        {found.name} <StateBadge state={found.state} />
      </h1>
      <p className="muted">
        Your role: <strong>{roleLabel(found.role)}</strong>
        {found.can_edit === true
          ? " — you can edit this file."
          : " — you can view this file."}
      </p>

      {archived && (
        <p className="notice" role="status">
          This file is archived. Restore it from its project to open it again.
        </p>
      )}

      <div className="actions">
        <button
          type="button"
          className="button primary"
          disabled={!canOpen || opening}
          onClick={() => void open()}
        >
          {opening ? "Opening…" : "Open in editor"}
        </button>
        {found.can?.share === true && !archived && (
          <button
            type="button"
            className="button"
            onClick={() => setSharing(true)}
          >
            Share
          </button>
        )}
        <Link
          className="button"
          to={`/workspace/${workspaceId}?project=${found.project_id}`}
        >
          Back to project
        </Link>
      </div>

      {problem !== null && <ErrorNotice error={problem} />}
      {sharing && (
        <ShareDialog
          documentId={found.id}
          documentName={found.name}
          canEdit={found.can?.edit === true}
          onClose={() => setSharing(false)}
        />
      )}
      {manualUrl !== null && (
        <div role="alert" className="notice error">
          <p>
            The server gave an editor address that isn't this site's, so it
            wasn't opened automatically. Check the deployment's public URL
            setting. The address was:
          </p>
          <p>
            <code>{manualUrl}</code>
          </p>
        </div>
      )}
    </>
  );
}

function FileNotFound() {
  return (
    <section className="panel narrow" aria-labelledby="file-missing-heading">
      <h1 id="file-missing-heading">File not found</h1>
      <p className="intro">
        It doesn't exist, was moved, or you don't have access to it.
      </p>
      <Link className="button primary" to="/">
        Back to your files
      </Link>
    </section>
  );
}
