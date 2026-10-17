import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { applyBranding, loadBranding } from "./branding";
import "./styles.css";

const element = document.getElementById("root");

if (element !== null) {
  // Render once the deployment's `/branding.json` arrives, so a rebranded deployment never
  // flashes the default name; a failed, slow or invalid fetch renders the defaults.
  void loadBranding((input, init) => fetch(input, init)).then((branding) => {
    applyBranding(document, branding);
    createRoot(element).render(
      <StrictMode>
        <App branding={branding} />
      </StrictMode>,
    );
  });
}
