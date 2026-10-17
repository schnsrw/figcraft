import { Link, useRouter } from "../router";
import { AccountScreen } from "./account";
import { FileScreen } from "./file";
import { HomeScreen } from "./home";
import { InviteScreen } from "./invite";
import { PeopleScreen } from "./people";
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
      return (
        <FileScreen
          key={route.documentId}
          workspaceId={route.workspaceId}
          documentId={route.documentId}
        />
      );
    case "people":
      return <PeopleScreen />;
    case "invite":
      return <InviteScreen />;
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
