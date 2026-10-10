import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { FerrixClient, FerrixProvider } from "@ferrix/react";
import { App } from "./App";
import "./styles.css";

// Same origin: Vite proxies /api (HTTP and the WebSocket) to the Ferrix server.
const client = new FerrixClient(window.location.origin);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <FerrixProvider client={client}>
      <App />
    </FerrixProvider>
  </StrictMode>,
);
