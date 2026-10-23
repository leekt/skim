"""Writes provisioning/grafana/dashboards/skim.json (the skim Grafana dashboard).

usage: python3 metrics/gen_dashboard.py

Edit panels here, or edit the dashboard in Grafana and export it over skim.json.
"""
import json
import os

DS = {"type": "prometheus", "uid": "prometheus"}
I = 'instance=~"$instance"'
panels = []
_id = [0]
_y = [0]


def nid():
    _id[0] += 1
    return _id[0]


def target(expr, legend="", instant=False, fmt=None, ref="A"):
    t = {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": ref}
    if instant:
        t.update({"instant": True, "range": False})
    if fmt:
        t["format"] = fmt
    return t


def row(title):
    panels.append({"type": "row", "title": title, "id": nid(), "collapsed": False,
                   "gridPos": {"h": 1, "w": 24, "x": 0, "y": _y[0]}, "panels": []})
    _y[0] += 1


def place(p, x, w, h):
    p["gridPos"] = {"x": x, "y": _y[0], "w": w, "h": h}
    p["id"] = nid()
    p["datasource"] = DS
    panels.append(p)


def stat(title, expr, unit="none", x=0, w=3, h=4, decimals=None, desc="", thresholds=None, color_mode="value"):
    d = {"unit": unit}
    if decimals is not None:
        d["decimals"] = decimals
    if thresholds:
        d["thresholds"] = {"mode": "absolute", "steps": thresholds}
        d["color"] = {"mode": "thresholds"}
    else:
        d["color"] = {"mode": "fixed", "fixedColor": "text"}
    place({"type": "stat", "title": title, "description": desc, "targets": [target(expr, instant=True)],
           "fieldConfig": {"defaults": d, "overrides": []},
           "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                       "colorMode": color_mode, "graphMode": "none", "textMode": "value", "justifyMode": "center"}}, x, w, h)


def ts(title, targets, unit="none", x=0, w=12, h=8, desc="", stack=False, bars=False, legend_table=False, min0=True):
    custom = {"lineWidth": 1, "fillOpacity": 10, "showPoints": "never", "spanNulls": True}
    if stack:
        custom["stacking"] = {"mode": "normal", "group": "A"}
    if bars:
        custom.update({"drawStyle": "bars", "fillOpacity": 80})
    d = {"unit": unit, "custom": custom}
    if min0:
        d["min"] = 0
    legend = {"displayMode": "table" if legend_table else "list", "placement": "right" if legend_table else "bottom", "showLegend": True}
    if legend_table:
        legend["calcs"] = ["lastNotNull", "max"]
    place({"type": "timeseries", "title": title, "description": desc, "targets": targets,
           "fieldConfig": {"defaults": d, "overrides": []},
           "options": {"legend": legend, "tooltip": {"mode": "multi", "sort": "desc"}}}, x, w, h)


def pie(title, expr, legend, x=0, w=6, h=8, desc=""):
    place({"type": "piechart", "title": title, "description": desc, "targets": [target(expr, legend, instant=True)],
           "fieldConfig": {"defaults": {"unit": "none"}, "overrides": []},
           "options": {"reduceOptions": {"calcs": ["lastNotNull"], "values": False}, "pieType": "donut",
                       "legend": {"displayMode": "table", "placement": "right", "values": ["value"], "showLegend": True},
                       "displayLabels": []}}, x, w, h)


def table(title, targets, organize, x=0, w=24, h=10, desc="", overrides=None, sort=None):
    t = [{"id": "merge", "options": {}}, {"id": "organize", "options": organize}]
    if sort:
        t.append({"id": "sortBy", "options": {"sort": [{"field": sort, "desc": True}]}})
    place({"type": "table", "title": title, "description": desc, "targets": targets, "transformations": t,
           "fieldConfig": {"defaults": {"custom": {"align": "auto", "cellOptions": {"type": "auto"}}}, "overrides": overrides or []},
           "options": {"showHeader": True, "cellHeight": "sm", "footer": {"show": False}}}, x, w, h)


def next_row(h):
    _y[0] += h


# ---- Overview --------------------------------------------------------------------------------
row("Overview")
green_amber_red = lambda a, r: [{"color": "green", "value": None}, {"color": "orange", "value": a}, {"color": "red", "value": r}]
red_green = lambda g: [{"color": "red", "value": None}, {"color": "green", "value": g}]
stat("Head block", f"skim_head_block{{{I}}}", x=0, w=4)
stat("Head age", f"skim_head_age_seconds{{{I}}}", "s", x=4, w=4, decimals=1, thresholds=green_amber_red(18, 36),
     desc="Time since the head block's slot started. Above one slot (12 s) means skim is behind.")
stat("Block arrival P50 (15m)", f'histogram_quantile(0.5, sum by (le) (rate(skim_block_arrival_seconds_bucket{{{I}}}[15m])))', "s", x=8, w=4, decimals=2,
     desc="Payload gossip arrival after slot start.")
stat("EL peers", f'skim_peers{{layer="el", {I}}}', x=12, w=4)
stat("SNAP state peers", f"skim_snap_peers{{{I}}}", x=16, w=4, thresholds=red_green(1),
     desc="EL peers that serve state skim doesn't hold. 0 means on-demand reads wait (--state-wait).")
stat("CL peers", f'skim_peers{{layer="cl", {I}}}', x=20, w=4)
next_row(4)
stat("Subscribed keys", f'skim_store_keys{{kind="subscribed", {I}}}', x=0, w=4)
stat("On-demand keys", f'skim_store_keys{{kind="on_demand", {I}}}', x=4, w=4)
stat("Addresses not fully subscribed", f"count(skim_address_subscription{{{I}}} < 2) or vector(0)", x=8, w=4,
     desc="Addresses your calls read that no subscription fully covers (see State and addresses).")
stat("RPC requests/s", f"sum(rate(skim_rpc_requests_total{{{I}}}[1m]))", "reqps", x=12, w=4, decimals=2)
stat("Memory (RSS)", f"process_resident_memory_bytes{{{I}}}", "bytes", x=16, w=4, decimals=0)
stat("Uptime", f"time() - process_start_time_seconds{{{I}}}", "s", x=20, w=4, decimals=0)
next_row(4)

# ---- Blocks ----------------------------------------------------------------------------------
row("Blocks")
ts("Block arrival after slot start", [
    target(f'histogram_quantile(0.5, sum by (le) (rate(skim_block_arrival_seconds_bucket{{{I}}}[5m])))', "P50"),
    target(f'histogram_quantile(0.9, sum by (le) (rate(skim_block_arrival_seconds_bucket{{{I}}}[5m])))', "P90", ref="B"),
    target(f'histogram_quantile(0.99, sum by (le) (rate(skim_block_arrival_seconds_bucket{{{I}}}[5m])))', "P99", ref="C"),
], "s", x=0, w=12, desc="When the execution payload reached skim over consensus gossip, relative to its slot start (5 m windows).")
place({"type": "heatmap", "title": "Block arrival distribution", "targets": [target(f'sum by (le) (increase(skim_block_arrival_seconds_bucket{{{I}}}[$__rate_interval]))', "{{le}}", fmt="heatmap")],
       "options": {"calculate": False, "yAxis": {"unit": "s"}, "color": {"scheme": "Blues", "mode": "scheme"}, "cellGap": 1},
       "fieldConfig": {"defaults": {}, "overrides": []}}, 12, 6, 8)
ts("Head block", [target(f"skim_head_block{{{I}}}", "{{instance}}")], x=18, w=6, min0=False)
next_row(8)

# ---- Peers -----------------------------------------------------------------------------------
row("Peers")
pie("EL peer clients", f'skim_peer_clients{{layer="el", {I}}}', "{{client}}", x=0, w=5)
pie("CL peer clients", f'skim_peer_clients{{layer="cl", {I}}}', "{{client}}", x=5, w=5)
ts("Peers", [target(f"skim_peers{{{I}}}", "{{layer}}"), target(f"skim_snap_peers{{{I}}}", "snap", ref="B")], x=10, w=6)
ts("SNAP latency per peer (EWMA)", [target(f"skim_snap_peer_latency_ms{{{I}}}", "{{client}} {{peer}}")], "ms", x=16, w=8, legend_table=True,
   desc="Point-lookup latency per state peer, from skim's adaptive fetch scheduler.")
next_row(8)
ts("SNAP answers per peer", [target(f"sum by (client, peer) (rate(skim_snap_peer_answers_total{{{I}}}[$__rate_interval]))", "{{client}} {{peer}}")], "reqps", x=0, w=8)
ts("SNAP errors per peer", [target(f"sum by (client, peer) (rate(skim_snap_peer_errors_total{{{I}}}[$__rate_interval]))", "{{client}} {{peer}}")], "reqps", x=8, w=8)
ts("SNAP fetch scheduler", [target(f'sum by (kind) (rate(skim_snap_fetch_total{{kind=~"requests|ok|errors|failed|hedges|root_unavailable", {I}}}[$__rate_interval]))', "{{kind}}")], "ops", x=16, w=8,
   desc="failed = a key no peer could serve in time (the request got -32002).")
next_row(8)

# ---- Peer acquisition -------------------------------------------------------------------------
row("Peer acquisition")
stat("First SNAP peer", f"skim_el_first_snap_peer_seconds{{{I}}}", "s", x=0, w=4, decimals=0, thresholds=green_amber_red(30, 120),
     desc="Until a SNAP-capable peer connects, state skim doesn't hold can't be fetched (requests wait up to --state-wait).")
stat("SNAP peers seen", f"skim_el_snap_peers_seen{{{I}}}", x=4, w=4)
stat("SNAP peers held 60 s", f"skim_el_snap_peers_held{{{I}}}", x=8, w=4, thresholds=red_green(1),
     desc="Many SNAP peers drop skim within a minute because they are full; these stayed. Held peers rank first in the saved list.")
pie("Sessions by client (SNAP?)", f'sum by (client, snap) (skim_el_sessions_total{{{I}}})', "{{client}} snap={{snap}}", x=12, w=6)
ts("Session ends by reason", [target(f"sum by (reason) (rate(skim_el_session_closes_total{{{I}}}[$__rate_interval]))", "{{reason}}")], "ops", x=18, w=6, stack=True,
   desc="TooManyPeers = the peer was full.")
next_row(8)

# ---- EL networking (reth's own metrics) -----------------------------------------------------
row("EL networking (reth)")
ts("Connections", [
    target(f"network_connected_peers{{{I}}}", "connected"),
    target(f"network_tracked_peers{{{I}}}", "tracked (known)", ref="B"),
    target(f"network_pending_outgoing_connections{{{I}}}", "dialing", ref="C"),
], x=0, w=8, desc="reth network manager: sessions, known peers, dials in progress.")
REASONS = ["too_many_peers", "useless_peer", "subprotocol_specific", "protocol_breach", "disconnect_requested", "ping_timeout",
           "tcp_subsystem_error", "already_connected", "incompatible", "client_quitting", "unexpected_identity", "null_node_identity", "connected_to_self"]
# rate() drops the metric name, so each reason is its own query.
reasons = lambda side: [target(f"sum(rate(network_{side}_{r}{{{I}}}[$__rate_interval]))", r, ref=chr(65 + k)) for k, r in enumerate(REASONS)]
ts("Disconnect reasons (outbound)", reasons("outbound"), "ops", x=8, w=8, stack=True,
   desc="Why sessions skim dialed ended. too_many_peers = the remote was full (common on Sepolia).")
ts("Disconnect reasons (inbound)", reasons("inbound"), "ops", x=16, w=8, stack=True)
next_row(8)

# ---- RPC -------------------------------------------------------------------------------------
row("JSON-RPC")
ts("Requests by method", [target(f"sum by (method) (rate(skim_rpc_requests_total{{{I}}}[$__rate_interval]))", "{{method}}")], "reqps", x=0, w=8, stack=True)
ts("Latency P95 by method", [target(f"histogram_quantile(0.95, sum by (le, method) (rate(skim_rpc_request_duration_seconds_bucket{{{I}}}[5m])))", "{{method}}")], "s", x=8, w=8, legend_table=True)
ts("Outcomes", [target(f"sum by (outcome) (rate(skim_rpc_requests_total{{{I}}}[$__rate_interval]))", "{{outcome}}")], "reqps", x=16, w=8, stack=True,
   desc="unavailable = -32002 (state or data no peer could serve); reverted = execution reverted.")
next_row(8)
table("Methods", [
    target(f"sum by (method) (increase(skim_rpc_requests_total{{{I}}}[$__range]))", instant=True, fmt="table"),
    target(f"histogram_quantile(0.5, sum by (le, method) (rate(skim_rpc_request_duration_seconds_bucket{{{I}}}[$__range])))", instant=True, fmt="table", ref="B"),
    target(f"histogram_quantile(0.95, sum by (le, method) (rate(skim_rpc_request_duration_seconds_bucket{{{I}}}[$__range])))", instant=True, fmt="table", ref="C"),
    target(f'sum by (method) (increase(skim_rpc_requests_total{{outcome!="ok", {I}}}[$__range]))', instant=True, fmt="table", ref="D"),
], {"excludeByName": {"Time": True}, "renameByName": {"method": "Method", "Value #A": "Requests", "Value #B": "P50", "Value #C": "P95", "Value #D": "Not ok"},
    "indexByName": {"method": 0, "Value #A": 1, "Value #D": 2, "Value #B": 3, "Value #C": 4}},
    w=24, h=8, sort="Requests", overrides=[
        {"matcher": {"id": "byRegexp", "options": "P50|P95"}, "properties": [{"id": "unit", "value": "s"}, {"id": "decimals", "value": 2}]},
        {"matcher": {"id": "byRegexp", "options": "Requests|Not ok"}, "properties": [{"id": "decimals", "value": 0}]},
    ])
next_row(8)

# ---- State and addresses ---------------------------------------------------------------------
row("State and addresses")
table("Addresses your calls read", [
    target(f"max by (address) (skim_address_calls_total{{{I}}})", instant=True, fmt="table"),
    target(f"max by (address) (skim_address_subscription{{{I}}})", instant=True, fmt="table", ref="B"),
    target(f'max by (address) (skim_address_slots{{subscribed="true", {I}}})', instant=True, fmt="table", ref="C"),
    target(f'max by (address) (skim_address_slots{{subscribed="false", {I}}})', instant=True, fmt="table", ref="D"),
    target(f"max by (address) (skim_address_fetched_keys_total{{{I}}})", instant=True, fmt="table", ref="E"),
    target(f"max by (address, subscription) (skim_address_info{{{I}}})", instant=True, fmt="table", ref="F"),
], {"excludeByName": {"Time": True, "Value #F": True}, "renameByName": {"address": "Address", "subscription": "Subscription", "Value #A": "Calls",
    "Value #B": "Status", "Value #C": "Slots subscribed", "Value #D": "Slots not subscribed", "Value #E": "Keys fetched"},
    "indexByName": {"address": 0, "subscription": 1, "Value #B": 2, "Value #A": 3, "Value #C": 4, "Value #D": 5, "Value #E": 6}},
    w=24, h=10, sort="Calls",
    desc="Every address eth_call executions read (top --metrics-addresses). subscribed = kept current by a subscription; on demand = fetched from SNAP peers and cached briefly. "
         "Subscribe hot on-demand addresses in skim.yaml (skim_dashboard returns a ready-made block).",
    overrides=[
        {"matcher": {"id": "byName", "options": "Status"}, "properties": [
            {"id": "custom.cellOptions", "value": {"type": "color-text"}},
            {"id": "mappings", "value": [{"type": "value", "options": {
                "2": {"text": "subscribed", "color": "green", "index": 0},
                "1": {"text": "partly subscribed", "color": "orange", "index": 1},
                "0": {"text": "on demand", "color": "red", "index": 2}}}]}]},
        {"matcher": {"id": "byName", "options": "Address"}, "properties": [{"id": "custom.width", "value": 400}]},
        {"matcher": {"id": "byRegexp", "options": "Calls|Slots.*|Keys fetched"}, "properties": [{"id": "decimals", "value": 0}]},
    ])
next_row(10)
ts("Calls per address (top 10)", [target(f"topk(10, sum by (address) (rate(skim_address_calls_total{{{I}}}[$__rate_interval])))", "{{address}}")], "ops", x=0, w=12, legend_table=True)
ts("Keys fetched from the network per address", [target(f"topk(10, sum by (address) (rate(skim_address_fetched_keys_total{{{I}}}[$__rate_interval])))", "{{address}}")], "ops", x=12, w=12, legend_table=True,
   desc="Steady fetching means the address is read often but not subscribed.")
next_row(8)
ts("Store keys", [target(f"skim_store_keys{{{I}}}", "{{kind}}")], x=0, w=8)
ts("Store memory", [target(f"skim_store_bytes{{{I}}}", "{{kind}}")], "bytes", x=8, w=8)
table("Subscriptions", [
    target(f"max by (name) (skim_subscription_ready{{{I}}})", instant=True, fmt="table"),
    target(f"max by (name) (skim_subscription_missing_keys{{{I}}})", instant=True, fmt="table", ref="B"),
], {"excludeByName": {"Time": True}, "renameByName": {"name": "Subscription", "Value #A": "Ready", "Value #B": "Missing keys"}},
    x=16, w=8, h=8, overrides=[{"matcher": {"id": "byName", "options": "Ready"}, "properties": [
        {"id": "custom.cellOptions", "value": {"type": "color-text"}},
        {"id": "mappings", "value": [{"type": "value", "options": {"1": {"text": "ready", "color": "green", "index": 0}, "0": {"text": "not ready", "color": "red", "index": 1}}}]}]}])
next_row(8)
ts("Subscription maintenance (per block)", [
    target(f"histogram_quantile(0.5, sum by (le) (rate(skim_subscription_maintenance_seconds_bucket{{{I}}}[5m])))", "P50"),
    target(f"histogram_quantile(0.99, sum by (le) (rate(skim_subscription_maintenance_seconds_bucket{{{I}}}[5m])))", "P99", ref="B"),
], "s", x=0, w=12, desc="One subscription pass after each applied block: declared keys, calls, readiness. P99 should stay flat as subscribed and archived keys grow.")
ts("Archive base writes", [target(f"sum(increase(skim_store_archive_base_writes_total{{{I}}}[1m]))", "per minute")], x=12, w=12, bars=True,
   desc="One redb commit each. Expected near 0 once archived keys have their base: re-subscribing a based key writes nothing.")
next_row(8)

# ---- Serving ---------------------------------------------------------------------------------
row("Serving the network")
ts("Requests answered for peers", [target(f"sum by (layer, kind) (rate(skim_served_requests_total{{{I}}}[$__rate_interval]))", "{{layer}} {{kind}}")], "reqps", x=0, w=12, stack=True)
ts("Items sent to peers", [target(f"sum by (layer, kind) (rate(skim_served_items_total{{{I}}}[$__rate_interval]))", "{{layer}} {{kind}}")], "ops", x=12, w=12, stack=True)
next_row(8)

# ---- Process ---------------------------------------------------------------------------------
row("Process")
ts("CPU (cores)", [target(f"rate(process_cpu_seconds_total{{{I}}}[$__rate_interval])", "{{instance}}")], x=0, w=8)
ts("Memory (RSS)", [target(f"process_resident_memory_bytes{{{I}}}", "{{instance}}")], "bytes", x=8, w=8)
ts("Open file descriptors / threads", [target(f"process_open_fds{{{I}}}", "fds"), target(f"process_threads{{{I}}}", "threads", ref="B")], x=16, w=8)
next_row(8)
ts("Retained data by structure (stacked) vs RSS", [
    target(f"sum by (part) (skim_memory_bytes{{{I}}})", "{{part}}"),
    target(f"sum(process_resident_memory_bytes{{{I}}})", "RSS", ref="B"),
    target(f"sum(skim_heap_live_bytes{{{I}}})", "heap (live)", ref="C"),
], "bytes", x=0, w=16, h=10, stack=True, legend_table=True,
   desc="Estimated payload bytes each structure holds (skim_memory_bytes), and skim's whole live heap (heap (live), counted at the allocator). Between the stack and the heap line: live data no structure counts (network buffers, page caches). Between the heap line and RSS: allocator overhead (free pages it keeps, fragmentation) plus code and stacks. A part that keeps rising after its cache is full points at a leak.")
# RSS is the total to compare against, not another layer of the stack.
next(p for p in panels if p.get("title") == "Retained data by structure (stacked) vs RSS")["fieldConfig"]["overrides"] = [{
    "matcher": {"id": "byName", "options": name},
    "properties": [{"id": "custom.stacking", "value": {"mode": "none"}}, {"id": "custom.fillOpacity", "value": 0}, {"id": "custom.lineWidth", "value": 2}],
} for name in ("RSS", "heap (live)")]
ts("Entries by structure", [target(f"sum by (part) (skim_memory_entries{{{I}}})", "{{part}}")], x=16, w=8, h=10, legend_table=True,
   desc="Blocks, keys or peers each structure holds. Fixed-size caches level off once full (tx index after 2,048 blocks, headers after 8,192).")
next_row(10)

dashboard = {
    "uid": "skim",
    "title": "skim",
    "description": "skim: peers, latency, RPC, the state your calls read, and what skim serves back.",
    "tags": ["skim", "ethereum"],
    "timezone": "browser",
    "editable": True,
    "graphTooltip": 1,
    "refresh": "5s",
    "schemaVersion": 41,
    "time": {"from": "now-1h", "to": "now"},
    "templating": {"list": [{
        "name": "instance", "label": "Instance", "type": "query", "datasource": DS,
        "query": {"query": "label_values(skim_head_block, instance)", "refId": "q"},
        "definition": "label_values(skim_head_block, instance)",
        "refresh": 2, "includeAll": True, "multi": True, "current": {"text": "All", "value": "$__all"}, "allValue": ".*",
    }]},
    "panels": panels,
}
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "provisioning/grafana/dashboards/skim.json")
with open(out, "w") as f:
    json.dump(dashboard, f, indent=2)
print(f"wrote {out}: {len(panels)} panels")
