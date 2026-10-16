// Renders a Kumo UI tree (`@cloudflare/kumo/catalog` UITree) with stock Kumo components.
// Props bound with { path } are resolved against the data model with Kumo's catalog helpers, and
// `visible` conditions use Kumo's evaluator. One addition: `repeat: { path }` renders an
// element once per item of that array, with the item at `/item` (table rows).
import { Badge } from "@cloudflare/kumo/components/badge";
import { Banner } from "@cloudflare/kumo/components/banner";
import { Chart } from "@cloudflare/kumo/components/chart";
import { Grid } from "@cloudflare/kumo/components/grid";
import { LayerCard } from "@cloudflare/kumo/components/layer-card";
import { Loader } from "@cloudflare/kumo/components/loader";
import { Surface } from "@cloudflare/kumo/components/surface";
import { Table } from "@cloudflare/kumo/components/table";
import { Tabs } from "@cloudflare/kumo/components/tabs";
import { Text } from "@cloudflare/kumo/components/text";
import { createVisibilityContext, evaluateVisibility, getByPath, isDynamicPath } from "@cloudflare/kumo/catalog";
import * as echarts from "echarts/core";
import { BarChart } from "echarts/charts";
import { GridComponent, TooltipComponent } from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";

// Only what dashboard.json's charts use.
echarts.use([BarChart, GridComponent, TooltipComponent, CanvasRenderer]);

// The Kumo components a config may use ("Table.Row" resolves to Table.Row).
const Kumo = { Badge, Banner, Chart, Grid, LayerCard, Loader, Surface, Table, Tabs, Text };
const component = (type) => type.split(".").reduce((c, k) => c?.[k], Kumo);

// Every { path } anywhere in the props (including inside arrays, e.g. ECharts series).
function resolveDeep(v, data) {
  if (isDynamicPath(v)) return getByPath(data, v.path);
  if (Array.isArray(v)) return v.map((x) => resolveDeep(x, data));
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, resolveDeep(x, data)]));
  return v;
}

const show = (v) => (v === null || v === undefined || v === "" ? "–" : typeof v === "object" ? JSON.stringify(v) : String(v));

export function Render({ tree, data, actions, dark, at = tree.root, keyPrefix = "" }) {
  const el = tree.elements[at];
  if (!el) return null;
  if (el.repeat) {
    const items = getByPath(data, el.repeat.path) ?? [];
    return items.map((item, i) => (
      <Element key={`${keyPrefix}${at}-${i}`} el={el} tree={tree} data={{ ...data, item }} actions={actions} dark={dark} />
    ));
  }
  return <Element key={keyPrefix + at} el={el} tree={tree} data={data} actions={actions} dark={dark} />;
}

function Element({ el, tree, data, actions, dark }) {
  if (el.visible !== undefined && !evaluateVisibility(el.visible, createVisibilityContext(data))) return null;
  const C = component(el.type);
  if (!C) return <Kumo.Text variant="error">unknown component {el.type}</Kumo.Text>;
  const { children: text, ...props } = resolveDeep(el.props ?? {}, data);
  if (el.type === "Chart") Object.assign(props, { echarts, isDarkMode: dark });
  if (el.action) {
    const run = (value) => actions[el.action.name]?.({ ...resolveDeep(el.action.params ?? {}, data), value });
    props.onValueChange = run;
  }
  const kids = el.children?.length
    ? el.children.map((k) => <Render key={k} at={k} tree={tree} data={data} actions={actions} dark={dark} keyPrefix={`${el.key}/`} />)
    : text === undefined ? undefined : show(text);
  return <C {...props}>{kids}</C>;
}
