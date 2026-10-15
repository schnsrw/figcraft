const workspaceItems = ["Recent", "Projects", "Libraries"] as const;

export function App() {
  return (
    <div className="app-shell">
      <header className="topbar">
        <a className="brand" href="/" aria-label="FigCraft workspace home">
          <span className="brand-mark" aria-hidden="true" />
          <span>FigCraft</span>
        </a>
        <span className="environment-label">SaaS workspace</span>
      </header>

      <div className="workspace-layout">
        <nav className="sidebar" aria-label="Workspace">
          <p className="eyebrow">Workspace</p>
          <ul>
            {workspaceItems.map((item, index) => (
              <li key={item}>
                <a className={index === 0 ? "active" : undefined} href={`#${item.toLowerCase()}`}>
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
            The account and workspace shell is ready. The existing Rust/WASM editor will mount into
            this application through a versioned bridge.
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
    </div>
  );
}
