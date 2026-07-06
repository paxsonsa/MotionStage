import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Relative base so the embedded SPA works regardless of the ephemeral port/host
// the runtime binds. Output goes to ui/dist, which the Rust crate embeds.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
