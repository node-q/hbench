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
  // The viewer accepts only its own Host and Origin: the proxy presents the target's, and drops the dev
  // server's Origin. Open the dev server with the `#k=<key>` that `hbench view` printed.
  server: {
    proxy: {
      "/api": {
        target: process.env.HBENCH_API ?? "http://127.0.0.1:8091",
        changeOrigin: true,
        configure: (proxy) => proxy.on("proxyReq", (req) => req.removeHeader("origin")),
      },
    },
  },
});
