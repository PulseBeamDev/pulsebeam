import { defineConfig } from "vitepress";
import { withMermaid } from "vitepress-plugin-mermaid";

export default withMermaid(
  defineConfig({
    title: "PulseBeam",
    description: "Build real-time video, audio, and room messaging with PulseBeam",
    cleanUrls: true,
    srcExclude: ["README.md", "AGENTS.md"],
    vite: {
      optimizeDeps: {
        include: [
          "mermaid > @braintree/sanitize-url",
          "mermaid > cytoscape",
          "mermaid > cytoscape-cose-bilkent",
          "mermaid > dayjs",
        ],
      },
    },
    head: [
      ["link", { rel: "icon", href: "https://pulsebeam.dev/favicon.svg" }],
    ],
    themeConfig: {
      nav: [
        { text: "Documentation", link: "/" },
        { text: "PulseBeam", link: "https://pulsebeam.dev" },
      ],
      sidebar: [
        {
          text: "Start here",
          items: [
            { text: "Build with PulseBeam", link: "/" },
            { text: "Getting started", link: "/quickstart" },
          ],
        },
        {
          text: "React SDK",
          items: [
            { text: "Quickstart", link: "/react/quickstart" },
            { text: "Concepts", link: "/react/" },
            { text: "Add a feature", link: "/react/guides" },
            { text: "API reference", link: "/react/reference" },
            { text: "Fix a problem", link: "/react/troubleshooting" },
          ],
        },
        {
          text: "Other integrations",
          items: [{ text: "WHIP publishing", link: "/whip" }],
        },
        {
          text: "Operate a server",
          items: [{ text: "Deployment", link: "/deployment" }],
        },
        {
          text: "Contribute",
          items: [
            { text: "Build and development", link: "/build" },
            { text: "Editor setup", link: "/ide" },
            { text: "Releasing", link: "/releasing" },
          ],
        },
      ],
      socialLinks: [
        { icon: "github", link: "https://github.com/PulseBeamDev/pulsebeam" },
      ],
      search: { provider: "local" },
      editLink: {
        pattern: "https://github.com/PulseBeamDev/pulsebeam/edit/main/docs/:path",
        text: "Edit this page on GitHub",
      },
      footer: {
        message: "Released under the GNU Affero General Public License v3.0.",
        copyright: "Copyright © PulseBeam contributors",
      },
    },
  }),
);
