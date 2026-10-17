import {
  type FormEvent,
  type KeyboardEvent,
  useId,
  useRef,
  useState,
} from "react";

import type { ApiError } from "../api/client";
import {
  type Invitation,
  type Member,
  TEAM_ROLES,
  TENANT_ROLES,
  type Team,
  type TeamMember,
  invitationLink,
  people,
} from "../api/people";
import {
  ErrorNotice,
  LoadingNotice,
  Modal,
  StateBadge,
  asApiError,
  useLoad,
} from "../components";
import { hasRole, useReady } from "../session";
import { roleLabel, slugify } from "./entry";

const ADMIN_ROLES = ["tenant_owner", "tenant_admin"] as const;

type TabId = "members" | "teams" | "invitations";

const TABS: { id: TabId; label: string }[] = [
  { id: "members", label: "Members" },
  { id: "teams", label: "Teams" },
  { id: "invitations", label: "Invitations" },
];

export function PeopleScreen() {
  const [tab, setTab] = useState<TabId>("members");
  const refs = useRef<Record<string, HTMLButtonElement | null>>({});
  const baseId = useId();

  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    const index = TABS.findIndex((item) => item.id === tab);
    let next: number;
    if (event.key === "ArrowRight") next = (index + 1) % TABS.length;
    else if (event.key === "ArrowLeft") {
      next = (index - 1 + TABS.length) % TABS.length;
    } else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = TABS.length - 1;
    else return;
    event.preventDefault();
    const target = TABS[next];
    if (target === undefined) return;
    setTab(target.id);
    refs.current[target.id]?.focus();
  };

  return (
    <>
      <h1>People</h1>
      <p className="intro">
        Who is in this organisation, how they are grouped into teams, and who
        has been invited.
      </p>
      <div role="tablist" aria-label="People" className="tabs">
        {TABS.map((item) => (
          <button
            key={item.id}
            ref={(element) => {
              refs.current[item.id] = element;
            }}
            type="button"
            role="tab"
            id={`${baseId}-${item.id}-tab`}
            aria-selected={tab === item.id}
            aria-controls={`${baseId}-${item.id}-panel`}
            tabIndex={tab === item.id ? 0 : -1}
            className="tab"
            onClick={() => setTab(item.id)}
            onKeyDown={onKeyDown}
          >
            {item.label}
          </button>
        ))}
      </div>
      <div
        role="tabpanel"
        id={`${baseId}-${tab}-panel`}
        aria-labelledby={`${baseId}-${tab}-tab`}
        tabIndex={0}
        className="tabpanel"
      >
        {tab === "members" && <MembersTab />}
        {tab === "teams" && <TeamsTab />}
        {tab === "invitations" && <InvitationsTab />}
      </div>
    </>
  );
}

// ---------------------------------------------------------------------------------------------
// Members

function MembersTab() {
  const { client, tenantId, role, principalId } = useReady();
  const [list, reload] = useLoad(
    () => people.members(client, tenantId),
    [client, tenantId],
  );
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [filter, setFilter] = useState("");
  const [confirm, setConfirm] = useState<Member | null>(null);
  const filterId = useId();
  const isOwner = role === "tenant_owner";
  const canManage = hasRole(role, ADMIN_ROLES);

  const act = async (action: () => Promise<unknown>) => {
    setProblem(null);
    try {
      await action();
      reload();
    } catch (error) {
      setProblem(asApiError(error));
      reload();
    }
  };

  if (list.kind === "loading")
    return <LoadingNotice label="Loading members…" />;
  if (list.kind === "error") {
    return <ErrorNotice error={list.error} retry={reload} />;
  }
  const needle = filter.trim().toLowerCase();
  const shown = list.value.items.filter(
    (member) =>
      member.state !== "removed" &&
      (needle === "" || member.display_name.toLowerCase().includes(needle)),
  );

  return (
    <>
      <div className="field">
        <label htmlFor={filterId}>Find a member</label>
        <input
          id={filterId}
          type="search"
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
        />
      </div>
      {problem !== null && <ErrorNotice error={problem} />}
      <table>
        <caption className="sr-only">Members of this organisation</caption>
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col">Role</th>
            <th scope="col">Status</th>
            {canManage && (
              <th scope="col">
                <span className="sr-only">Actions</span>
              </th>
            )}
          </tr>
        </thead>
        <tbody>
          {shown.map((member) => {
            const self = member.principal_id === principalId;
            const ownerRow = member.role === "tenant_owner";
            const editable = canManage && (isOwner || !ownerRow);
            return (
              <tr key={member.principal_id}>
                <th scope="row">
                  {member.display_name}
                  {self && <span className="muted"> (you)</span>}
                </th>
                <td>
                  {editable ? (
                    <select
                      aria-label={`Role of ${member.display_name}`}
                      value={member.role}
                      onChange={(event) =>
                        void act(() =>
                          people.setMember(
                            client,
                            tenantId,
                            member.principal_id,
                            {
                              role: event.target.value,
                            },
                          ),
                        )
                      }
                    >
                      {TENANT_ROLES.map((option) => (
                        <option
                          key={option}
                          value={option}
                          disabled={option === "tenant_owner" && !isOwner}
                        >
                          {roleLabel(option)}
                        </option>
                      ))}
                    </select>
                  ) : (
                    roleLabel(member.role)
                  )}
                </td>
                <td>
                  {member.state === "suspended" ? (
                    <StateBadge state="suspended" />
                  ) : (
                    "Active"
                  )}
                </td>
                {canManage && (
                  <td>
                    <div className="actions">
                      {editable && !self && (
                        <>
                          <button
                            type="button"
                            className="button"
                            onClick={() =>
                              void act(() =>
                                people.setMember(
                                  client,
                                  tenantId,
                                  member.principal_id,
                                  {
                                    state:
                                      member.state === "suspended"
                                        ? "active"
                                        : "suspended",
                                  },
                                ),
                              )
                            }
                          >
                            {member.state === "suspended"
                              ? `Reactivate ${member.display_name}`
                              : `Suspend ${member.display_name}`}
                          </button>
                          <button
                            type="button"
                            className="button danger"
                            onClick={() => setConfirm(member)}
                          >
                            {`Remove ${member.display_name}`}
                          </button>
                        </>
                      )}
                    </div>
                  </td>
                )}
              </tr>
            );
          })}
        </tbody>
      </table>
      {shown.length === 0 && <p className="muted">No one matches.</p>}
      {list.value.truncated && (
        <p className="muted small">
          Showing the first members only. Use the search box.
        </p>
      )}
      {!canManage && (
        <p className="muted small">
          Only owners and admins can change roles or remove people.
        </p>
      )}
      {confirm !== null && (
        <Modal title="Remove member" onClose={() => setConfirm(null)}>
          <p>
            Remove <strong>{confirm.display_name}</strong> from this
            organisation? They lose access to everything here straight away.
          </p>
          <div className="actions">
            <button
              type="button"
              className="button danger"
              onClick={() => {
                const target = confirm;
                setConfirm(null);
                void act(() =>
                  people.removeMember(client, tenantId, target.principal_id),
                );
              }}
            >
              Remove
            </button>
            <button
              type="button"
              className="button"
              onClick={() => setConfirm(null)}
            >
              Cancel
            </button>
          </div>
        </Modal>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------------------------
// Teams

function TeamsTab() {
  const { client, tenantId, role } = useReady();
  const [teams, reloadTeams] = useLoad(
    () => people.teams(client, tenantId),
    [client, tenantId],
  );
  const [selected, setSelected] = useState<string | null>(null);
  const nameId = useId();
  const [name, setName] = useState("");
  const [problem, setProblem] = useState<ApiError | null>(null);
  const canCreate = hasRole(role, ADMIN_ROLES);

  const create = async (event: FormEvent) => {
    event.preventDefault();
    setProblem(null);
    try {
      const created = await people.createTeam(
        client,
        tenantId,
        name.trim(),
        slugify(name),
      );
      setName("");
      reloadTeams();
      setSelected(created.id);
    } catch (error) {
      setProblem(asApiError(error));
    }
  };

  if (teams.kind === "loading") return <LoadingNotice label="Loading teams…" />;
  if (teams.kind === "error") {
    return <ErrorNotice error={teams.error} retry={reloadTeams} />;
  }
  const current =
    teams.value.items.find((team) => team.id === selected) ?? null;

  return (
    <div className="layout-two">
      <section aria-labelledby="teams-heading">
        <h2 id="teams-heading">Teams</h2>
        {teams.value.items.length === 0 ? (
          <p className="muted">No teams yet.</p>
        ) : (
          <ul className="side-list" aria-label="Teams">
            {teams.value.items.map((team) => (
              <li key={team.id}>
                <button
                  type="button"
                  className="linklike"
                  aria-current={selected === team.id ? "true" : undefined}
                  onClick={() => setSelected(team.id)}
                >
                  {team.name} <StateBadge state={team.state} />
                </button>
              </li>
            ))}
          </ul>
        )}
        {canCreate && (
          <form
            className="inline-form"
            onSubmit={(event) => void create(event)}
          >
            <div className="field grow">
              <label htmlFor={nameId}>New team</label>
              <input
                id={nameId}
                value={name}
                maxLength={120}
                onChange={(event) => setName(event.target.value)}
              />
            </div>
            <button
              type="submit"
              className="button primary"
              disabled={name.trim() === ""}
            >
              Add team
            </button>
          </form>
        )}
        {problem !== null && <ErrorNotice error={problem} />}
      </section>
      <section aria-live="polite">
        {current === null ? (
          <p className="muted">Choose a team to see who is in it.</p>
        ) : (
          <TeamPanel key={current.id} team={current} onChanged={reloadTeams} />
        )}
      </section>
    </div>
  );
}

function TeamPanel({ team, onChanged }: { team: Team; onChanged: () => void }) {
  const { client, tenantId, role, principalId } = useReady();
  const [members, reload] = useLoad(
    () => people.teamMembers(client, tenantId, team.id),
    [client, tenantId, team.id],
  );
  const [everyone] = useLoad(
    () => people.members(client, tenantId),
    [client, tenantId],
  );
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [addWho, setAddWho] = useState("");
  const [addRole, setAddRole] = useState("viewer");
  const whoId = useId();
  const roleId = useId();

  const act = async (
    action: () => Promise<unknown>,
    after: () => void = reload,
  ) => {
    setProblem(null);
    try {
      await action();
    } catch (error) {
      setProblem(asApiError(error));
    }
    after();
  };

  if (members.kind === "loading")
    return <LoadingNotice label="Loading team…" />;
  if (members.kind === "error") {
    return <ErrorNotice error={members.error} retry={reload} />;
  }
  const mine = members.value.items.find(
    (member) => member.principal_id === principalId,
  );
  const canManage =
    team.state === "active" &&
    (hasRole(role, ADMIN_ROLES) || mine?.role === "team_manager");
  const inTeam = new Set(
    members.value.items.map((member) => member.principal_id),
  );
  const candidates =
    everyone.kind === "ready"
      ? everyone.value.items.filter(
          (member) =>
            member.state === "active" && !inTeam.has(member.principal_id),
        )
      : [];

  return (
    <>
      <div className="toolbar">
        <h2>
          {team.name} <StateBadge state={team.state} />
        </h2>
        {canManage && (
          <button
            type="button"
            className="button"
            onClick={() =>
              void act(
                () => people.archiveTeam(client, tenantId, team.id),
                onChanged,
              )
            }
          >
            Archive team
          </button>
        )}
      </div>
      {problem !== null && <ErrorNotice error={problem} />}
      <table>
        <caption className="sr-only">Members of {team.name}</caption>
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col">Team role</th>
            {canManage && (
              <th scope="col">
                <span className="sr-only">Actions</span>
              </th>
            )}
          </tr>
        </thead>
        <tbody>
          {members.value.items.map((member: TeamMember) => (
            <tr key={member.principal_id}>
              <th scope="row">{member.display_name}</th>
              <td>
                {canManage ? (
                  <select
                    aria-label={`Team role of ${member.display_name}`}
                    value={member.role}
                    onChange={(event) =>
                      void act(() =>
                        people.setTeamMemberRole(
                          client,
                          tenantId,
                          team.id,
                          member.principal_id,
                          event.target.value,
                        ),
                      )
                    }
                  >
                    {TEAM_ROLES.map((option) => (
                      <option key={option} value={option}>
                        {roleLabel(option)}
                      </option>
                    ))}
                  </select>
                ) : (
                  roleLabel(member.role)
                )}
              </td>
              {canManage && (
                <td>
                  <button
                    type="button"
                    className="button"
                    onClick={() =>
                      void act(() =>
                        people.removeTeamMember(
                          client,
                          tenantId,
                          team.id,
                          member.principal_id,
                        ),
                      )
                    }
                  >
                    {`Remove ${member.display_name} from team`}
                  </button>
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
      {members.value.items.length === 0 && (
        <p className="muted">This team has no members yet.</p>
      )}
      {canManage && (
        <form
          className="inline-form"
          onSubmit={(event) => {
            event.preventDefault();
            if (addWho === "") return;
            void act(async () => {
              await people.addTeamMember(
                client,
                tenantId,
                team.id,
                addWho,
                addRole,
              );
              setAddWho("");
            });
          }}
        >
          <div className="field grow">
            <label htmlFor={whoId}>Add to team</label>
            <select
              id={whoId}
              value={addWho}
              onChange={(event) => setAddWho(event.target.value)}
            >
              <option value="">Choose a person…</option>
              {candidates.map((member) => (
                <option key={member.principal_id} value={member.principal_id}>
                  {member.display_name}
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor={roleId}>Team role</label>
            <select
              id={roleId}
              value={addRole}
              onChange={(event) => setAddRole(event.target.value)}
            >
              {TEAM_ROLES.map((option) => (
                <option key={option} value={option}>
                  {roleLabel(option)}
                </option>
              ))}
            </select>
          </div>
          <button
            type="submit"
            className="button primary"
            disabled={addWho === ""}
          >
            Add
          </button>
        </form>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------------------------
// Invitations

function InvitationsTab() {
  const { client, tenantId, role } = useReady();
  const canManage = hasRole(role, ADMIN_ROLES);
  const [teams] = useLoad(
    () => people.teams(client, tenantId),
    [client, tenantId],
  );
  const [pending, reload] = useLoad(
    () =>
      canManage
        ? people.invitations(client, tenantId)
        : Promise.resolve({ items: [] as Invitation[], truncated: false }),
    [client, tenantId, canManage],
  );
  const [email, setEmail] = useState("");
  const [team, setTeam] = useState("");
  const [teamRole, setTeamRole] = useState("viewer");
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [link, setLink] = useState<{ email: string; url: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const emailId = useId();
  const teamId = useId();
  const roleId = useId();

  if (!canManage) {
    return (
      <p className="notice">
        Only owners and admins can invite people. Ask one of them to send an
        invitation.
      </p>
    );
  }
  if (teams.kind === "loading" || pending.kind === "loading") {
    return <LoadingNotice label="Loading invitations…" />;
  }
  if (teams.kind === "error") return <ErrorNotice error={teams.error} />;
  if (pending.kind === "error")
    return <ErrorNotice error={pending.error} retry={reload} />;
  const activeTeams = teams.value.items.filter(
    (item) => item.state === "active",
  );
  const teamName = (id: string) =>
    teams.value.items.find((item) => item.id === id)?.name ?? "A removed team";

  const send = async (event: FormEvent) => {
    event.preventDefault();
    setProblem(null);
    setCopied(false);
    try {
      const created = await people.invite(
        client,
        tenantId,
        team,
        email.trim(),
        teamRole,
      );
      setLink({
        email: email.trim(),
        url: invitationLink(window.location.origin, created.token),
      });
      setEmail("");
      reload();
    } catch (error) {
      setProblem(asApiError(error));
    }
  };

  return (
    <>
      <p className="muted">
        An invitation adds the person to a team (and to the organisation if they
        are new). Send them the link yourself; it is shown once.
      </p>
      {activeTeams.length === 0 ? (
        <p className="notice">
          Create a team first, then invite people into it.
        </p>
      ) : (
        <form className="inline-form" onSubmit={(event) => void send(event)}>
          <div className="field grow">
            <label htmlFor={emailId}>Email address</label>
            <input
              id={emailId}
              type="email"
              value={email}
              required
              maxLength={254}
              onChange={(event) => setEmail(event.target.value)}
            />
          </div>
          <div className="field">
            <label htmlFor={teamId}>Team</label>
            <select
              id={teamId}
              value={team}
              required
              onChange={(event) => setTeam(event.target.value)}
            >
              <option value="">Choose a team…</option>
              {activeTeams.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor={roleId}>Team role</label>
            <select
              id={roleId}
              value={teamRole}
              onChange={(event) => setTeamRole(event.target.value)}
            >
              {TEAM_ROLES.map((option) => (
                <option key={option} value={option}>
                  {roleLabel(option)}
                </option>
              ))}
            </select>
          </div>
          <button
            type="submit"
            className="button primary"
            disabled={email.trim() === "" || team === ""}
          >
            Create invitation
          </button>
        </form>
      )}
      {problem !== null && <ErrorNotice error={problem} />}
      {link !== null && (
        <div role="status" className="notice ok">
          <p>
            Invitation for <strong>{link.email}</strong> created. Copy the link
            now; it won't be shown again.
          </p>
          <p>
            <code>{link.url}</code>
          </p>
          <button
            type="button"
            className="button"
            onClick={() => {
              void navigator.clipboard
                ?.writeText(link.url)
                .then(() => setCopied(true));
            }}
          >
            {copied ? "Copied" : "Copy link"}
          </button>
        </div>
      )}
      <h2>Pending invitations</h2>
      {pending.value.items.length === 0 ? (
        <p className="muted">Nothing pending.</p>
      ) : (
        <ul className="list" aria-label="Pending invitations">
          {pending.value.items.map((invitation) => (
            <li key={invitation.id}>
              <div className="row-main">
                <span className="row-title">{invitation.email}</span>
                <span className="muted small">
                  {teamName(invitation.team_id)} · {roleLabel(invitation.role)}
                </span>
              </div>
              <button
                type="button"
                className="button"
                aria-label={`Revoke invitation for ${invitation.email}`}
                onClick={() => {
                  setProblem(null);
                  people
                    .revokeInvitation(
                      client,
                      tenantId,
                      invitation.team_id,
                      invitation.id,
                    )
                    .then(reload)
                    .catch((error: unknown) => setProblem(asApiError(error)));
                }}
              >
                Revoke
              </button>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
