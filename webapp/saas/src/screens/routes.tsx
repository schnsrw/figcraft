import { useRouter } from "../router";

/** The screens for a signed-in person acting in an organisation, chosen by the route. */
export function ReadyRoutes() {
  const { route } = useRouter();
  switch (route.name) {
    case "home":
      return <h1>Your files</h1>;
    case "workspace":
      return <h1>Workspace</h1>;
    case "file":
      return <h1>File</h1>;
    case "people":
      return <h1>People</h1>;
    case "invite":
      return <h1>Invitation</h1>;
    case "not_found":
      return null;
  }
}
