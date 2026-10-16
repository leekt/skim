import "@cloudflare/kumo/styles/standalone";
import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { setByPath } from "@cloudflare/kumo/catalog";
import tree from "./dashboard.json";
import { Render } from "./render.jsx";

// Same origin as skim's JSON-RPC in production; the dev server proxies /rpc to it.
const RPC = import.meta.env.DEV ? "/rpc" : "/";
const POLL_MS = 2000;

async function fetchDashboard() {
  const r = await fetch(RPC, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "skim_dashboard", params: [] }),
  });
  const j = await r.json();
  if (j.error) throw new Error(j.error.message);
  return j.result;
}

function App() {
  const [data, setData] = useState(null);
  // The tab is in the URL hash, so /dashboard#peers links to a tab.
  const [ui, setUi] = useState({ tab: location.hash.slice(1) || "overview", error: "" });
  useEffect(() => { history.replaceState(null, "", `#${ui.tab}`); }, [ui.tab]);
  const dark = window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
  useEffect(() => {
    document.documentElement.dataset.mode = dark ? "dark" : "light";
    let stop = false;
    const tick = async () => {
      try {
        const d = await fetchDashboard();
        if (!stop) {
          setData(d);
          setUi((u) => ({ ...u, error: "" }));
        }
      } catch (e) {
        if (!stop) setUi((u) => ({ ...u, error: `skim not reachable: ${e.message}` }));
      }
      if (!stop) setTimeout(tick, POLL_MS);
    };
    tick();
    return () => { stop = true; };
  }, [dark]);
  const actions = {
    set_ui: ({ field, value }) => setUi((u) => { const n = { ...u }; setByPath(n, `/${field}`, value); return n; }),
  };
  const model = { ...(data ?? {}), ui, loaded: data !== null };
  return <Render tree={tree} data={model} actions={actions} dark={dark} />;
}

createRoot(document.getElementById("root")).render(<StrictMode><App /></StrictMode>);
