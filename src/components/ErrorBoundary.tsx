import { Component, type ErrorInfo, type ReactNode } from "react";

interface ErrorBoundaryProps {
  children: ReactNode;
  context: string;
  /**
   * Optional component rendered in the failed state. Omitting it preserves
   * the historical behavior (render nothing); callers that own the window
   * the subtree lives in (the overlay) pass a fallback that recovers it.
   */
  fallback?: React.ComponentType;
}

interface ErrorBoundaryState {
  failed: boolean;
}

/** Prevents a non-critical UI subtree from blanking the entire app. */
export class ErrorBoundary extends Component<
  ErrorBoundaryProps,
  ErrorBoundaryState
> {
  state: ErrorBoundaryState = { failed: false };

  static getDerivedStateFromError(): ErrorBoundaryState {
    return { failed: true };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error(
      `Error rendering ${this.props.context}:`,
      error,
      info.componentStack,
    );
  }

  render(): ReactNode {
    if (this.state.failed) {
      const Fallback = this.props.fallback;
      return Fallback ? <Fallback /> : null;
    }
    return this.props.children;
  }
}
