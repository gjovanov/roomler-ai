// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// Pure edge-interpretation helpers for the mesh graph (wave 4). Kept out
// of the component so the labelling and asymmetry rules are unit-testable
// without mounting SVG.

export interface MeshEdgeEnd {
  /** overlay node id of the REPORTER */
  node: string
  /** how the reporter reaches the other end */
  carrier: string
  /** CLI-style qualifier (`turn/udp` / `derp/tcp`); null from old agents */
  relay?: string | null
  rtt_ms?: number | null
  stalled?: boolean
}

/**
 * The label a carrier renders with — `relay:turn/udp`, matching the CLI's
 * CONN column exactly. A relayed carrier without the qualifier (an agent
 * older than the field) degrades to the bare word rather than inventing a
 * flavour; non-relay carriers never take one.
 */
export function qualifiedCarrier(carrier: string, relay?: string | null): string {
  if (relay && (carrier === 'relay' || carrier === 'derp')) return `relay:${relay}`
  return carrier
}

export interface EdgeSides {
  /** the `from` node's own report, when it reported */
  from?: MeshEdgeEnd
  /** the `to` node's own report, when it reported */
  to?: MeshEdgeEnd
  /**
   * Both ends reported and they DISAGREE on the carrier class — direct one
   * way, relayed the other. Real routing information (e.g. only one side
   * sits behind the corporate firewall), not an inconsistency to hide.
   */
  asymmetric: boolean
}

/** Resolve an edge's `ends` into per-side reports keyed by direction. */
export function edgeSides(
  ends: MeshEdgeEnd[] | undefined,
  fromId: string,
  toId: string,
): EdgeSides {
  const from = ends?.find((e) => e.node === fromId)
  const to = ends?.find((e) => e.node === toId)
  return { from, to, asymmetric: !!(from && to && from.carrier !== to.carrier) }
}

/**
 * Carrier classes participating in an edge — what the class toggles and the
 * per-class counts key on. An asymmetric edge belongs to BOTH its classes:
 * hiding "relay" must not hide the half of it that is direct.
 */
export function participatingCarriers(
  merged: string,
  ends: MeshEdgeEnd[] | undefined,
): string[] {
  const set = new Set<string>()
  if (ends?.length) {
    for (const e of ends) set.add(e.carrier)
  } else {
    set.add(merged)
  }
  return [...set]
}
