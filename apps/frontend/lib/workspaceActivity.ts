import * as React from "react";
import type { ActivityProps, ExoticComponent } from "react";

function resolveActivity(): ExoticComponent<ActivityProps> {
  for (const name of ["Activity", "unstable_Activity"]) {
    const exported = Reflect.get(React, name);
    if (exported) return exported;
  }
  if (React.version === "19.2.0-canary-0bdb9206-20250818") {
    return Symbol.for("react.activity") as unknown as ExoticComponent<ActivityProps>;
  }
  throw new Error(`当前 React ${React.version} 未提供工作区所需的 Activity 组件`);
}

export const WorkspaceActivity = resolveActivity();
