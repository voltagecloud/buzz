// Production build of buzz-app fixture pages whose dev-mode CSS layer order
// differs from the shipped app (dialog atlas, agent-control, composer gallery).
const BUZZ_APP = process.env.BUZZ_APP ?? "/home/ben/projects/buzz-app";
const { default: react } = await import(`${BUZZ_APP}/node_modules/@vitejs/plugin-react/dist/index.js`);
export default {
  root: BUZZ_APP,
  configFile: false,
  envDir: false,
  base: "./",
  publicDir: `${BUZZ_APP}/public`,
  plugins: [
    react(),
    {
      // These fixtures skip src/main.tsx, which is where the app imports fonts.
      name: "reference-fixture-fonts",
      transform(code, id) {
        if (!/tests\/fixtures\/(dialog-gallery\/main|agent-control|product-ui\/main)\.tsx$/.test(id)) return;
        return `${code}
import "@fontsource-variable/inter/wght.css";
import "@fontsource-variable/inter/wght-italic.css";
import "@fontsource/jetbrains-mono/400.css";
`;
      },
    },
  ],
  logLevel: "warn",
  build: {
    outDir: process.env.FIXTURE_OUT ?? "/tmp/buzz-fixtures-dist",
    emptyOutDir: true,
    sourcemap: false,
    rollupOptions: {
      input: [
        `${BUZZ_APP}/tests/fixtures/dialog-gallery.html`,
        `${BUZZ_APP}/tests/fixtures/agent-control.html`,
        `${BUZZ_APP}/tests/fixtures/product-ui.html`,
      ],
    },
  },
};
