import { Component, type ReactNode } from "react";

interface Props {
  /** Renders the caught error. */
  fallback: (error: unknown, reset: () => void) => ReactNode;
  children?: ReactNode;
}

/**
 * Catches errors thrown while rendering, in particular the `RxFerrixError` that `useQuery` / `usePaginatedQuery`
 * throw when a query fails (e.g. `not_a_member`). Give it a `key` to reset it when its subject changes.
 */
export class ErrorBoundary extends Component<Props, { error: unknown }> {
  override state: { error: unknown } = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return { error };
  }

  reset = () => this.setState({ error: null });

  override render() {
    if (this.state.error !== null) return this.props.fallback(this.state.error, this.reset);
    return this.props.children;
  }
}
