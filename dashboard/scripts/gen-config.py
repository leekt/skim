import json, sys
E = {}
n = [0]
def el(type, props=None, children=None, **kw):
    n[0] += 1
    k = f"{type.lower().replace('.', '-')}-{n[0]}"
    e = {"key": k, "type": type, "props": props or {}}
    if children: e["children"] = [c for c in children]
    e.update(kw)
    E[k] = e
    for c in children or []:
        E[c]["parentKey"] = k
    return k
P = lambda p: {"path": p}
def text(t, variant=None, visible=None, **kw):
    pr = {"children": t}
    if variant: pr["variant"] = variant
    pr.update(kw)
    return el("Text", pr, **({"visible": visible} if visible is not None else {}))
def tab_is(t): return {"eq": [P("/ui/tab"), t]}

def stat(label, path, unit=None):
    kids = [text(P(path), "heading", size="lg")]
    if unit: kids.append(text(unit, "secondary", size="sm"))
    return el("LayerCard", {}, [el("LayerCard.Secondary", {"children": label}), el("LayerCard.Primary", {}, kids)])

def badge_switch(path, cases):
    return [el("Badge", {"variant": v, "children": label}, visible={"eq": [P(path), val]}) for val, label, v in cases]

def table(title, rows_path, cols, empty="nothing yet"):
    # cols: (header, cell) where cell is a path under /item, or a list of element keys
    head = el("Table.Header", {}, [el("Table.Row", {}, [el("Table.Head", {"children": h}) for h, _ in cols])])
    cells = []
    for h, c in cols:
        if isinstance(c, list):
            cells.append(el("Table.Cell", {}, c))
        else:
            cells.append(el("Table.Cell", {"children": P(c)}))
    row = el("Table.Row", {}, cells, repeat=P(rows_path))
    body = el("Table.Body", {}, [row])
    tbl = el("Table", {}, [head, body])
    none = text(empty, "secondary", visible={"not": {"path": rows_path + "/0"}})
    # Wide tables scroll inside their card instead of widening the page.
    return el("LayerCard", {"className": "min-w-0"}, [el("LayerCard.Secondary", {"children": title}), el("LayerCard.Primary", {"className": "overflow-x-auto"}, [tbl, none])])

# Header
header = el("Surface", {}, [
    text("skim", "heading", size="lg", **{"as": "h1"}),
    text("Peers, latency and the state your requests read. Refreshes every 2 s.", "secondary"),
])
err = el("Banner", {"variant": "error", "title": "Disconnected", "description": P("/ui/error")}, visible={"path": "/ui/error"})
loading = el("Loader", {"size": 24}, visible={"not": {"path": "/loaded"}})
tabs = el("Tabs", {"variant": "underline", "className": "overflow-x-auto", "value": P("/ui/tab"), "tabs": [
    {"value": "overview", "label": "Overview"},
    {"value": "peers", "label": "Peers"},
    {"value": "addresses", "label": "Addresses"},
    {"value": "rpc", "label": "RPC"},
    {"value": "serving", "label": "Serving"},
]}, action={"name": "set_ui", "params": {"field": "tab"}})

# Overview
stats = el("Grid", {"variant": "6up", "gap": "sm"}, [
    stat("Head block", "/summary/head"),
    stat("Head age", "/summary/head_age_s", "seconds since its slot started"),
    stat("Block arrival P50", "/summary/arrival_p50_ms", "ms after slot start"),
    stat("Block arrival P90", "/summary/arrival_p90_ms", "ms after slot start"),
    stat("EL peers", "/summary/el_peers"),
    stat("SNAP state peers", "/summary/snap_peers", "serve state skim doesn't hold"),
    stat("CL peers", "/summary/cl_peers"),
    stat("Addresses not fully pinned", "/summary/addresses_not_pinned", "read by requests, fetched on demand"),
    stat("Subscribed keys", "/summary/subscribed_keys"),
    stat("On-demand keys", "/summary/on_demand_keys"),
    stat("Addresses used", "/summary/addresses_used"),
    stat("Uptime", "/summary/uptime_min", "minutes"),
])
chart = el("LayerCard", {}, [
    el("LayerCard.Secondary", {"children": "Block arrival after slot start (ms)"}),
    el("LayerCard.Primary", {}, [el("Chart", {"height": 260, "options": {
        "animation": False,
        "tooltip": {"trigger": "axis"},
        "grid": {"left": 48, "right": 16, "top": 16, "bottom": 32},
        "xAxis": {"type": "category", "data": P("/blocks/numbers")},
        "yAxis": {"type": "value", "name": "ms"},
        "series": [{"type": "bar", "name": "arrival", "data": P("/blocks/arrival_ms")}],
    }})]),
])
overview = el("Grid", {"gap": "base", "className": "grid-cols-1"}, [stats, chart], visible=tab_is("overview"))

# Peers
snap_badges = badge_switch("/item/snap", [("snap/2", "snap/2", "success"), ("snap/1", "snap/1", "info"), ("none", "no SNAP", "neutral")])
el_clients = table("Execution-layer clients", "/el/clients", [("Client", "/item/client"), ("Peers", "/item/count")])
cl_clients = table("Consensus-layer clients", "/cl/clients", [("Client", "/item/client"), ("Peers", "/item/count")])
el_peers = table("Execution-layer peers (SNAP latency from skim's fetch scheduler)", "/el/peers", [
    ("Peer", "/item/id"), ("Client", "/item/client"), ("Version", "/item/version"), ("Protocol", "/item/eth"),
    ("State", snap_badges), ("Connected (min)", "/item/age_min"), ("SNAP answers", "/item/snap_ok"),
    ("Errors", "/item/snap_errors"), ("Latency EWMA (ms)", "/item/ewma_ms"), ("Best (ms)", "/item/min_ms"), ("In flight", "/item/inflight"),
], "no EL peers connected yet")
dir_badges = badge_switch("/item/direction", [("outbound", "outbound", "secondary"), ("inbound", "inbound", "outline")])
cl_peers = table("Consensus-layer peers (sorted by gossip delivered first)", "/cl/peers", [
    ("Peer", "/item/id"), ("Client", "/item/client"), ("Version", "/item/version"), ("Direction", dir_badges),
    ("Connected (min)", "/item/age_min"), ("Gossip first", "/item/gossip_first"), ("Head slot", "/item/head_slot"),
], "no CL peers connected yet")
peers = el("Grid", {"gap": "base", "className": "grid-cols-1"}, [
    el("Grid", {"variant": "2up", "gap": "base"}, [el_clients, cl_clients]), el_peers, cl_peers,
], visible=tab_is("peers"))

# Addresses
status_badges = badge_switch("/item/status", [("pinned", "pinned", "success"), ("partly pinned", "partly pinned", "warning"), ("on demand", "on demand", "neutral")])
addr_note = el("Banner", {"variant": "default", "title": "What requests read",
    "description": "Every address an eth_call execution read (cache hits included). Pinned = kept current by a subscription. On demand = fetched from SNAP peers and cached for a few blocks; pin hot ones to answer without the network."})
addrs = table("Addresses by calls", "/addresses", [
    ("Address", [text(P("/item/address"), "mono", size="sm")]), ("Subscription", "/item/label"), ("Status", status_badges),
    ("Calls", "/item/calls"), ("Slots read", "/item/slots"), ("Not pinned", "/item/unpinned_slots"),
    ("Keys fetched", "/item/fetched"), ("Last block", "/item/last_block"),
], "no eth_call yet")
suggest = el("LayerCard", {"className": "min-w-0"}, [
    el("LayerCard.Secondary", {"children": "Suggested skim.yaml subscriptions (most-used addresses not fully pinned)"}),
    el("LayerCard.Primary", {"className": "overflow-x-auto"}, [text(P("/suggestion"), "mono", size="sm", **{"as": "pre"})]),
], visible={"path": "/suggestion"})
addresses = el("Grid", {"gap": "base", "className": "grid-cols-1"}, [addr_note, addrs, suggest], visible=tab_is("addresses"))

# RPC
rpc = el("Grid", {"gap": "base", "className": "grid-cols-1"}, [table("JSON-RPC methods (latency over the last 512 calls)", "/rpc", [
    ("Method", [text(P("/item/method"), "mono", size="sm")]), ("Calls", "/item/count"), ("Errors", "/item/errors"),
    ("P50 (ms)", "/item/p50_ms"), ("P95 (ms)", "/item/p95_ms"), ("Max (ms)", "/item/max_ms"),
], "no requests yet")], visible=tab_is("rpc"))

# Serving
def srow(label, req, items):
    return el("Table.Row", {}, [el("Table.Cell", {"children": label}), el("Table.Cell", {"children": P(req)}), el("Table.Cell", {"children": P(items) if items else "–"})])
serve_tbl = el("Table", {}, [
    el("Table.Header", {}, [el("Table.Row", {}, [el("Table.Head", {"children": h}) for h in ("Request", "Requests answered", "Items sent")])]),
    el("Table.Body", {}, [
        srow("EL headers", "/served/el/headers/requests", "/served/el/headers/items"),
        srow("EL bodies", "/served/el/bodies/requests", "/served/el/bodies/items"),
        srow("EL receipts", "/served/el/receipts/requests", "/served/el/receipts/items"),
        srow("EL block access lists", "/served/el/bals/requests", "/served/el/bals/items"),
        srow("snap (bytecodes)", "/served/el/snap/requests", "/served/el/snap/bytecodes"),
        srow("CL envelopes by range", "/served/cl/envelopes_by_range/requests", None),
        srow("CL envelopes by root", "/served/cl/envelopes_by_root/requests", None),
        srow("CL envelopes sent", "/served/cl/envelopes", None),
    ]),
])
serving = el("Grid", {"gap": "base", "className": "grid-cols-1"}, [
    el("Grid", {"variant": "2up", "gap": "sm"}, [stat("EL range held from block", "/served/el/served_from"), stat("CL envelopes held from slot", "/served/cl/served_from_slot")]),
    el("LayerCard", {"className": "min-w-0"}, [el("LayerCard.Secondary", {"children": "What skim answered for other peers"}), el("LayerCard.Primary", {"className": "overflow-x-auto"}, [serve_tbl])]),
], visible=tab_is("serving"))

root = el("Surface", {"className": "p-6"}, [
    el("Grid", {"gap": "base", "className": "grid-cols-1"}, [header, err, loading, tabs, overview, peers, addresses, rpc, serving]),
])
E[root].pop("parentKey", None)
json.dump({"root": root, "elements": E}, open(sys.argv[1], "w"), indent=1)
print(len(E), "elements")
