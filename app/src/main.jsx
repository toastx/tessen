import "./polyfill"; // must stay first — see the comment in polyfill.js

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { ConnectionProvider, WalletProvider } from "@solana/wallet-adapter-react";
import { WalletModalProvider } from "@solana/wallet-adapter-react-ui";
import "@solana/wallet-adapter-react-ui/styles.css";
import "./nocturne.css";
import "./app.css";
import { CLUSTERS, initialCluster, rememberCluster } from "./cluster";
import App from "./App";

function Root() {
  const [cluster, setCluster] = useState(initialCluster);
  const pick = c => { rememberCluster(c); setCluster(c); };
  return (
    // keyed on the endpoint so switching clusters rebuilds the connection and
    // drops every account read taken against the old one
    <ConnectionProvider key={cluster} endpoint={CLUSTERS[cluster].endpoint}>
      {/* empty wallets array: modern wallets register themselves via the
          Wallet Standard, so @solana/wallet-adapter-wallets isn't needed */}
      <WalletProvider wallets={[]} autoConnect>
        <WalletModalProvider>
          <App cluster={cluster} onCluster={pick} />
        </WalletModalProvider>
      </WalletProvider>
    </ConnectionProvider>
  );
}

createRoot(document.getElementById("root")).render(<Root />);
