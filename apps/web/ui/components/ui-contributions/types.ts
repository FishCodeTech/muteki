/**
 * EXT-02：声明式 UI Contribution 的类型契约。
 *
 * 扩展在 manifest 的 `ui` 字段声明一个 JSON 文件（见
 * examples/extensions/hello/ui/contributions.json），内容必须落在这里定义的
 * 声明式 schema 内。渲染器只按这份 schema 解释数据，**不加载第三方任意
 * React / JS 代码**；复杂界面只能通过 `artifact_viewers` 的 `iframe` 种类
 * 以受控沙箱 iframe 呈现（无 allow-scripts，内容来自公开 API 的 projection
 * 数据）。
 */

/** 表单字段（命令表单与创建表单共用）。 */
export type ContributionField = {
  name: string;
  type?: "string" | "integer" | "number" | "boolean";
  label?: string;
  required?: boolean;
  enum?: (string | number)[];
  default?: unknown;
  placeholder?: string;
};

/** 命令表单：渲染成一组输入 + 提交按钮，提交即调用扩展业务命令。 */
export type CommandFormContribution = {
  command_type: string;
  title: string;
  description?: string;
  fields: ContributionField[];
};

/** 导航项：只声明标题与路由，由宿主渲染成链接。 */
export type NavigationContribution = {
  id: string;
  title: string;
  route: string;
  icon?: string;
};

/** 状态标签：把某个字段的取值映射为 文案 + 颜色。 */
export type StatusLabelContribution = {
  field: string;
  labels: Record<
    string,
    { text?: string; color?: "green" | "red" | "amber" | "blue" | "muted" }
  >;
};

/** 看板：从扩展公开 projection 读取条目，按状态字段分列渲染卡片。 */
export type BoardContribution = {
  /** projection 名（缺省读默认 projection）。 */
  projection?: string;
  /** projection data 中条目列表所在字段；缺省把整个 data 当成单条目。 */
  items_field?: string;
  /** 条目上用于分列的状态字段。 */
  status_field: string;
  columns: { id: string; title: string; statuses: string[] }[];
  card: { title_field: string; subtitle_field?: string; badge_field?: string };
  status_labels?: StatusLabelContribution;
};

/** Artifact viewer：呈现 projection 内容；iframe 种类走受控沙箱。 */
export type ArtifactViewerContribution = {
  id: string;
  title: string;
  kind: "text" | "json" | "iframe";
  projection?: string;
  /** 取 projection data 的哪个字段作为内容；缺省用整个 data。 */
  field?: string;
};

/** 一份扩展的 UI Contribution 描述文件。 */
export type UiContributions = {
  navigation?: NavigationContribution[];
  command_forms?: CommandFormContribution[];
  board?: BoardContribution;
  status_labels?: StatusLabelContribution;
  artifact_viewers?: ArtifactViewerContribution[];
};

/** 设置 schema（JSON Schema 子集，与 EXT-01 validate_against_schema 同口径）。 */
export type JsonSchemaSubset = {
  type?: string;
  properties?: Record<string, JsonSchemaSubset>;
  required?: string[];
  enum?: unknown[];
  additionalProperties?: boolean;
  items?: JsonSchemaSubset;
};

export const STATUS_COLORS: Record<string, string> = {
  green: "var(--green)",
  red: "var(--red)",
  amber: "var(--amber)",
  blue: "var(--blue)",
  muted: "var(--muted)",
};
