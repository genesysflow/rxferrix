import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { RxFerrixClient, RxFerrixProvider, getAuth } from "@rxferrix/react";
import { App } from "./App";
import "./styles.css";

// Same origin: Vite proxies /api (HTTP and the WebSocket) to the RxFerrix server.
const client = new RxFerrixClient(window.location.origin);
// Restores a stored session (tokens in localStorage) and keeps the socket authenticated.
getAuth(client);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <RxFerrixProvider client={client}>
      <App />
    </RxFerrixProvider>
  </StrictMode>,
);
