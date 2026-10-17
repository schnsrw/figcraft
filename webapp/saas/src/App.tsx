import { type ReactNode, useEffect, useRef } from "react";

import type { ApiClient } from "./api/client";
import { type Branding, DEFAULT_BRANDING } from "./branding";
import { Link, RouterProvider, signInHref, useRouter } from "./router";
import {
  AccountMenu,
  NoTenantScreen,
  NotFoundScreen,
  SignedOutScreen,
  TenantPicker,
  UnavailableScreen,
  UnsupportedScreen,
} from "./screens/entry";
import { BootstrapScreen } from "./screens/bootstrap";
import { InviteScreen, captureInviteToken } from "./screens/invite";
import { ReadyRoutes } from "./screens/routes";
import { SessionProvider, useSession } from "./session";

export interface AppProps {
  /** Validated runtime branding (`parseBranding`); defaults to the FigCraft look. */
  branding?: Branding;
  /** Tests inject a client around a fake `fetch`. */
  client?: ApiClient;
}

export function App({ branding = DEFAULT_BRANDING, client }: AppProps) {
  return (
    <SessionProvider {...(client === undefined ? {} : { client })}>
      <RouterProvider>
        <Shell branding={branding} />
      </RouterProvider>
    </SessionProvider>
  );
}

function Shell({ branding }: { branding: Branding }) {
  const footerLinks = [
    { label: "Support", href: branding.supportUrl },
    { label: "Terms", href: branding.termsUrl },
    { label: "Privacy", href: branding.privacyUrl },
  ].filter(
    (link): link is { label: string; href: string } => link.href !== null,
  );
  const showFooter = footerLinks.length > 0 || branding.footerText !== null;
  const { state } = useSession();

  return (
    <div className="app-shell">
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <header className="topbar">
        <Link
          className="brand"
          to="/"
          aria-label={`${branding.productName} workspace home`}
        >
          {branding.logoUrl === null ? (
            <span className="brand-mark" aria-hidden="true" />
          ) : (
            <img
              className="brand-logo"
              src={branding.logoUrl}
              alt=""
              aria-hidden="true"
            />
          )}
          <span>{branding.productName}</span>
        </Link>
        {state.kind === "ready" && (
          <nav className="topnav" aria-label="Main">
            <Link to="/">Files</Link>
            <Link to="/people">People</Link>
          </nav>
        )}
        <AccountMenu />
      </header>

      <Main>
        <Body branding={branding} />
      </Main>

      {showFooter && (
        <footer className="site-footer">
          {branding.footerText !== null && <span>{branding.footerText}</span>}
          {footerLinks.length > 0 && (
            <nav aria-label="Legal and support">
              {footerLinks.map((link) => (
                <a key={link.label} href={link.href} rel="noopener noreferrer">
                  {link.label}
                </a>
              ))}
            </nav>
          )}
        </footer>
      )}
    </div>
  );
}

/** The page's `<main>`: takes focus when the route changes so keyboard and screen-reader users land on the new content. */
function Main({ children }: { children: ReactNode }) {
  const { path } = useRouter();
  const ref = useRef<HTMLElement>(null);
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    ref.current?.focus({ preventScroll: true });
  }, [path]);
  return (
    <main id="main" className="page" tabIndex={-1} ref={ref}>
      {children}
    </main>
  );
}

function Body({ branding }: { branding: Branding }) {
  const { state } = useSession();
  const { path, route } = useRouter();
  useDocumentTitle(branding, route.name);
  // An invitation token arrives in the fragment; keep it (for the sign-in round trip) and hide it.
  useEffect(() => {
    if (route.name === "invite") captureInviteToken();
  }, [route.name]);

  switch (state.kind) {
    case "loading":
      return (
        <p role="status" className="muted">
          Loading…
        </p>
      );
    case "signed_out":
      return (
        <SignedOutScreen
          productName={branding.productName}
          signInHref={signInHref(path)}
          deepLink={
            route.name === "file" ||
            route.name === "workspace" ||
            route.name === "invite"
          }
        />
      );
    case "unavailable":
      return <UnavailableScreen error={state.error} />;
    case "unsupported":
      return <UnsupportedScreen credential={state.credential} />;
    case "bootstrap_required":
      return <BootstrapScreen />;
    case "choose_tenant":
      // An invitation can be accepted before choosing an organisation.
      return route.name === "invite" ? (
        <InviteScreen />
      ) : (
        <TenantPicker tenants={state.tenants} />
      );
    case "no_tenant":
      return route.name === "invite" ? (
        <InviteScreen />
      ) : (
        <NoTenantScreen deployment={state.session.deployment} />
      );
    case "ready":
      return route.name === "not_found" ? <NotFoundScreen /> : <ReadyRoutes />;
  }
}

const TITLES: Record<string, string> = {
  home: "Files",
  workspace: "Workspace",
  file: "File",
  people: "People",
  invite: "Invitation",
  not_found: "Page not found",
};

function useDocumentTitle(branding: Branding, route: string) {
  useEffect(() => {
    document.title = `${TITLES[route] ?? ""} · ${branding.productName}`;
  }, [branding.productName, route]);
}
