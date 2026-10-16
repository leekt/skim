import { defineConfig } from "vite";
import { viteSingleFile } from "vite-plugin-singlefile";

// One self-contained HTML file (skim embeds it and serves it at /dashboard).
export default defineConfig({
  plugins: [viteSingleFile()],
  oxc: { jsx: { runtime: "automatic" } },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      // Kumo's modules start with "use client" (a React Server Components hint, irrelevant here).
      onwarn(w, warn) {
        if (w.code !== "MODULE_LEVEL_DIRECTIVE") warn(w);
      },
    },
  },
  server: { proxy: { "/rpc": { target: "http://127.0.0.1:8545", rewrite: () => "/" } } },
});
