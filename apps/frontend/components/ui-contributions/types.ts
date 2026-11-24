
export type ContributionField = {
  name: string;
  type?: "string" | "integer" | "number" | "boolean";
  label?: string;
  required?: boolean;
  enum?: (string | number)[];
  default?: unknown;
  placeholder?: string;
};

export type CommandFormContribution = {
  command_type: string;
  id?: string;
  title: string;
  description?: string;
  fields: ContributionField[];
};

export type NavigationContribution = {
  id: string;
  title: string;
  route: string;
  icon?: string;
};

export type StatusLabelContribution = {
  field: string;
  labels: Record<
    string,
    { text?: string; color?: "green" | "red" | "amber" | "blue" | "muted" }
  >;
};

export type BoardContribution = {
  projection?: string;
  items_field?: string;
  status_field: string;
  columns: { id: string; title: string; statuses: string[] }[];
  card: { title_field: string; subtitle_field?: string; badge_field?: string };
  status_labels?: StatusLabelContribution;
};

export type ArtifactViewerContribution = {
  id: string;
  title: string;
  kind: "text" | "json" | "iframe";
  projection?: string;
  field?: string;
};

export type UiContributions = {
  navigation?: NavigationContribution[];
  command_forms?: CommandFormContribution[];
  board?: BoardContribution;
  status_labels?: StatusLabelContribution;
  artifact_viewers?: ArtifactViewerContribution[];
};

export type JsonSchemaSubset = {
  type?: string;
  properties?: Record<string, JsonSchemaSubset>;
  required?: string[];
  enum?: unknown[];
  additionalProperties?: boolean;
  items?: JsonSchemaSubset;
};
