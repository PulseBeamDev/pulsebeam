import { defineConfig } from "vitepress";
import { withMermaid } from "vitepress-plugin-mermaid";

export default withMermaid(
  defineConfig({
    title: "PulseBeam",
    description: "Official documentation for the PulseBeam WebRTC SFU",
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
          text: "Guide",
          items: [
            { text: "Introduction", link: "/" },
            { text: "Quickstart", link: "/quickstart" },
            { text: "Deployment", link: "/deployment" },
          ],
        },
        {
          text: "React SDK",
          items: [
            { text: "Start here", link: "/react/" },
            { text: "Run the demo", link: "/react/quickstart" },
            { text: "Add a feature", link: "/react/guides" },
            { text: "API reference", link: "/react/reference" },
            { text: "Fix a problem", link: "/react/troubleshooting" },
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
