import {
  type FormEvent,
  type KeyboardEvent,
  useId,
  useRef,
  useState,
} from "react";

import type { ApiError } from "../api/client";
import {
  type AccountUser,
  type CreatedUser,
  type Invitation,
  type Member,
  TEAM_ROLES,
  TENANT_ROLES,
  type Team,
  type TeamMember,
  invitationLink,
  people,
  resetLink,
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

/** A link to hand to someone, shown once with a copy button. */
export function OneTimeLink({
  heading,
  url,
  note,
  testId,
}: {
  heading: string;
  url: string;
  note: string;
  testId: string;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <div role="status" className="notice ok" data-testid={testId}>
      <p>
        <strong>{heading}</strong> {note}
      </p>
      <p>
        <code data-testid={`${testId}-url`}>{url}</code>
      </p>
      <button
        type="button"
        className="button"
        onClick={() => {
          void navigator.clipboard?.writeText(url).then(() => setCopied(true));
        }}
      >
        {copied ? "Copied" : "Copy link"}
      </button>
    </div>
  );
}

function hoursText(seconds: number): string {
  if (seconds < 3600) return `${Math.max(1, Math.round(seconds / 60))} minutes`;
  const hours = Math.round(seconds / 3600);
  return hours === 1 ? "1 hour" : `${hours} hours`;
}

function MembersTab() {
  const { client, tenantId, role, principalId } = useReady();
  const isOwner = role === "tenant_owner";
  const canManage = hasRole(role, ADMIN_ROLES);
  const [list, reload] = useLoad(
    () => people.members(client, tenantId),
    [client, tenantId],
  );
  // Password accounts (login, locked, disabled). Anyone who can see members may read this list;
  // a deployment without password accounts just returns nothing useful, so a failure is quiet.
  const [accounts, reloadAccounts] = useLoad(
    () =>
      people
        .users(client, tenantId)
        .catch(() => ({ items: [] as AccountUser[], truncated: false })),
    [client, tenantId],
  );
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [issued, setIssued] = useState<{
    heading: string;
    url: string;
    note: string;
  } | null>(null);
  const [filter, setFilter] = useState("");
  const [confirm, setConfirm] = useState<Member | null>(null);
  const filterId = useId();

  const refresh = () => {
    reload();
    reloadAccounts();
  };

  const act = async (action: () => Promise<unknown>) => {
    setProblem(null);
    setNotice(null);
    try {
      await action();
    } catch (error) {
      setProblem(asApiError(error));
    }
    refresh();
  };

  if (list.kind === "loading")
    return <LoadingNotice label="Loading members…" />;
  if (list.kind === "error") {
    return <ErrorNotice error={list.error} retry={reload} />;
  }
  const byPrincipal = new Map<string, AccountUser>();
  if (accounts.kind === "ready") {
    for (const account of accounts.value.items) {
      byPrincipal.set(account.principal_id, account);
    }
  }
  const showLogin = byPrincipal.size > 0;
  const needle = filter.trim().toLowerCase();
  const shown = list.value.items.filter(
    (member) =>
      member.state !== "removed" &&
      (needle === "" || member.display_name.toLowerCase().includes(needle)),
  );
  const issue = (
    heading: string,
    token: string,
    expires: number,
    who: string,
  ) =>
    setIssued({
      heading,
      url: resetLink(window.location.origin, token),
      note: `Send this to ${who}. It works once and expires in ${hoursText(expires)}.`,
    });

  return (
    <>
      {canManage && (
        <AddPerson
          onCreated={(created, name) => {
            issue(
              `${name} was added.`,
              created.setup_token,
              created.expires_in_secs,
              name,
            );
            refresh();
          }}
          onProblem={setProblem}
        />
      )}
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
      {notice !== null && (
        <p role="status" className="notice ok">
          {notice}
        </p>
      )}
      {issued !== null && (
        <OneTimeLink
          heading={issued.heading}
          url={issued.url}
          note={issued.note}
          testId="onetime-link"
        />
      )}
      <table>
        <caption className="sr-only">Members of this organisation</caption>
        <thead>
          <tr>
            <th scope="col">Name</th>
            {showLogin && <th scope="col">Login</th>}
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
            const account = byPrincipal.get(member.principal_id);
            const disabled =
              member.state === "suspended" || account?.state === "disabled";
            const name = member.display_name;
            return (
              <tr key={member.principal_id}>
                <th scope="row">
                  {name}
                  {self && <span className="muted"> (you)</span>}
                </th>
                {showLogin && <td>{account?.login ?? "—"}</td>}
                <td>
                  {editable ? (
                    <select
                      aria-label={`Role of ${name}`}
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
                  {disabled ? (
                    <StateBadge state="suspended" />
                  ) : account?.locked === true ? (
                    <span className="badge">Locked</span>
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
                                account !== undefined
                                  ? disabled
                                    ? people.enableUser(
                                        client,
                                        tenantId,
                                        member.principal_id,
                                      )
                                    : people.disableUser(
                                        client,
                                        tenantId,
                                        member.principal_id,
                                      )
                                  : people.setMember(
                                      client,
                                      tenantId,
                                      member.principal_id,
                                      {
                                        state: disabled
                                          ? "active"
                                          : "suspended",
                                      },
                                    ),
                              )
                            }
                          >
                            {disabled ? `Enable ${name}` : `Disable ${name}`}
                          </button>
                          {account !== undefined && (
                            <>
                              <button
                                type="button"
                                className="button"
                                onClick={() => {
                                  setProblem(null);
                                  setNotice(null);
                                  people
                                    .resetLink(
                                      client,
                                      tenantId,
                                      member.principal_id,
                                    )
                                    .then((result) =>
                                      issue(
                                        `Password reset link for ${name}.`,
                                        result.reset_token,
                                        result.expires_in_secs,
                                        name,
                                      ),
                                    )
                                    .catch((error: unknown) =>
                                      setProblem(asApiError(error)),
                                    );
                                }}
                              >
                                {`Reset password for ${name}`}
                              </button>
                              <button
                                type="button"
                                className="button"
                                onClick={() =>
                                  void act(async () => {
                                    const result =
                                      await people.revokeUserSessions(
                                        client,
                                        tenantId,
                                        member.principal_id,
                                      );
                                    setNotice(
                                      `${name} was signed out of ${result.revoked} session${result.revoked === 1 ? "" : "s"}.`,
                                    );
                                  })
                                }
                              >
                                {`Sign ${name} out everywhere`}
                              </button>
                            </>
                          )}
                          <button
                            type="button"
                            className="button danger"
                            onClick={() => setConfirm(member)}
                          >
                            {`Remove ${name}`}
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

/** Admin creates a person with a login name; they get a one-time link to choose their password. */
function AddPerson({
  onCreated,
  onProblem,
}: {
  onCreated: (created: CreatedUser, name: string) => void;
  onProblem: (error: ApiError | null) => void;
}) {
  const { client, tenantId, role } = useReady();
  const loginId = useId();
  const nameId = useId();
  const roleId = useId();
  const [open, setOpen] = useState(false);
  const [login, setLogin] = useState("");
  const [name, setName] = useState("");
  const [newRole, setNewRole] = useState("editor");
  const [busy, setBusy] = useState(false);

  if (!open) {
    return (
      <div className="toolbar">
        <button
          type="button"
          className="button primary"
          data-testid="addperson-open"
          onClick={() => setOpen(true)}
        >
          Add a person
        </button>
      </div>
    );
  }
  return (
    <form
      className="inline-form"
      data-testid="addperson-form"
      onSubmit={(event: FormEvent) => {
        event.preventDefault();
        if (busy) return;
        setBusy(true);
        onProblem(null);
        people
          .createUser(client, tenantId, {
            login: login.trim(),
            display_name: name.trim(),
            role: newRole,
          })
          .then((created) => {
            onCreated(created, name.trim());
            setLogin("");
            setName("");
            setOpen(false);
          })
          .catch((error: unknown) => onProblem(asApiError(error)))
          .finally(() => setBusy(false));
      }}
    >
      <div className="field grow">
        <label htmlFor={nameId}>Name</label>
        <input
          id={nameId}
          data-testid="addperson-name"
          value={name}
          maxLength={64}
          required
          onChange={(event) => setName(event.target.value)}
        />
      </div>
      <div className="field grow">
        <label htmlFor={loginId}>Login name</label>
        <input
          id={loginId}
          data-testid="addperson-login"
          value={login}
          maxLength={254}
          autoCapitalize="off"
          spellCheck={false}
          required
          onChange={(event) => setLogin(event.target.value)}
        />
      </div>
      <div className="field">
        <label htmlFor={roleId}>Role</label>
        <select
          id={roleId}
          data-testid="addperson-role"
          value={newRole}
          onChange={(event) => setNewRole(event.target.value)}
        >
          {TENANT_ROLES.map((option) => (
            <option
              key={option}
              value={option}
              disabled={option === "tenant_owner" && role !== "tenant_owner"}
            >
              {roleLabel(option)}
            </option>
          ))}
        </select>
      </div>
      <button
        type="submit"
        className="button primary"
        data-testid="addperson-submit"
        disabled={busy || login.trim() === "" || name.trim() === ""}
      >
        {busy ? "Adding…" : "Create and get link"}
      </button>
      <button type="button" className="button" onClick={() => setOpen(false)}>
        Cancel
      </button>
    </form>
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
