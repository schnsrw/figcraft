import { useState } from "react";

import type { ApiError } from "../api/client";
import type { CloudDocument } from "../api/types";
import {
  documents,
  projects,
  type ProjectWithCan,
  workspaces,
} from "../api/workspaces";
import {
  CreateForm,
  EmptyState,
  ErrorNotice,
  LoadingNotice,
  NameDialog,
  PageHeader,
  StateBadge,
  asApiError,
  initialOf,
  relativeTime,
  tint,
  useLoad,
} from "../components";
import { Link, useRouter } from "../router";
import { useSession } from "../session";

/** `resource_inactive` on a workspace route means it is suspended, archived or being deleted. */
export function WorkspaceInactive({ error }: { error: ApiError }) {
  return (
    <section className="panel narrow" aria-labelledby="inactive-heading">
      <h1 id="inactive-heading">This workspace isn't available</h1>
      <p className="intro">
        It is suspended or archived, so its files can't be opened or changed
        right now. An administrator can restore it.
      </p>
      {error.requestId !== null && (
        <p className="muted small">Reference {error.requestId}</p>
      )}
      <Link className="button primary" to="/">
        Back to your files
      </Link>
    </section>
  );
}

type Dialog =
  | { kind: "rename-workspace" }
  | { kind: "rename-project"; id: string; name: string }
  | { kind: "rename-file"; id: string; name: string };

export function WorkspaceScreen({ workspaceId }: { workspaceId: string }) {
  const { client } = useSession();
  const { path, navigate } = useRouter();
  const selected = new URLSearchParams(path.split("?")[1] ?? "").get("project");
  const [workspace, reloadWorkspace] = useLoad(
    () => workspaces.get(client, workspaceId),
    [client, workspaceId],
  );
  const [projectList, reloadProjects] = useLoad(
    () => projects.list(client, workspaceId),
    [client, workspaceId],
  );
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [showArchived, setShowArchived] = useState(false);
  const [filesVersion, setFilesVersion] = useState(0);

  if (workspace.kind === "loading") {
    return <LoadingNotice label="Loading workspace…" />;
  }
  if (workspace.kind === "error") {
    if (workspace.error.code === "resource_inactive") {
      return <WorkspaceInactive error={workspace.error} />;
    }
    if (workspace.error.status === 404) {
      return (
        <section className="panel narrow">
          <h1>Workspace not found</h1>
          <p className="intro">
            It doesn't exist, or you don't have access to it.
          </p>
          <Link className="button primary" to="/">
            Back to your files
          </Link>
        </section>
      );
    }
    return <ErrorNotice error={workspace.error} retry={reloadWorkspace} />;
  }

  const ws = workspace.value;
  const can = ws.can;
  const allProjects =
    projectList.kind === "ready" ? projectList.value.items : [];
  const visibleProjects = allProjects.filter(
    (project) => showArchived || project.state !== "archived",
  );
  const current =
    allProjects.find((project) => project.id === selected) ??
    visibleProjects[0] ??
    null;

  const act = async (action: () => Promise<unknown>, after: () => void) => {
    setProblem(null);
    try {
      await action();
      after();
    } catch (error) {
      setProblem(asApiError(error));
    }
  };

  return (
    <>
      <PageHeader
        breadcrumb={
          <nav aria-label="Breadcrumb" className="breadcrumb">
            <Link to="/">Files</Link>
            <span aria-hidden="true">/</span>
            <span aria-current="page">{ws.name}</span>
          </nav>
        }
        title={
          <>
            {ws.name} <StateBadge state={ws.state} />
          </>
        }
        description={`${visibleProjects.length} project${visibleProjects.length === 1 ? "" : "s"}`}
        actions={
          <>
            {can?.rename === true && (
              <button
                type="button"
                className="button"
                onClick={() => setDialog({ kind: "rename-workspace" })}
              >
                Rename workspace
              </button>
            )}
            {can?.archive === true && ws.state === "active" && (
              <button
                type="button"
                className="button"
                onClick={() =>
                  void act(
                    () => workspaces.archive(client, ws.id),
                    reloadWorkspace,
                  )
                }
              >
                Archive workspace
              </button>
            )}
            {can?.restore === true && ws.state === "archived" && (
              <button
                type="button"
                className="button primary"
                onClick={() =>
                  void act(
                    () => workspaces.restore(client, ws.id),
                    reloadWorkspace,
                  )
                }
              >
                Restore workspace
              </button>
            )}
          </>
        }
      />

      {problem !== null && <ErrorNotice error={problem} />}

      <div className="layout-two">
        <aside className="side-panel" aria-labelledby="projects-heading">
          <h2 id="projects-heading">Projects</h2>
          {projectList.kind === "loading" && (
            <LoadingNotice label="Loading projects…" />
          )}
          {projectList.kind === "error" && (
            <ErrorNotice error={projectList.error} retry={reloadProjects} />
          )}
          {projectList.kind === "ready" && (
            <>
              {visibleProjects.length === 0 ? (
                <p className="muted small">No projects yet.</p>
              ) : (
                <ul className="side-list" aria-label="Projects">
                  {visibleProjects.map((project) => (
                    <li key={project.id}>
                      <Link
                        to={`/workspace/${ws.id}?project=${project.id}`}
                        aria-current={
                          current?.id === project.id ? "page" : undefined
                        }
                      >
                        <span>{project.name}</span>
                        <StateBadge state={project.state} />
                      </Link>
                    </li>
                  ))}
                </ul>
              )}
              {allProjects.some((project) => project.state === "archived") && (
                <label className="check">
                  <input
                    type="checkbox"
                    checked={showArchived}
                    onChange={(event) => setShowArchived(event.target.checked)}
                  />
                  Show archived
                </label>
              )}
              {can?.create_project === true && ws.state === "active" && (
                <div className="side-form">
                  <CreateForm
                    label="New project"
                    buttonLabel="Add project"
                    onCreate={async (name) => {
                      const created = await projects.create(
                        client,
                        ws.id,
                        name,
                      );
                      reloadProjects();
                      navigate(`/workspace/${ws.id}?project=${created.id}`);
                    }}
                  />
                </div>
              )}
            </>
          )}
        </aside>

        <section aria-labelledby="files-heading">
          {current === null ? (
            projectList.kind === "ready" ? (
              <EmptyState
                glyph="▤"
                title="No project selected"
                text={
                  can?.create_project === true
                    ? "Projects group related files, like one per product or campaign. Add the first one in the panel on the left."
                    : "Choose a project on the left to see its files. Ask an administrator if you expected to see some."
                }
              />
            ) : null
          ) : (
            <ProjectFiles
              key={current.id}
              project={current}
              version={filesVersion}
              workspaceId={ws.id}
              workspaceCan={can}
              onRenameProject={() =>
                setDialog({
                  kind: "rename-project",
                  id: current.id,
                  name: current.name,
                })
              }
              onRenameFile={(file) =>
                setDialog({ kind: "rename-file", id: file.id, name: file.name })
              }
              onProjectChanged={reloadProjects}
              onProblem={setProblem}
            />
          )}
        </section>
      </div>

      {dialog?.kind === "rename-workspace" && (
        <NameDialog
          title="Rename workspace"
          label="Workspace name"
          initial={ws.name}
          submitLabel="Save"
          onSubmit={async (name) => {
            await workspaces.rename(client, ws.id, name);
            reloadWorkspace();
          }}
          onClose={() => setDialog(null)}
        />
      )}
      {dialog?.kind === "rename-project" && (
        <NameDialog
          title="Rename project"
          label="Project name"
          initial={dialog.name}
          submitLabel="Save"
          onSubmit={async (name) => {
            await projects.rename(client, dialog.id, name);
            reloadProjects();
          }}
          onClose={() => setDialog(null)}
        />
      )}
      {dialog?.kind === "rename-file" && (
        <NameDialog
          title="Rename file"
          label="File name"
          initial={dialog.name}
          submitLabel="Save"
          onSubmit={async (name) => {
            await documents.rename(client, dialog.id, name);
            setFilesVersion((version) => version + 1);
          }}
          onClose={() => setDialog(null)}
        />
      )}
    </>
  );
}

function ProjectFiles({
  project,
  version,
  workspaceId,
  workspaceCan,
  onRenameProject,
  onRenameFile,
  onProjectChanged,
  onProblem,
}: {
  project: ProjectWithCan;
  /** Bumped by the parent when a dialog it owns changed a file. */
  version: number;
  workspaceId: string;
  workspaceCan: { archive: boolean; restore: boolean } | undefined;
  onRenameProject: () => void;
  onRenameFile: (file: CloudDocument) => void;
  onProjectChanged: () => void;
  onProblem: (error: ApiError | null) => void;
}) {
  const { client } = useSession();
  const [detail, reloadDetail] = useLoad(
    () => projects.get(client, project.id),
    [client, project.id],
  );
  const [files, reloadFiles] = useLoad(
    () => documents.list(client, project.id),
    [client, project.id, version],
  );
  const [showArchived, setShowArchived] = useState(false);
  const can = detail.kind === "ready" ? detail.value.can : undefined;

  const act = async (action: () => Promise<unknown>, after: () => void) => {
    onProblem(null);
    try {
      await action();
      after();
    } catch (error) {
      onProblem(asApiError(error));
    }
  };

  const items =
    files.kind === "ready"
      ? files.value.items.filter(
          (file) => showArchived || file.state !== "archived",
        )
      : [];

  const canAdd = can?.create_document === true && project.state === "active";
  const createFile = async (name: string) => {
    await documents.create(client, project.id, name);
    reloadFiles();
  };

  return (
    <>
      <div className="section-head">
        <h2 id="files-heading">
          {project.name} <StateBadge state={project.state} />
        </h2>
        <div className="actions">
          {can?.rename === true && (
            <button
              type="button"
              className="button small"
              onClick={onRenameProject}
            >
              Rename project
            </button>
          )}
          {can?.archive === true && project.state === "active" && (
            <button
              type="button"
              className="button small"
              onClick={() =>
                void act(
                  () => projects.archive(client, project.id),
                  () => {
                    onProjectChanged();
                    reloadDetail();
                  },
                )
              }
            >
              Archive project
            </button>
          )}
          {can?.restore === true && project.state === "archived" && (
            <button
              type="button"
              className="button small"
              onClick={() =>
                void act(
                  () => projects.restore(client, project.id),
                  () => {
                    onProjectChanged();
                    reloadDetail();
                  },
                )
              }
            >
              Restore project
            </button>
          )}
        </div>
      </div>

      {canAdd && items.length > 0 && (
        <div className="card-form">
          <CreateForm
            label="New file"
            buttonLabel="Add file"
            successLabel={(name) => `File “${name}” added.`}
            onCreate={createFile}
          />
        </div>
      )}

      {files.kind === "loading" && <LoadingNotice label="Loading files…" />}
      {files.kind === "error" && (
        <ErrorNotice error={files.error} retry={reloadFiles} />
      )}
      {files.kind === "ready" && (
        <>
          {items.length === 0 ? (
            canAdd ? (
              <EmptyState
                glyph="+"
                title="Add the first file"
                text="Files are where design happens. Name it after the screen or the idea; you can rename it any time."
              >
                <CreateForm
                  label="New file"
                  buttonLabel="Add file"
                  successLabel={(name) => `File “${name}” added.`}
                  onCreate={createFile}
                />
              </EmptyState>
            ) : (
              <EmptyState
                glyph="▤"
                title="No files in this project"
                text="When someone adds a file here it will show up for you."
              />
            )
          ) : (
            <ul className="cards" aria-label="Files">
              {items.map((file) => (
                <li className="card" key={file.id}>
                  <div
                    className="tile"
                    style={{ background: tint(file.id) }}
                    aria-hidden="true"
                  >
                    {initialOf(file.name)}
                  </div>
                  <div className="card-body">
                    <Link
                      className="card-title"
                      to={`/workspace/${workspaceId}/file/${file.id}`}
                    >
                      {file.name}
                    </Link>
                    <div className="card-meta">
                      <StateBadge state={file.state} />
                      {file.updated_at !== undefined && (
                        <span>Edited {relativeTime(file.updated_at)}</span>
                      )}
                    </div>
                  </div>
                  <div className="card-actions">
                    {can?.create_document === true &&
                      file.state === "active" && (
                        <button
                          type="button"
                          className="button"
                          aria-label={`Rename ${file.name}`}
                          onClick={() => onRenameFile(file)}
                        >
                          Rename
                        </button>
                      )}
                    {workspaceCan?.archive === true &&
                      file.state === "active" && (
                        <button
                          type="button"
                          className="button"
                          aria-label={`Archive ${file.name}`}
                          onClick={() =>
                            void act(
                              () => documents.archive(client, file.id),
                              reloadFiles,
                            )
                          }
                        >
                          Archive
                        </button>
                      )}
                    {workspaceCan?.restore === true &&
                      file.state === "archived" && (
                        <button
                          type="button"
                          className="button"
                          aria-label={`Restore ${file.name}`}
                          onClick={() =>
                            void act(
                              () => documents.restore(client, file.id),
                              reloadFiles,
                            )
                          }
                        >
                          Restore
                        </button>
                      )}
                  </div>
                </li>
              ))}
            </ul>
          )}
          {files.value.truncated && (
            <p className="muted small">Showing the first files only.</p>
          )}
          {files.value.items.some((file) => file.state === "archived") && (
            <label className="check">
              <input
                type="checkbox"
                checked={showArchived}
                onChange={(event) => setShowArchived(event.target.checked)}
              />
              Show archived files
            </label>
          )}
        </>
      )}
    </>
  );
}
