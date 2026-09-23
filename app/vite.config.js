import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The backend has no CORS layer, so the dev server proxies it same-origin.
// ponytail: dev proxy; add tower-http CorsLayer to backend/ when this deploys
// anywhere the two aren't served from one origin.
const BACKEND = process.env.VITE_BACKEND || "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react()],
  define: { global: "globalThis" },
  server: {
    fs: { allow: [".", "../target/idl"] },
    proxy: {
      "/api": { target: BACKEND, changeOrigin: true, rewrite: p => p.replace(/^\/api/, "") },
      "/ws": { target: BACKEND, ws: true }
    }
  }
});
