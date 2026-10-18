import { Link, useRouter } from "../router";
import { AccountScreen } from "./account";
import { HomeScreen } from "./home";
import { InviteScreen } from "./invite";
import { PeopleScreen } from "./people";
import { ResetScreen } from "./reset";
import { SearchScreen } from "./search";
import { SettingsScreen } from "./settings";
import { WorkspaceScreen } from "./workspace";

/** The screens for a signed-in person acting in an organisation, chosen by the route. */
export function ReadyRoutes() {
  const { route } = useRouter();
  switch (route.name) {
    case "home":
      return <HomeScreen />;
    case "workspace":
      return (
        <WorkspaceScreen
          key={route.workspaceId}
          workspaceId={route.workspaceId}
        />
      );
    case "file":
    case "editor":
      // The web server serves the FigCraft editor at a file's URL; this app only gets here when
      // it doesn't (a development server), so say where the file is instead of a blank page.
      return <EditorElsewhere workspaceId={route.workspaceId} />;
    case "people":
      return <PeopleScreen />;
    case "invite":
      return <InviteScreen />;
    case "search":
      return <SearchScreen />;
    case "settings":
      return <SettingsScreen />;
    case "reset":
      return <ResetScreen />;
    case "account":
      return <AccountScreen />;
    case "setup":
      return (
        <section className="panel narrow">
          <h1>Already set up</h1>
          <p className="intro">This workspace already has an administrator.</p>
          <Link className="button primary" to="/">
            Go to your files
          </Link>
        </section>
      );
    case "not_found":
      return null;
  }
}

/** A file's URL rendered by this app: the editor isn't served here. */
function EditorElsewhere({ workspaceId }: { workspaceId: string }) {
  return (
    <section className="panel narrow" aria-labelledby="editor-elsewhere">
      <h1 id="editor-elsewhere">Files open in the editor</h1>
      <p>
        This address opens the file in the FigCraft editor, which this server
        doesn&apos;t serve.
      </p>
      <Link to={`/workspace/${workspaceId}`}>Back to files</Link>
    </section>
  );
}
