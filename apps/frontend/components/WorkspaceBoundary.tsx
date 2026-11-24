"use client";

import { Component, type ErrorInfo, type ReactNode } from "react";
import { Button } from "@muteki/ui";

type Props = { name: string; resetKey: string; onRetry?: () => void; children: ReactNode };
type State = { failed: boolean; resetKey: string };

export class WorkspaceBoundary extends Component<Props, State> {
  state: State = { failed: false, resetKey: this.props.resetKey };

  static getDerivedStateFromError(): Partial<State> { return { failed: true }; }

  static getDerivedStateFromProps(props: Props, state: State): Partial<State> | null {
    return props.resetKey !== state.resetKey ? { failed: false, resetKey: props.resetKey } : null;
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("muteki.workspace.render_failed", { workspace: this.props.name, error, componentStack: info.componentStack });
  }

  private retry = () => {
    this.props.onRetry?.();
    this.setState({ failed: false });
  };

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <div className="workspace-route-state" data-workspace-error={this.props.name}>
        <section className="workspace-route-card" aria-label={`${this.props.name}加载失败`}>
          <h1>{this.props.name}暂时无法显示</h1>
          <p role="alert">工作区加载失败。可以重新尝试，或通过侧栏打开其他工作区。</p>
          <div className="workspace-route-actions"><Button variant="primary" onClick={this.retry}>重新尝试</Button></div>
        </section>
      </div>
    );
  }
}
