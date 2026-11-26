#!/usr/bin/env bun
// Local preview of the landing page with hot reload. PORT picks the port (default 3000).
import index from "./src/index.html";

const server = Bun.serve({
  development: true,
  fetch: async (request) => {
    const file = Bun.file(`public${new URL(request.url).pathname}`);
    return (await file.exists()) ? new Response(file) : new Response("Not found", { status: 404 });
  },
  hostname: "127.0.0.1",
  port: Number(process.env.PORT ?? 3000),
  routes: { "/": index },
});

console.log(`GitSync landing page on ${server.url}`);
