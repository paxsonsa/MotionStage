import { Component, StrictMode, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
// Self-hosted fonts (bundled into the binary — the UI must work fully offline).
import "@fontsource/saira-condensed/500.css";
import "@fontsource/saira-condensed/600.css";
import "@fontsource/saira-condensed/700.css";
import "@fontsource/saira-semi-condensed/500.css";
import "@fontsource/saira-semi-condensed/600.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";
import "@fontsource/ibm-plex-mono/600.css";
import App from "./App";
import "./styles.css";

// Never let a render error blank the screen — show it instead, with a reload.
class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };
  static getDerivedStateFromError(error: Error) { return { error }; }
  componentDidCatch(error: Error) { console.error("UI crash:", error); }
  render() {
    if (this.state.error) {
      return (
        <div style={{ padding: 24, fontFamily: "ui-monospace, monospace", color: "#ffb0a8", background: "#0b0b0f", height: "100vh" }}>
          <div style={{ fontWeight: 700, marginBottom: 8 }}>UI error</div>
          <pre style={{ whiteSpace: "pre-wrap", color: "#e9e7e0" }}>{String(this.state.error?.message ?? this.state.error)}</pre>
          <button onClick={() => this.setState({ error: null })} style={{ marginTop: 12, padding: "6px 14px", cursor: "pointer" }}>Dismiss</button>
        </div>
      );
    }
    return this.props.children;
  }
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>,
);
