import { defineConfig } from "vite";

export default defineConfig({
  // served by Rocket at / — absolute base keeps /tiles + /api calls same-origin
  base: "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
  },
  server: {
    port: 5174,
    proxy: {
      // local dev against the embedded/standalone server on 5150
      "/tiles": "http://127.0.0.1:5150",
      "/vector": "http://127.0.0.1:5150",
      "/terrain": "http://127.0.0.1:5150",
      "/api": "http://127.0.0.1:5150",
      "/health": "http://127.0.0.1:5150",
    },
  },
});
