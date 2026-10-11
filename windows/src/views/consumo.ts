// Meu Claude in the island: token use and its API-equivalent US$ per Claude
// Code session, today. A header pill ("US$ 12,40 hoje") opens a card in place
// of the overview's left card, like the plan pills: today's sessions by cost,
// and a click on one shows where its tokens went. The numbers come from
// src-tauri/src/usage.rs, which reads the transcripts; a `usage` event keeps
// the open card live while hooks are installed, and the card refreshes on open
// and every 10 s otherwise.

import { Bridge, onEvent } from "../core/bridge";
import { State } from "../core/state";
import { clear, dot, h } from "./dom";

export interface Totals {
  calls: number;
  input: number;
  cache_read: number;
  cache_write: number;
  cache_write_1h: number;
  output: number;
  last: number;
  peak: number;
  compactions: number;
  model: string;
  usd: number;
  unpriced: number;
  tools: Record<string, { calls: number; tokens: number }>;
  title: string;
  started: string;
  updated: string;
}

export interface UsageEntry {
  cwd: string;
  totals: Totals;
  subagent_count: number;
  subagents: Totals;
}

const COLOR = "#d97757";
const REFRESH_MS = 10_000;

let sessions: Record<string, UsageEntry> = {};
let open = false;
let selected: string | null = null;
let timer: number | null = null;

const midnight = () => new Date().setHours(0, 0, 0, 0);
const cost = (e: UsageEntry) => e.totals.usd + e.subagents.usd;
const usd = (n: number) => n.toLocaleString("pt-BR", { style: "currency", currency: "USD" }).replace("US$", "US$ ");
const tokens = (n: number) =>
  n >= 1e9 ? `${(n / 1e9).toFixed(2)} bi` : n >= 1e6 ? `${(n / 1e6).toFixed(1)} mi` : n >= 1e3 ? `${Math.round(n / 1e3)} mil` : `${n}`;

/** Today's sessions, the most expensive first. */
function today(): [string, UsageEntry][] {
  const since = midnight();
  return Object.entries(sessions)
    .filter(([, e]) => Date.parse(e.totals.updated) >= since)
    .sort((a, b) => cost(b[1]) - cost(a[1]));
}

async function refresh() {
  const got = await Bridge.usageSessions(midnight());
  if (got) {
    sessions = got;
    State.notify();
  }
}

/** Live totals from the hooks; called once at start-up. */
export async function startConsumo() {
  await onEvent<UsageEntry & { session_id: string }>("usage", ({ session_id, ...entry }) => {
    sessions[session_id] = entry;
    if (open && State.mode === "expanded") State.notify();
  });
  void refresh();
}

export const consumoCardOpen = () => open && !State.showingPlanDetail && State.view === "overview";

export function closeConsumo() {
  open = false;
  selected = null;
  if (timer != null) window.clearInterval(timer);
  timer = null;
}

function toggle() {
  if (consumoCardOpen()) return closeConsumo(), State.notify();
  open = true;
  State.showingPlanDetail = false;
  void refresh();
  timer = window.setInterval(() => (open ? void refresh() : closeConsumo()), REFRESH_MS);
  State.notify();
}

// ── Pill ──────────────────────────────────────────────────────────────────────

export function buildConsumoPill() {
  const label = h("span", { class: "plan-pill-label" });
  const el = h("button", { class: "plan-pill", title: "Consumo de hoje (equivalente API)", onclick: toggle },
    h("i", { class: "plan-pill-dot", style: `background:${COLOR}` }), label);
  el.style.setProperty("--plan", COLOR);
  return {
    el,
    sync() {
      el.style.display = State.view === "overview" ? "" : "none";
      el.classList.toggle("active", consumoCardOpen());
      label.textContent = `${usd(today().reduce((s, [, e]) => s + cost(e), 0))} hoje`;
    },
  };
}

// ── Card ──────────────────────────────────────────────────────────────────────

function row(label: string, value: string): HTMLElement {
  return h("div", { class: "plan-row" }, h("span", { class: "plan-label consumo-label", text: label }), h("span", { class: "plan-credits", text: value }));
}

function list(rows: [string, UsageEntry][]): HTMLElement[] {
  if (!rows.length) return [h("div", { class: "plan-row plan-none", text: "Nenhuma sessão hoje." })];
  return rows.map(([id, e]) =>
    h("button", { class: "plan-row consumo-session", onclick: () => ((selected = id), State.notify()) },
      h("span", { class: "plan-credits consumo-usd", text: usd(cost(e)) }),
      h("span", { class: "consumo-title", text: e.totals.title || "(sem título)" }),
    ),
  );
}

function detail(e: UsageEntry): HTMLElement[] {
  const t = e.totals;
  const tools = Object.entries(t.tools).sort((a, b) => b[1].tokens - a[1].tokens).slice(0, 5);
  const out = [
    row("Tokens", `${tokens(t.input)} entrada · ${tokens(t.output)} saída`),
    row("Cache", `${tokens(t.cache_read)} lidos · ${tokens(t.cache_write)} escritos`),
    row("Contexto", `${tokens(t.last)} agora · pico ${tokens(t.peak)} · ${t.compactions} compactações`),
    row("Chamadas", `${t.calls} · ${t.model || "?"}${t.unpriced ? ` · ${t.unpriced} sem preço` : ""}`),
  ];
  if (e.subagent_count) {
    out.push(row("Subagentes", `${e.subagent_count} · ${tokens(e.subagents.input)} entrada · ${usd(e.subagents.usd)}`));
  }
  for (const [name, v] of tools) out.push(row(name, `${v.calls}× · ${tokens(v.tokens)} devolvidos`));
  return out;
}

/** The card that stands in for the overview's left card while the pill is open. */
export class ConsumoCard {
  readonly el = h("div", { class: "plan-card" });
  private key = "";

  sync() {
    const e = selected ? sessions[selected] : undefined;
    if (selected && !e) selected = null;
    const key = JSON.stringify([selected, selected ? e : today().map(([id, x]) => [id, cost(x), x.totals.title])]);
    if (key === this.key) return;
    this.key = key;
    clear(this.el);
    const back = e && h("button", { class: "consumo-back", text: "‹", title: "Voltar", onclick: () => ((selected = null), State.notify()) });
    const title = e ? e.totals.title || "(sem título)" : "Consumo de hoje";
    const sub = e ? usd(cost(e)) : `${today().length} sessões · equivalente API`;
    this.el.append(
      h("div", { class: "plan-head" }, back || dot(COLOR, 7), h("span", { class: "plan-title", text: title }), h("span", { class: "plan-sub", text: sub })),
      h("div", { class: "plan-rows consumo-rows" }, ...(e ? detail(e) : list(today()))),
    );
  }
}
