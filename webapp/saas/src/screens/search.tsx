import { type FormEvent, useEffect, useId, useRef, useState } from "react";

import { type SearchResult, searchNames } from "../api/search";
import {
  ErrorNotice,
  LoadingNotice,
  StateBadge,
  asApiError,
} from "../components";
import type { ApiError } from "../api/client";
import { Link, useRouter } from "../router";
import { useSession } from "../session";

const MAX_TERM = 100;

function hitLink(hit: SearchResult["hits"][number]): string {
  switch (hit.kind) {
    case "workspace":
      return `/workspace/${hit.workspaceId}`;
    case "project":
      return `/workspace/${hit.workspaceId}?project=${hit.id}`;
    case "file":
      return `/workspace/${hit.workspaceId}/file/${hit.id}`;
  }
}

const KIND_LABEL = { workspace: "Workspace", project: "Project", file: "File" };

/** Find workspaces, projects and files by name. The term lives in the URL (`/search?q=…`). */
export function SearchScreen() {
  const { client } = useSession();
  const { path, navigate } = useRouter();
  const term = (
    new URLSearchParams(path.split("?")[1] ?? "").get("q") ?? ""
  ).slice(0, MAX_TERM);
  const inputId = useId();
  const [draft, setDraft] = useState(term);
  const [state, setState] = useState<
    | { kind: "idle" }
    | { kind: "loading" }
    | { kind: "error"; error: ApiError }
    | { kind: "done"; result: SearchResult }
  >({ kind: "idle" });
  const stop = useRef(false);

  useEffect(() => {
    setDraft(term);
    if (term.trim() === "") {
      setState({ kind: "idle" });
      return;
    }
    stop.current = false;
    setState({ kind: "loading" });
    let cancelled = false;
    searchNames(client, term, () => stop.current)
      .then((result) => {
        if (!cancelled) setState({ kind: "done", result });
      })
      .catch((error: unknown) => {
        if (!cancelled) setState({ kind: "error", error: asApiError(error) });
      });
    return () => {
      cancelled = true;
      stop.current = true;
    };
  }, [client, term]);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const next = draft.trim();
    navigate(next === "" ? "/search" : `/search?q=${encodeURIComponent(next)}`);
  };

  return (
    <>
      <h1>Search</h1>
      <p className="intro">
        Find a workspace, project or file by name among the ones you can open.
      </p>
      <form className="inline-form" role="search" onSubmit={submit}>
        <div className="field grow">
          <label htmlFor={inputId}>Search by name</label>
          <input
            id={inputId}
            data-testid="search-input"
            type="search"
            value={draft}
            maxLength={MAX_TERM}
            onChange={(event) => setDraft(event.target.value)}
          />
        </div>
        <button
          type="submit"
          className="button primary"
          data-testid="search-submit"
        >
          Search
        </button>
      </form>
      <div aria-live="polite">
        {state.kind === "loading" && <LoadingNotice label="Searching…" />}
        {state.kind === "error" && <ErrorNotice error={state.error} />}
        {state.kind === "done" && (
          <>
            <p className="muted" data-testid="search-summary">
              {state.result.hits.length === 0
                ? `No results for “${term}”.`
                : `${state.result.hits.length} result${state.result.hits.length === 1 ? "" : "s"} for “${term}”.`}
            </p>
            {state.result.truncated && (
              <p className="muted small" data-testid="search-truncated">
                Some workspaces or projects weren't searched (too many, or one
                couldn't be read), so there may be more results.
              </p>
            )}
            {state.result.hits.length > 0 && (
              <ul className="list" aria-label="Search results">
                {state.result.hits.map((hit) => (
                  <li key={`${hit.kind}-${hit.id}`}>
                    <div className="row-main">
                      <Link to={hitLink(hit)}>{hit.name}</Link>
                      <span className="muted small">
                        {KIND_LABEL[hit.kind]}
                        {hit.kind !== "workspace" && ` · ${hit.workspaceName}`}
                        {hit.kind === "file" &&
                          hit.projectName !== undefined &&
                          ` / ${hit.projectName}`}
                      </span>
                    </div>
                    {hit.archived && <StateBadge state="archived" />}
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </div>
    </>
  );
}
