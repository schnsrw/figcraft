import { type Branding, DEFAULT_BRANDING } from "./branding";

const workspaceItems = ["Recent", "Projects", "Libraries"] as const;

export interface AppProps {
  /** Validated runtime branding (`parseBranding`); defaults to the FigCraft look. */
  branding?: Branding;
}

export function App({ branding = DEFAULT_BRANDING }: AppProps) {
  const footerLinks = [
    { label: "Support", href: branding.supportUrl },
    { label: "Terms", href: branding.termsUrl },
    { label: "Privacy", href: branding.privacyUrl },
  ].filter(
    (link): link is { label: string; href: string } => link.href !== null,
  );
  const showFooter = footerLinks.length > 0 || branding.footerText !== null;

  return (
    <div className="app-shell">
      <header className="topbar">
        <a
          className="brand"
          href="/"
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
        </a>
        <span className="environment-label">SaaS workspace</span>
      </header>

      <div className="workspace-layout">
        <nav className="sidebar" aria-label="Workspace">
          <p className="eyebrow">Workspace</p>
          <ul>
            {workspaceItems.map((item, index) => (
              <li key={item}>
                <a
                  className={index === 0 ? "active" : undefined}
                  href={`#${item.toLowerCase()}`}
                >
                  {item}
                </a>
              </li>
            ))}
          </ul>
        </nav>

        <main className="workspace-main">
          <p className="eyebrow">Cloud foundation</p>
          <h1>Your design workspace</h1>
          <p className="intro">
            The account and workspace shell is ready. The existing Rust/WASM
            editor will mount into this application through a versioned bridge.
          </p>

          <section className="status-card" aria-labelledby="runtime-heading">
            <div>
              <p className="status-label">Editor runtime</p>
              <h2 id="runtime-heading">Rust/WASM boundary preserved</h2>
            </div>
            <span className="status-pill">Foundation</span>
          </section>
        </main>
      </div>

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
