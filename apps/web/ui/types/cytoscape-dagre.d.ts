// cytoscape-dagre ships no types; it's a cytoscape extension registered via
// cytoscape.use(). Declaring it as a plugin function is enough for our usage.
declare module "cytoscape-dagre" {
  import type cytoscape from "cytoscape";
  const ext: cytoscape.Ext;
  export default ext;
}
