import { type FormEvent, useId, useState } from "react";

import type { ApiError } from "../api/client";
import {
  DOCUMENT_ROLES,
  type Grant,
  type ShareLink,
  people,
  shareEditorUrl,
  sharing,
} from "../api/people";
import {
  ErrorNotice,
  LoadingNotice,
  type Loadable,
  Modal,
  asApiError,
  useLoad,
} from "../components";
import { useReady } from "../session";
import { roleLabel } from "./entry";

const EXPIRIES: { label: string; seconds: number }[] = [
  { label: "1 day", seconds: 24 * 60 * 60 },
  { label: "7 days", seconds: 7 * 24 * 60 * 60 },
  { label: "30 days", seconds: 30 * 24 * 60 * 60 },
];

/**
 * Who can open one file: people, teams and links, with names rather than identifiers. The caller
 * needs the `share` permission to get here; the server still checks every change.
 */
export function ShareDialog({
  documentId,
  documentName,
  canEdit,
  onClose,
}: {
  documentId: string;
  documentName: string;
  canEdit: boolean;
  onClose: () => void;
}) {
  const { client, tenantId } = useReady();
  const [members, reloadMembers] = useLoad(
    () => sharing.members(client, documentId),
    [client, documentId],
  );
  const [teamGrants, reloadTeamGrants] = useLoad(
    () => sharing.teamGrants(client, documentId),
    [client, documentId],
  );
  const [links, reloadLinks] = useLoad(
    () => sharing.links(client, documentId),
    [client, documentId],
  );
  const [everyone] = useLoad(
    () => people.members(client, tenantId),
    [client, tenantId],
  );
  const [teams] = useLoad(
    () => people.teams(client, tenantId),
    [client, tenantId],
  );
  const [problem, setProblem] = useState<ApiError | null>(null);

  const act = async (action: () => Promise<unknown>, after: () => void) => {
    setProblem(null);
    try {
      await action();
    } catch (error) {
      setProblem(asApiError(error));
    }
    after();
  };

  const personName = (id: string | undefined) =>
    everyone.kind === "ready"
      ? (everyone.value.items.find((member) => member.principal_id === id)
          ?.display_name ?? "Someone no longer in the organisation")
      : "…";
  const teamName = (id: string | undefined) =>
    teams.kind === "ready"
      ? (teams.value.items.find((team) => team.id === id)?.name ??
        "A removed team")
      : "…";

  return (
    <Modal title={`Share “${documentName}”`} onClose={onClose}>
      {problem !== null && <ErrorNotice error={problem} />}

      <GrantSection
        title="People"
        addLabel="Add a person"
        loading={members}
        reload={reloadMembers}
        nameOf={(grant) => personName(grant.principal_id)}
        idOf={(grant) => grant.principal_id ?? ""}
        candidates={
          everyone.kind === "ready" && members.kind === "ready"
            ? everyone.value.items
                .filter(
                  (member) =>
                    member.state === "active" &&
                    !members.value.items.some(
                      (grant) => grant.principal_id === member.principal_id,
                    ),
                )
                .map((member) => ({
                  id: member.principal_id,
                  name: member.display_name,
                }))
            : []
        }
        onSet={(id, role) =>
          act(
            () => sharing.setMember(client, documentId, id, role),
            reloadMembers,
          )
        }
        onRemove={(id) =>
          act(() => sharing.removeMember(client, documentId, id), reloadMembers)
        }
      />

      <GrantSection
        title="Teams"
        addLabel="Add a team"
        loading={teamGrants}
        reload={reloadTeamGrants}
        nameOf={(grant) => teamName(grant.team_id)}
        idOf={(grant) => grant.team_id ?? ""}
        candidates={
          teams.kind === "ready" && teamGrants.kind === "ready"
            ? teams.value.items
                .filter(
                  (team) =>
                    team.state === "active" &&
                    !teamGrants.value.items.some(
                      (grant) => grant.team_id === team.id,
                    ),
                )
                .map((team) => ({ id: team.id, name: team.name }))
            : []
        }
        onSet={(id, role) =>
          act(
            () => sharing.setTeamGrant(client, documentId, id, role),
            reloadTeamGrants,
          )
        }
        onRemove={(id) =>
          act(
            () => sharing.removeTeamGrant(client, documentId, id),
            reloadTeamGrants,
          )
        }
      />

      <LinksSection
        documentId={documentId}
        canEdit={canEdit}
        links={links}
        reload={reloadLinks}
        onProblem={setProblem}
      />

      <div className="actions">
        <button type="button" className="button primary" onClick={onClose}>
          Done
        </button>
      </div>
    </Modal>
  );
}

function GrantSection({
  title,
  addLabel,
  loading,
  reload,
  nameOf,
  idOf,
  candidates,
  onSet,
  onRemove,
}: {
  title: string;
  addLabel: string;
  loading: Loadable<{ items: Grant[]; truncated: boolean }>;
  reload: () => void;
  nameOf: (grant: Grant) => string;
  idOf: (grant: Grant) => string;
  candidates: { id: string; name: string }[];
  onSet: (id: string, role: string) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
}) {
  const [who, setWho] = useState("");
  const [role, setRole] = useState("viewer");
  const whoId = useId();
  const roleId = useId();
  const headingId = useId();

  return (
    <section aria-labelledby={headingId} className="share-section">
      <h3 id={headingId}>{title}</h3>
      {loading.kind === "loading" && (
        <LoadingNotice label={`Loading ${title.toLowerCase()}…`} />
      )}
      {loading.kind === "error" && (
        <ErrorNotice error={loading.error} retry={reload} />
      )}
      {loading.kind === "ready" && (
        <>
          {loading.value.items.length === 0 ? (
            <p className="muted small">No one added yet.</p>
          ) : (
            <ul className="list" aria-label={title}>
              {loading.value.items.map((grant) => (
                <li key={idOf(grant)}>
                  <span className="row-title">{nameOf(grant)}</span>
                  <select
                    aria-label={`Access for ${nameOf(grant)}`}
                    value={grant.role}
                    onChange={(event) =>
                      void onSet(idOf(grant), event.target.value)
                    }
                  >
                    {DOCUMENT_ROLES.map((option) => (
                      <option key={option} value={option}>
                        {roleLabel(option)}
                      </option>
                    ))}
                    {!(DOCUMENT_ROLES as readonly string[]).includes(
                      grant.role,
                    ) && (
                      <option value={grant.role}>
                        {roleLabel(grant.role)}
                      </option>
                    )}
                  </select>
                  <button
                    type="button"
                    className="button"
                    aria-label={`Remove ${nameOf(grant)}`}
                    onClick={() => void onRemove(idOf(grant))}
                  >
                    Remove
                  </button>
                </li>
              ))}
            </ul>
          )}
          <form
            className="inline-form"
            onSubmit={(event: FormEvent) => {
              event.preventDefault();
              if (who === "") return;
              void onSet(who, role).then(() => setWho(""));
            }}
          >
            <div className="field grow">
              <label htmlFor={whoId}>{addLabel}</label>
              <select
                id={whoId}
                value={who}
                onChange={(event) => setWho(event.target.value)}
              >
                <option value="">Choose…</option>
                {candidates.map((candidate) => (
                  <option key={candidate.id} value={candidate.id}>
                    {candidate.name}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label htmlFor={roleId}>{`${title} access`}</label>
              <select
                id={roleId}
                value={role}
                onChange={(event) => setRole(event.target.value)}
              >
                {DOCUMENT_ROLES.map((option) => (
                  <option key={option} value={option}>
                    {roleLabel(option)}
                  </option>
                ))}
              </select>
            </div>
            <button type="submit" className="button" disabled={who === ""}>
              {`Add to ${title.toLowerCase()}`}
            </button>
          </form>
        </>
      )}
    </section>
  );
}

function expiryText(link: ShareLink): string {
  if (link.expires_at === null) return "No expiry";
  return `Expires ${new Date(link.expires_at * 1000).toLocaleString()}`;
}

function LinksSection({
  documentId,
  canEdit,
  links,
  reload,
  onProblem,
}: {
  documentId: string;
  canEdit: boolean;
  links: Loadable<{ items: ShareLink[] }>;
  reload: () => void;
  onProblem: (error: ApiError | null) => void;
}) {
  const { client } = useReady();
  const [role, setRole] = useState<"viewer" | "editor">("viewer");
  const [label, setLabel] = useState("");
  const [expiry, setExpiry] = useState(EXPIRIES[1]?.seconds ?? 604800);
  const [created, setCreated] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const headingId = useId();
  const roleId = useId();
  const labelId = useId();
  const expiryId = useId();

  const create = async (event: FormEvent) => {
    event.preventDefault();
    onProblem(null);
    setCopied(false);
    try {
      const result = await sharing.createLink(
        client,
        documentId,
        role,
        label.trim() === "" ? null : label.trim(),
        expiry,
      );
      setCreated(
        shareEditorUrl(window.location.origin, documentId, result.secret),
      );
      setLabel("");
      reload();
    } catch (error) {
      onProblem(asApiError(error));
    }
  };

  const live =
    links.kind === "ready"
      ? links.value.items.filter((link) => link.revoked_at === null)
      : [];

  return (
    <section aria-labelledby={headingId} className="share-section">
      <h3 id={headingId}>Links</h3>
      <p className="muted small">
        Anyone with a link can open this file without signing in, until it
        expires or you revoke it.
      </p>
      {links.kind === "loading" && <LoadingNotice label="Loading links…" />}
      {links.kind === "error" && (
        <ErrorNotice error={links.error} retry={reload} />
      )}
      {links.kind === "ready" &&
        (live.length === 0 ? (
          <p className="muted small">No active links.</p>
        ) : (
          <ul className="list" aria-label="Links">
            {live.map((link) => {
              const name = link.label ?? "Unlabelled link";
              return (
                <li key={link.id}>
                  <div className="row-main">
                    <span className="row-title">{name}</span>
                    <span className="muted small">
                      {link.role === "editor" ? "Can edit" : "Can view"} ·{" "}
                      {expiryText(link)}
                    </span>
                  </div>
                  <button
                    type="button"
                    className="button danger"
                    aria-label={`Revoke ${name}`}
                    onClick={() => {
                      onProblem(null);
                      sharing
                        .revokeLink(client, documentId, link.id)
                        .then(reload)
                        .catch((error: unknown) =>
                          onProblem(asApiError(error)),
                        );
                    }}
                  >
                    Revoke
                  </button>
                </li>
              );
            })}
          </ul>
        ))}
      {created !== null && (
        <div role="status" className="notice ok">
          <p>Link created. Copy it now; it won't be shown again.</p>
          <p>
            <code>{created}</code>
          </p>
          <button
            type="button"
            className="button"
            onClick={() => {
              void navigator.clipboard
                ?.writeText(created)
                .then(() => setCopied(true));
            }}
          >
            {copied ? "Copied" : "Copy link"}
          </button>
        </div>
      )}
      <form className="inline-form" onSubmit={(event) => void create(event)}>
        <div className="field">
          <label htmlFor={roleId}>Link access</label>
          <select
            id={roleId}
            value={role}
            onChange={(event) =>
              setRole(event.target.value === "editor" ? "editor" : "viewer")
            }
          >
            <option value="viewer">Can view</option>
            <option value="editor" disabled={!canEdit}>
              Can edit
            </option>
          </select>
        </div>
        <div className="field grow">
          <label htmlFor={labelId}>Label (optional)</label>
          <input
            id={labelId}
            value={label}
            maxLength={100}
            onChange={(event) => setLabel(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor={expiryId}>Expires after</label>
          <select
            id={expiryId}
            value={expiry}
            onChange={(event) => setExpiry(Number(event.target.value))}
          >
            {EXPIRIES.map((option) => (
              <option key={option.seconds} value={option.seconds}>
                {option.label}
              </option>
            ))}
          </select>
        </div>
        <button type="submit" className="button">
          Create link
        </button>
      </form>
    </section>
  );
}
