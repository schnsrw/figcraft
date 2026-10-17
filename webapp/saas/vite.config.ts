/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Development only: `npm run dev` serves the app on :5173 and forwards the API, the editor and the
// relay to a running stack (default the Compose stack on :8080; set FIGCRAFT_STACK to change it),
// so screens can be tried against real data. The `Origin` header is replaced by the stack's own,
// because the API accepts changes only from its public origin. `vite build` ignores all of this.
const environment = (
  globalThis as { process?: { env: Record<string, string | undefined> } }
).process?.env;
const stack = environment?.FIGCRAFT_STACK ?? "http://127.0.0.1:8080";
const forward = {
  target: stack,
  changeOrigin: true,
  ws: true,
  headers: { origin: stack },
};

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    sourcemap: true,
  },
  server: {
    proxy: {
      "/api": forward,
      "/edit": forward,
      "/ws": forward,
      "/branding.json": forward,
      "/brand": forward,
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
  },
});
