import { defineConfig } from "vitepress";

const repo = "https://github.com/genesysflow/ferrix";

export default defineConfig({
  title: "Ferrix",
  description: "A reactive backend in Rust that ships as a single binary.",
  // Served from https://genesysflow.github.io/ferrix/ (DOCS_BASE overrides it, e.g. "/" for a custom domain).
  base: process.env.DOCS_BASE ?? "/ferrix/",
  cleanUrls: true,
  // Links to a local dev server (http://localhost:5173) are instructions, not pages.
  ignoreDeadLinks: "localhostLinks",
  lastUpdated: true,
  head: [["link", { rel: "icon", type: "image/svg+xml", href: `${process.env.DOCS_BASE ?? "/ferrix/"}logo.svg` }]],
  themeConfig: {
    logo: "/logo.svg",
    nav: [
      { text: "Get started", link: "/getting-started" },
      { text: "Guide", link: "/guide/functions" },
      { text: "Operations", link: "/operations/deploying" },
      { text: "Reference", items: [
        { text: "API contract", link: "/CONTRACT" },
        { text: "Protocol", link: "/PROTOCOL" },
      ] },
    ],
    sidebar: [
      {
        text: "Introduction",
        items: [
          { text: "What is Ferrix?", link: "/" },
          { text: "Get started", link: "/getting-started" },
        ],
      },
      {
        text: "Guide",
        items: [
          { text: "Functions, tables and indexes", link: "/guide/functions" },
          { text: "Auth", link: "/guide/auth" },
          { text: "File storage", link: "/guide/file-storage" },
          { text: "Scheduler and crons", link: "/guide/scheduler" },
          { text: "Workflows", link: "/guide/workflows" },
          { text: "Search", link: "/guide/search" },
          { text: "Live aggregates", link: "/guide/aggregates" },
          { text: "Environment variables", link: "/guide/environment" },
          { text: "Analytics (OLAP)", link: "/guide/analytics" },
          { text: "Migrations", link: "/guide/migrations" },
          { text: "Binary data", link: "/guide/binary-data" },
          { text: "The dashboard", link: "/guide/dashboard" },
          { text: "Things to know", link: "/guide/things-to-know" },
        ],
      },
      {
        text: "Operations",
        items: [
          { text: "Deploying", link: "/operations/deploying" },
          { text: "Deploys with no downtime", link: "/operations/zero-downtime" },
          { text: "Backups and restores", link: "/operations/backups" },
          { text: "Logs and metrics", link: "/operations/monitoring" },
        ],
      },
      {
        text: "Reference",
        items: [
          { text: "API contract", link: "/CONTRACT" },
          { text: "Protocol", link: "/PROTOCOL" },
        ],
      },
    ],
    outline: { level: [2, 3] },
    search: { provider: "local" },
    socialLinks: [{ icon: "github", link: repo }],
    editLink: { pattern: `${repo}/edit/master/docs/:path`, text: "Edit this page on GitHub" },
  },
});
