import { defineConfig } from "vitepress";

const repo = "https://github.com/genesysflow/rxferrix";

export default defineConfig({
  title: "RxFerrix",
  description: "A reactive backend in Rust that ships as a single binary.",
  // Served from https://genesysflow.github.io/rxferrix/ (DOCS_BASE overrides it, e.g. "/" for a custom domain).
  base: process.env.DOCS_BASE ?? "/rxferrix/",
  cleanUrls: true,
  // Links to a local dev server (http://localhost:5173) are instructions, not pages.
  ignoreDeadLinks: "localhostLinks",
  lastUpdated: true,
  head: [["link", { rel: "icon", type: "image/svg+xml", href: `${process.env.DOCS_BASE ?? "/rxferrix/"}logo.svg` }]],
  themeConfig: {
    logo: "/logo.svg",
    nav: [
      { text: "Get started", link: "/getting-started" },
      { text: "Guide", link: "/guide/concepts" },
      { text: "Operations", link: "/operations/deploying" },
      { text: "Reference", items: [
        { text: "Rust API", link: "/CONTRACT" },
        { text: "Wire protocol", link: "/PROTOCOL" },
        { text: "Design", link: "/DESIGN" },
        { text: "Command line", link: "/reference/cli" },
      ] },
    ],
    sidebar: [
      {
        text: "Introduction",
        items: [
          { text: "What is RxFerrix?", link: "/" },
          { text: "Get started", link: "/getting-started" },
          { text: "Core concepts", link: "/guide/concepts" },
        ],
      },
      {
        text: "Guide",
        items: [
          { text: "Functions and data", link: "/guide/functions" },
          { text: "Connect a client", link: "/guide/clients" },
          { text: "Rust and TypeScript types", link: "/guide/types" },
          { text: "Authentication and authorization", link: "/guide/auth" },
          { text: "File storage", link: "/guide/file-storage" },
          { text: "Scheduler and crons", link: "/guide/scheduler" },
          { text: "Workflows", link: "/guide/workflows" },
          { text: "Search", link: "/guide/search" },
          { text: "Live aggregates", link: "/guide/aggregates" },
          { text: "Environment variables", link: "/guide/environment" },
          { text: "Analytics (OLAP)", link: "/guide/analytics" },
          { text: "Migrations", link: "/guide/migrations" },
          { text: "Testing", link: "/guide/testing" },
          { text: "Binary data", link: "/guide/binary-data" },
          { text: "The dashboard", link: "/guide/dashboard" },
          { text: "Behavior and limits", link: "/guide/things-to-know" },
        ],
      },
      {
        text: "Operations",
        items: [
          { text: "Deploy an app", link: "/operations/deploying" },
          { text: "Update a running app", link: "/operations/zero-downtime" },
          { text: "Backups and restores", link: "/operations/backups" },
          { text: "Logs and metrics", link: "/operations/monitoring" },
        ],
      },
      {
        text: "Reference",
        items: [
          { text: "Rust API", link: "/CONTRACT" },
          { text: "Wire protocol", link: "/PROTOCOL" },
          { text: "Design", link: "/DESIGN" },
          { text: "Command line", link: "/reference/cli" },
        ],
      },
    ],
    outline: { level: [2, 3] },
    search: { provider: "local" },
    socialLinks: [{ icon: "github", link: repo }],
    editLink: { pattern: `${repo}/edit/master/docs/:path`, text: "Edit this page on GitHub" },
  },
});
