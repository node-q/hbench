import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `npm run dev` proxies the API to a running `hbench view <folder> --port 8091`.
export default defineConfig({
  plugins: [react()],
  // Stable file names: the server sends no cache headers, so content hashes only churn git.
  build: {
    outDir: "dist",
    emptyOutDir: true,
    chunkSizeWarningLimit: 1500,
    rollupOptions: { output: { entryFileNames: "assets/index.js", assetFileNames: "assets/[name][extname]" } },
  },
  server: { proxy: { "/api": process.env.HBENCH_API ?? "http://127.0.0.1:8091" } },
});
