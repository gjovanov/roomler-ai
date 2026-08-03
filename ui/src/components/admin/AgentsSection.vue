<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Devices</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-update"
        variant="tonal"
        size="small"
        class="mr-2 d-none d-sm-inline-flex"
        :disabled="agentStore.agents.length === 0"
        @click="updateAllDialogOpen = true"
      >
        Update all
      </v-btn>
      <v-btn
        prepend-icon="mdi-key-plus"
        color="primary"
        variant="flat"
        size="small"
        @click="openEnrollDialog"
      >
        Enroll device
      </v-btn>
    </v-card-title>

    <v-card-text>
      <v-alert
        v-if="agentStore.error"
        type="error"
        variant="tonal"
        closable
        @click:close="agentStore.error = null"
        class="mb-4"
      >
        {{ agentStore.error }}
      </v-alert>

      <!-- S1a — forced-update feedback (delivered / offline counts). -->
      <v-alert
        v-if="updateNotice"
        type="info"
        variant="tonal"
        closable
        @click:close="updateNotice = null"
        class="mb-4"
      >
        {{ updateNotice }}
      </v-alert>

      <div
        v-if="agentStore.loading && agentStore.agents.length === 0"
        class="d-flex justify-center pa-8"
      >
        <v-progress-circular indeterminate />
      </div>

      <!-- Desktop / tablet (≥ sm): full table. Action column is leftmost so
           it never falls off the right edge regardless of overflow; codec
           chips collapse to "+N" on lgAndDown to reclaim ~200px on mid-width
           viewports (was the field bug from the field-test host 2026-05-01: "cannot
           select the last Laptop in the list, not possible to scroll").
           Below sm we render the dedicated card list further down. -->
      <v-table
        v-else-if="agentStore.agents.length > 0 && !mobile"
        density="compact"
        class="agents-table"
      >
        <thead>
          <tr>
            <th class="agents-actions-col">Actions</th>
            <th>Name</th>
            <th>Status</th>
            <th>OS</th>
            <th>Overlay</th>
            <th>Consent</th>
            <th>Codecs</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="a in agentStore.agents" :key="a.id">
            <td class="agents-actions-col">
              <v-btn
                icon="mdi-remote-desktop"
                size="small"
                variant="text"
                color="primary"
                :disabled="!a.is_online"
                :to="{ name: 'agent-remote', params: { tenantId, agentId: a.id } }"
                :aria-label="`Connect to agent ${a.name}`"
              />
              <!-- Everything but Connect lives in ONE grouped menu — the
                   seven bare icons were unscannable and the mobile cards
                   had silently drifted (they dropped Reassign owner). -->
              <v-menu>
                <template #activator="{ props: menuProps }">
                  <v-btn
                    v-bind="menuProps"
                    icon="mdi-dots-vertical"
                    size="small"
                    variant="text"
                    :aria-label="`Actions for ${a.name}`"
                  />
                </template>
                <v-list density="compact" min-width="230">
                  <v-list-subheader>Maintenance</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-update"
                    title="Update now"
                    :disabled="updateBusy === a.id"
                    @click="triggerUpdate(a)"
                  />
                  <v-list-subheader>Diagnostics</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-alert-circle-outline"
                    title="Crash reports"
                    @click="openCrashes(a)"
                  />
                  <v-list-item
                    prepend-icon="mdi-text-box-search-outline"
                    title="Agent logs"
                    @click="openLogs(a)"
                  />
                  <v-list-subheader>Access</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-account-switch"
                    title="Reassign owner"
                    @click="openReassign(a)"
                  />
                  <v-list-subheader>Network</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-ip-network-outline"
                    title="Tunnel mesh routes"
                    @click="openRoutes(a)"
                  />
                  <v-list-item
                    v-if="nodeForAgent(a.id)"
                    prepend-icon="mdi-lan-disconnect"
                    :title="nodeForAgent(a.id)?.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
                    @click="confirmEvict(nodeForAgent(a.id)!)"
                  />
                  <v-divider />
                  <v-list-item
                    prepend-icon="mdi-delete"
                    title="Delete device"
                    base-color="error"
                    @click="confirmDelete(a)"
                  />
                </v-list>
              </v-menu>
            </td>
            <td>
              <div class="d-flex align-center">
                <v-icon
                  :color="presenceColor(a)"
                  size="small"
                  class="mr-2"
                  :title="presenceTitle(a)"
                >
                  {{ presenceIcon(a) }}
                </v-icon>
                <span class="font-weight-medium">{{ a.name }}</span>
                <v-btn
                  :icon="copiedAgentId === a.id ? 'mdi-check' : 'mdi-content-copy'"
                  size="x-small"
                  variant="text"
                  :color="copiedAgentId === a.id ? 'success' : undefined"
                  class="ml-1"
                  @click="copyAgentId(a.id)"
                  :aria-label="`Copy agent ID for ${a.name}`"
                  :title="copiedAgentId === a.id
                    ? 'Copied!'
                    : `Copy agent ID — use with \`roomler-tunnel forward --agent <id>\``"
                />
              </div>
              <div class="text-caption text-medium-emphasis d-flex align-center flex-wrap">
                <span class="agent-id-preview" :title="`Agent ID: ${a.id}`">
                  id: {{ shortId(a.id) }}
                </span>
                <span class="mx-1">·</span>
                <span :title="`machine_id: ${a.machine_id}`">{{ shortId(a.machine_id) }}</span>
                <span v-if="a.agent_version"> · v{{ a.agent_version }}</span>
              </div>
            </td>
            <td>
              <v-chip
                size="small"
                :color="statusColor(a)"
                variant="flat"
                :title="presenceTitle(a)"
              >
                {{ statusLabel(a) }}
              </v-chip>
            </td>
            <td>
              <v-chip size="x-small" :prepend-icon="osIcon(a.os)" variant="tonal">
                {{ a.os }}
              </v-chip>
            </td>
            <td>
              <template v-if="nodeForAgent(a.id)">
                <div class="text-caption font-mono">{{ nodeForAgent(a.id)!.overlay_ip }}</div>
                <div
                  class="text-caption text-medium-emphasis font-mono"
                  :title="deriveOverlayV6(nodeForAgent(a.id)!.overlay_ip)"
                >
                  {{ deriveOverlayV6(nodeForAgent(a.id)!.overlay_ip) }}
                </div>
              </template>
              <span v-else class="text-caption text-medium-emphasis">—</span>
            </td>
            <td>
              <v-select
                :model-value="a.access_policy.consent_mode ?? 'prompt'"
                :items="CONSENT_MODE_ITEMS"
                density="compact"
                variant="plain"
                hide-details
                :disabled="consentBusy === a.id"
                :loading="consentBusy === a.id"
                class="consent-select"
                :aria-label="`Consent mode for ${a.name}`"
                @update:model-value="(m) => onConsentModeChange(a, m as ConsentMode)"
              />
            </td>
            <td>
              <div v-if="codecChips(a).length === 0" class="text-caption text-medium-emphasis">—</div>
              <div v-else-if="lgAndDown" class="d-flex flex-wrap gap-1 align-center">
                <v-chip
                  size="x-small"
                  :color="codecChips(a)[0].color"
                  variant="tonal"
                  :title="codecChips(a).map(c => c.tooltip).join(', ')"
                >
                  {{ codecChips(a)[0].label }}
                </v-chip>
                <v-chip
                  v-if="codecChips(a).length > 1"
                  size="x-small"
                  variant="tonal"
                  :title="codecChips(a).slice(1).map(c => c.tooltip).join(', ')"
                >
                  +{{ codecChips(a).length - 1 }}
                </v-chip>
              </div>
              <div v-else class="d-flex flex-wrap gap-1">
                <v-chip
                  v-for="codec in codecChips(a)"
                  :key="codec.label"
                  size="x-small"
                  :color="codec.color"
                  variant="tonal"
                  :title="codec.tooltip"
                >
                  {{ codec.label }}
                </v-chip>
              </div>
            </td>
            <td class="text-caption" :title="fmtDate(a.last_seen_at)">{{ fmtRelative(a.last_seen_at) }}</td>
          </tr>
        </tbody>
      </v-table>

      <!-- Mobile: stacked card list. Each card is a tappable target;
           Connect / Delete actions are full-width buttons at the bottom
           of the card so the rightmost item is reachable on a narrow
           viewport (the field bug from the field-test host 2026-05-01: "cannot
           select the last Laptop in the list, not possible to scroll").
           Codecs / version / last-seen drop to small lines so the
           card stays compact at ~120px tall. -->
      <v-list
        v-else-if="agentStore.agents.length > 0 && mobile"
        density="compact"
        class="pa-0"
      >
        <v-card
          v-for="a in agentStore.agents"
          :key="a.id"
          variant="outlined"
          class="mb-2"
        >
          <v-card-text class="pa-3">
            <div class="d-flex align-center mb-1">
              <v-icon
                :color="presenceColor(a)"
                size="small"
                class="mr-2"
                :title="presenceTitle(a)"
              >
                {{ presenceIcon(a) }}
              </v-icon>
              <span class="font-weight-medium">{{ a.name }}</span>
              <v-btn
                :icon="copiedAgentId === a.id ? 'mdi-check' : 'mdi-content-copy'"
                size="x-small"
                variant="text"
                :color="copiedAgentId === a.id ? 'success' : undefined"
                class="ml-1"
                @click="copyAgentId(a.id)"
                :aria-label="`Copy agent ID for ${a.name}`"
                :title="copiedAgentId === a.id ? 'Copied!' : 'Copy agent ID'"
              />
              <v-btn
                icon="mdi-update"
                size="x-small"
                variant="text"
                color="primary"
                class="ml-1"
                :disabled="updateBusy === a.id"
                :loading="updateBusy === a.id"
                @click="triggerUpdate(a)"
                :aria-label="`Update agent ${a.name} now`"
                title="Update agent now"
              />
              <v-spacer />
              <v-chip
                size="x-small"
                :color="statusColor(a)"
                variant="flat"
                :title="presenceTitle(a)"
              >
                {{ statusLabel(a) }}
              </v-chip>
            </div>
            <div class="text-caption text-medium-emphasis mb-2">
              <span :title="`Agent ID: ${a.id}`">id: {{ shortId(a.id) }}</span>
              <span class="mx-1">·</span>
              <span :title="`machine_id: ${a.machine_id}`">{{ shortId(a.machine_id) }}</span>
            </div>
            <div class="d-flex flex-wrap gap-1 mb-2">
              <v-chip
                size="x-small"
                :prepend-icon="osIcon(a.os)"
                variant="tonal"
              >
                {{ a.os }}
              </v-chip>
              <v-chip
                v-if="a.agent_version"
                size="x-small"
                variant="tonal"
              >
                v{{ a.agent_version }}
              </v-chip>
              <v-chip
                v-for="codec in codecChips(a)"
                :key="codec.label"
                size="x-small"
                :color="codec.color"
                variant="tonal"
                :title="codec.tooltip"
              >
                {{ codec.label }}
              </v-chip>
            </div>
            <div class="text-caption text-medium-emphasis mb-2">
              Last seen: {{ fmtDate(a.last_seen_at) }}
            </div>
            <v-select
              :model-value="a.access_policy.consent_mode ?? 'prompt'"
              :items="CONSENT_MODE_ITEMS"
              label="Consent"
              density="compact"
              variant="outlined"
              hide-details
              :disabled="consentBusy === a.id"
              :loading="consentBusy === a.id"
              class="mb-2"
              :aria-label="`Consent mode for ${a.name}`"
              @update:model-value="(m) => onConsentModeChange(a, m as ConsentMode)"
            />
            <div class="d-flex gap-2">
              <v-btn
                size="small"
                variant="tonal"
                color="primary"
                prepend-icon="mdi-remote-desktop"
                :disabled="!a.is_online"
                :to="{ name: 'agent-remote', params: { tenantId, agentId: a.id } }"
                :aria-label="`Connect to agent ${a.name}`"
                class="flex-grow-1"
              >
                Connect
              </v-btn>
              <!-- Same grouped menu as the desktop table — the mobile card
                   used to carry its own DIVERGED subset (no Reassign owner). -->
              <v-menu>
                <template #activator="{ props: menuProps }">
                  <v-btn
                    v-bind="menuProps"
                    icon="mdi-dots-vertical"
                    size="small"
                    variant="text"
                    :aria-label="`Actions for ${a.name}`"
                  />
                </template>
                <v-list density="compact" min-width="230">
                  <v-list-subheader>Maintenance</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-update"
                    title="Update now"
                    :disabled="updateBusy === a.id"
                    @click="triggerUpdate(a)"
                  />
                  <v-list-subheader>Diagnostics</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-alert-circle-outline"
                    title="Crash reports"
                    @click="openCrashes(a)"
                  />
                  <v-list-item
                    prepend-icon="mdi-text-box-search-outline"
                    title="Agent logs"
                    @click="openLogs(a)"
                  />
                  <v-list-subheader>Access</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-account-switch"
                    title="Reassign owner"
                    @click="openReassign(a)"
                  />
                  <v-list-subheader>Network</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-ip-network-outline"
                    title="Tunnel mesh routes"
                    @click="openRoutes(a)"
                  />
                  <v-list-item
                    v-if="nodeForAgent(a.id)"
                    prepend-icon="mdi-lan-disconnect"
                    :title="nodeForAgent(a.id)?.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
                    @click="confirmEvict(nodeForAgent(a.id)!)"
                  />
                  <v-divider />
                  <v-list-item
                    prepend-icon="mdi-delete"
                    title="Delete device"
                    base-color="error"
                    @click="confirmDelete(a)"
                  />
                </v-list>
              </v-menu>
            </div>
          </v-card-text>
        </v-card>
      </v-list>

      <div v-else class="text-center pa-4 pa-md-6 pa-lg-8 text-medium-emphasis">
        <v-icon size="64" color="grey-lighten-1" class="mb-2">mdi-desktop-classic</v-icon>
        <p class="mb-2">No devices enrolled yet.</p>
        <p class="text-body-2">
          Click "Enroll device" for a one-line installer per platform — the
          machine appears here as soon as it enrolls.
        </p>
      </div>
    </v-card-text>
  </v-card>

  <!-- Tunnel clients (2026-08-04): CLI-only endpoints folded into the
       unified Devices page — /network/tunnel-clients redirects here. Same
       overlay/last-seen columns; no remote-desktop/consent/codec cells
       (those are agent-only capabilities). -->
  <v-card class="mt-4">
    <v-card-title class="d-flex align-center">
      <span>Tunnel clients</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-key-plus"
        variant="tonal"
        size="small"
        @click="openTunnelEnrollDialog"
      >
        Enroll tunnel client
      </v-btn>
    </v-card-title>
    <v-card-text>
      <v-table v-if="tunnelClientStore.clients.length > 0" density="compact">
        <thead>
          <tr>
            <th class="agents-actions-col">Actions</th>
            <th>Name</th>
            <th>Status</th>
            <th>OS</th>
            <th>Overlay</th>
            <th>Version</th>
            <th>Last seen</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="c in tunnelClientStore.clients" :key="c.id">
            <td class="agents-actions-col">
              <v-menu>
                <template #activator="{ props: menuProps }">
                  <v-btn
                    v-bind="menuProps"
                    icon="mdi-dots-vertical"
                    size="small"
                    variant="text"
                    :aria-label="`Actions for ${c.name}`"
                  />
                </template>
                <v-list density="compact" min-width="230">
                  <v-list-subheader>Network</v-list-subheader>
                  <v-list-item
                    v-if="nodeForTunnelClient(c.id)"
                    prepend-icon="mdi-lan-disconnect"
                    :title="nodeForTunnelClient(c.id)?.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
                    @click="confirmEvict(nodeForTunnelClient(c.id)!)"
                  />
                  <v-divider />
                  <v-list-item
                    prepend-icon="mdi-delete"
                    title="Delete tunnel client"
                    base-color="error"
                    @click="confirmTunnelDelete(c)"
                  />
                </v-list>
              </v-menu>
            </td>
            <td class="font-weight-medium">{{ c.name }}</td>
            <td>
              <v-chip
                size="small"
                :color="c.status === 'online' ? 'success' : 'grey'"
                variant="flat"
              >
                {{ c.status }}
              </v-chip>
            </td>
            <td>
              <v-chip size="x-small" variant="tonal">{{ c.os }}</v-chip>
            </td>
            <td>
              <template v-if="nodeForTunnelClient(c.id)">
                <div class="text-caption font-mono">
                  {{ nodeForTunnelClient(c.id)!.overlay_ip }}
                </div>
                <div
                  class="text-caption text-medium-emphasis font-mono"
                  :title="deriveOverlayV6(nodeForTunnelClient(c.id)!.overlay_ip)"
                >
                  {{ deriveOverlayV6(nodeForTunnelClient(c.id)!.overlay_ip) }}
                </div>
              </template>
              <span v-else class="text-caption text-medium-emphasis">—</span>
            </td>
            <td class="text-caption">{{ c.client_version || '—' }}</td>
            <td class="text-caption" :title="fmtDate(c.last_seen_at)">
              {{ fmtRelative(c.last_seen_at) }}
            </td>
          </tr>
        </tbody>
      </v-table>
      <div v-else class="text-center pa-4 text-medium-emphasis">
        <p class="mb-1">No tunnel clients enrolled.</p>
        <p class="text-body-2">
          Tunnel clients are CLI-only endpoints (SOCKS5 / port forwards) with
          no remote-desktop — enroll one with the command from "Enroll tunnel
          client".
        </p>
      </div>
    </v-card-text>
  </v-card>

  <!-- S4 — unified enrollment dialog (token + per-OS install commands
       derived from this origin; template map vitest-locked). -->
  <EnrollmentDialog
    :model-value="enrollDialogOpen"
    kind="agent"
    :token="enrollToken?.enrollment_token ?? null"
    :expires-in="enrollToken?.expires_in ?? null"
    :loading="enrollLoading"
    :error="enrollError"
    @update:model-value="(v: boolean) => { if (!v) closeEnrollDialog() }"
  />

  <!-- Tunnel-client enrollment (folded from /network/tunnel-clients) -->
  <EnrollmentDialog
    :model-value="tunnelEnrollDialogOpen"
    kind="tunnel"
    :token="tunnelEnrollToken?.enrollment_token ?? null"
    :expires-in="tunnelEnrollToken?.expires_in ?? null"
    :loading="tunnelEnrollLoading"
    :error="tunnelEnrollError"
    @update:model-value="(v: boolean) => { if (!v) closeTunnelEnrollDialog() }"
  />

  <!-- Overlay eviction (lifted from MachinesSection; shared by devices and
       tunnel clients — the confirm copy branches on will_rejoin). -->
  <v-dialog v-model="evictDialogOpen" max-width="540">
    <v-card>
      <v-card-title>
        {{ evictTarget?.will_rejoin
          ? `Force a new overlay address for “${evictTarget?.name}”?`
          : `Remove “${evictTarget?.name}” from the mesh?` }}
      </v-card-title>
      <v-card-text>
        <ul class="text-body-2 mb-3 ps-4">
          <li>
            It leaves the mesh immediately — every peer drops its routes, and
            traffic to
            <span class="text-mono">{{ evictTarget?.overlay_ip }}</span>
            stops.
          </li>
          <li>
            That address is released back to this tenant's pool and may later
            be assigned to a <strong>different</strong> machine.
          </li>
          <li v-if="evictTarget?.will_rejoin">
            <strong>{{ evictTarget?.name }}</strong> is still enrolled, so it
            rejoins automatically on its next connect — with a
            <strong>different</strong> overlay address. This does not revoke
            access; to remove the device for good, delete the agent or tunnel
            client.
          </li>
          <li v-else>
            Its backing device is no longer enrolled, so it will not come back.
          </li>
        </ul>
        <v-alert type="warning" variant="tonal" density="compact" class="mb-0">
          Anything pinned to the old address stops working — firewall rules,
          scripts, and any client configured with
          <code>overlay_exit_node = "{{ evictTarget?.name }}"</code>.
        </v-alert>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="evictDialogOpen = false">Cancel</v-btn>
        <v-btn color="error" variant="flat" :loading="evicting" @click="performEvict">
          {{ evictTarget?.will_rejoin ? 'Evict and reassign' : 'Remove from mesh' }}
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Tunnel-client delete confirmation (folded from TunnelClientsSection) -->
  <v-dialog v-model="tunnelDeleteDialogOpen" max-width="480">
    <v-card>
      <v-card-title>Delete tunnel client?</v-card-title>
      <v-card-text>
        <p class="font-weight-medium mb-2">{{ tunnelDeleteTarget?.name }}</p>
        <p class="text-body-2">
          Its access token is revoked and, if it joined the overlay, its node
          is evicted and the overlay address released. The CLI stays installed
          on the machine and can be re-enrolled with a new token.
        </p>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="tunnelDeleteDialogOpen = false">Cancel</v-btn>
        <v-btn
          color="error"
          variant="flat"
          :loading="tunnelDeleting"
          @click="performTunnelDelete"
        >
          Delete
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Crash reports modal -->
  <AgentCrashesDialog
    v-model="crashesDialogOpen"
    :tenant-id="tenantId"
    :agent-id="crashesTarget?.id ?? ''"
    :agent-name="crashesTarget?.name ?? ''"
  />

  <!-- Logs viewer modal (centralized agent-log upload, rc.58/rc.59) -->
  <AgentLogsDialog
    v-model="logsDialogOpen"
    :tenant-id="tenantId"
    :agent-id="logsTarget?.id ?? ''"
    :agent-name="logsTarget?.name ?? ''"
  />

  <!-- S1a — Update-all confirmation -->
  <v-dialog v-model="updateAllDialogOpen" max-width="480">
    <v-card>
      <v-card-title>Update all agents?</v-card-title>
      <v-card-text>
        Pushes an immediate self-update to every agent in this tenant
        ({{ agentStore.agents.length }} total,
        {{ agentStore.agents.filter((a) => a.is_online).length }} online).
        Each agent downloads the latest release, installs it, and restarts —
        active remote-control sessions on updating agents will drop for a few
        seconds. Offline agents update on their next periodic check.
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="updateAllDialogOpen = false">Cancel</v-btn>
        <v-btn color="primary" variant="flat" @click="doUpdateAll" :loading="updatingAll">
          Update all
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Delete confirmation -->
  <v-dialog v-model="deleteDialogOpen" max-width="540">
    <v-card>
      <v-card-title>Remove this device from the tenant?</v-card-title>
      <v-card-text>
        <p class="font-weight-medium mb-3">{{ deleteTarget?.name }}</p>
        <ul class="text-body-2 mb-3 ps-4">
          <li>
            It is evicted from the overlay mesh immediately — peers drop its
            routes, and any live remote-control or tunnel session to it ends.
          </li>
          <li>
            Its overlay address is released back to this tenant's pool and may
            later be assigned to a <strong>different</strong> machine.
          </li>
          <li>
            The agent stays installed on the host. It can be enrolled again with
            a new enrollment token, but it comes back with a
            <strong>new overlay address and a new name</strong>.
          </li>
        </ul>
        <v-alert type="warning" variant="tonal" density="compact" class="mb-0">
          Anything pinned to the old identity stops working — scripts using this
          agent id, tunnel forwards targeting it, firewall rules on its old
          overlay address, and any client configured with
          <code>overlay_exit_node = "{{ deleteTarget?.name }}"</code>.
        </v-alert>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="deleteDialogOpen = false">Cancel</v-btn>
        <v-btn color="error" variant="flat" @click="performDelete" :loading="deleting">
          Remove device
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Reassign device owner (MANAGE_AGENTS) -->
  <v-dialog v-model="reassignDialogOpen" max-width="440">
    <v-card>
      <v-card-title>Reassign device owner</v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          Owner of <strong>{{ reassignTarget?.name }}</strong>. The owner self-controls
          without an allowlist entry, and consent (email / push) routes to them.
        </p>
        <v-select
          v-model="reassignOwnerId"
          :items="memberSelectItems"
          label="New owner"
          density="compact"
          variant="outlined"
          hide-details
        />
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="reassignDialogOpen = false">Cancel</v-btn>
        <v-btn
          color="primary"
          variant="flat"
          :loading="reassignBusy"
          :disabled="!reassignOwnerId || reassignOwnerId === reassignTarget?.owner_user_id"
          @click="confirmReassign"
        >
          Reassign
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Subnet routes (mesh subnet-router, Phase 2 — MANAGE_AGENTS) -->
  <v-dialog v-model="routesDialogOpen" max-width="560">
    <v-card>
      <v-card-title>Subnet routes</v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          CIDRs that <strong>{{ routesTarget?.name }}</strong> advertises for the
          mesh (<code>roomler-tunnel socks5</code>). A LAN target IP matching one of
          these routes is dialed by this agent, so the mesh can reach non-agent
          devices behind it (a NAS, a printer, a database host). Access is still
          gated by your <strong>Tunnel ACL</strong> — a route steers the dial, it
          doesn't authorize it.
        </p>

        <div v-if="routesDraft.length" class="d-flex flex-wrap gap-2 mb-3">
          <v-chip
            v-for="cidr in routesDraft"
            :key="cidr"
            closable
            size="small"
            variant="tonal"
            color="primary"
            prepend-icon="mdi-ip-network-outline"
            @click:close="removeRoute(cidr)"
          >
            {{ cidr }}
          </v-chip>
        </div>
        <p v-else class="text-body-2 text-medium-emphasis mb-3">
          No routes yet — this agent only reaches its own host.
        </p>

        <div v-if="advertisedSuggestions.length" class="mb-3">
          <p class="text-caption text-medium-emphasis mb-1">
            Advertised by the agent — click to approve into the routes above:
          </p>
          <div class="d-flex flex-wrap gap-2">
            <v-chip
              v-for="cidr in advertisedSuggestions"
              :key="cidr"
              size="small"
              variant="outlined"
              color="primary"
              prepend-icon="mdi-lan-pending"
              append-icon="mdi-plus"
              @click="approveAdvertised(cidr)"
              :aria-label="`Approve advertised route ${cidr}`"
            >
              {{ cidr }}
            </v-chip>
          </div>
        </div>

        <v-text-field
          v-model="routeInput"
          label="Add route (CIDR)"
          placeholder="10.66.24.0/24"
          density="compact"
          variant="outlined"
          :error-messages="routeInputError ? [routeInputError] : []"
          append-inner-icon="mdi-plus"
          hint="e.g. 10.66.24.0/24 (subnet) or 10.66.24.53/32 (single host)"
          persistent-hint
          @click:append-inner="addRoute"
          @keyup.enter="addRoute"
        />
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="routesDialogOpen = false">Cancel</v-btn>
        <v-btn color="primary" variant="flat" :loading="routesBusy" @click="saveRoutes">
          Save
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, watch } from 'vue'
import { useDisplay } from 'vuetify'
import {
  useAgentStore,
  type Agent,
  type ConsentMode,
  type EnrollmentToken,
} from '@/stores/agents'
import { codecChips } from './agentCodecChips'
import AgentCrashesDialog from './AgentCrashesDialog.vue'
import AgentLogsDialog from './AgentLogsDialog.vue'
import EnrollmentDialog from '@/components/enroll/EnrollmentDialog.vue'
import {
  useOverlayRoutesStore,
  deriveOverlayV6,
  type OverlayNode,
} from '@/stores/overlayRoutes'
import {
  useTunnelClientStore,
  type TunnelClient,
  type TunnelEnrollmentToken,
} from '@/stores/tunnelClients'

const props = defineProps<{ tenantId: string }>()

const agentStore = useAgentStore()
const overlayStore = useOverlayRoutesStore()
const tunnelClientStore = useTunnelClientStore()

// ── Unified-devices join (2026-08-04): overlay node per device row. The
// wire's agent_id / tunnel_client_id FKs are the join key — the node NAME
// is a lossy, de-duplicated DNS label and must not be used for this.
const nodesByAgentId = computed(() => {
  const m = new Map<string, OverlayNode>()
  for (const n of overlayStore.nodes) if (n.agent_id) m.set(n.agent_id, n)
  return m
})
const nodesByTunnelClientId = computed(() => {
  const m = new Map<string, OverlayNode>()
  for (const n of overlayStore.nodes) if (n.tunnel_client_id) m.set(n.tunnel_client_id, n)
  return m
})
function nodeForAgent(agentId: string): OverlayNode | undefined {
  return nodesByAgentId.value.get(agentId)
}
function nodeForTunnelClient(tcId: string): OverlayNode | undefined {
  return nodesByTunnelClientId.value.get(tcId)
}

// ── Overlay eviction (lifted from MachinesSection) ───────────────
const evictDialogOpen = ref(false)
const evictTarget = ref<OverlayNode | null>(null)
const evicting = ref(false)
function confirmEvict(node: OverlayNode) {
  evictTarget.value = node
  evictDialogOpen.value = true
}
async function performEvict() {
  if (!evictTarget.value) return
  evicting.value = true
  try {
    await overlayStore.evictNode(props.tenantId, evictTarget.value.id)
    evictDialogOpen.value = false
    evictTarget.value = null
    await overlayStore.fetchNodes(props.tenantId)
  } finally {
    evicting.value = false
  }
}

// ── Tunnel clients (folded from TunnelClientsSection) ────────────
const tunnelEnrollDialogOpen = ref(false)
const tunnelEnrollToken = ref<TunnelEnrollmentToken | null>(null)
const tunnelEnrollLoading = ref(false)
const tunnelEnrollError = ref<string | null>(null)
async function openTunnelEnrollDialog() {
  tunnelEnrollDialogOpen.value = true
  tunnelEnrollLoading.value = true
  tunnelEnrollToken.value = null
  tunnelEnrollError.value = null
  try {
    tunnelEnrollToken.value = await tunnelClientStore.issueEnrollmentToken(props.tenantId)
  } catch (e) {
    tunnelEnrollError.value = (e as Error).message
  } finally {
    tunnelEnrollLoading.value = false
  }
}
function closeTunnelEnrollDialog() {
  tunnelEnrollDialogOpen.value = false
  tunnelEnrollToken.value = null
  tunnelEnrollError.value = null
  // A freshly-enrolled client shows up on the next fetch.
  tunnelClientStore.fetchTunnelClients(props.tenantId)
}

const tunnelDeleteDialogOpen = ref(false)
const tunnelDeleteTarget = ref<TunnelClient | null>(null)
const tunnelDeleting = ref(false)
function confirmTunnelDelete(c: TunnelClient) {
  tunnelDeleteTarget.value = c
  tunnelDeleteDialogOpen.value = true
}
async function performTunnelDelete() {
  if (!tunnelDeleteTarget.value) return
  tunnelDeleting.value = true
  try {
    await tunnelClientStore.deleteTunnelClient(props.tenantId, tunnelDeleteTarget.value.id)
    tunnelDeleteDialogOpen.value = false
    tunnelDeleteTarget.value = null
    await Promise.all([
      tunnelClientStore.fetchTunnelClients(props.tenantId),
      overlayStore.fetchNodes(props.tenantId),
    ])
  } finally {
    tunnelDeleting.value = false
  }
}

// Per-device consent mode. Email/Push route the request to the device OWNER
// (approve-link) for unattended hosts; PromptThenEmail (host-then-email hybrid)
// is not offered yet — it still behaves as host-prompt server-side.
const CONSENT_MODE_ITEMS: { title: string; value: ConsentMode }[] = [
  { title: 'Prompt on host (attended)', value: 'prompt' },
  { title: 'Auto-grant (unattended)', value: 'auto' },
  { title: 'Email the owner', value: 'email' },
  { title: 'Push to the owner', value: 'push' },
  { title: 'Prompt host + email owner', value: 'prompt_then_email' },
]
// Agent id whose consent mode is mid-update (disables + spins that row's select).
const consentBusy = ref<string | null>(null)
async function onConsentModeChange(a: Agent, mode: ConsentMode) {
  if ((a.access_policy.consent_mode ?? 'prompt') === mode) return
  consentBusy.value = a.id
  try {
    await agentStore.updateAccessPolicy(props.tenantId, a.id, {
      ...a.access_policy,
      consent_mode: mode,
    })
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    consentBusy.value = null
  }
}

// `mobile` flips below sm (~600px) so the table stays usable on tablets
// and small laptops; `lgAndDown` (≤1280) drives the codec-chip rollup so
// mid-width viewports don't blow past the Actions column.
const { smAndDown: mobile, lgAndDown } = useDisplay()

const enrollDialogOpen = ref(false)
const enrollLoading = ref(false)
const enrollToken = ref<EnrollmentToken | null>(null)
const enrollError = ref<string | null>(null)

// Per-row copy-feedback for the agent-id copy button. Holds the id
// of the last-copied agent for 2 s so the row's mdi-content-copy
// icon swaps to mdi-check (and back) without us having to thread
// state through each row.
const copiedAgentId = ref<string | null>(null)
let copiedAgentIdTimer: ReturnType<typeof setTimeout> | null = null

const deleteDialogOpen = ref(false)
const deleteTarget = ref<Agent | null>(null)
const deleting = ref(false)

// S1a — operator-forced self-update. Per-row busy id + a transient
// notice rendered in the info alert at the top of the card.
const updateBusy = ref<string | null>(null)
const updateNotice = ref<string | null>(null)
const updateAllDialogOpen = ref(false)
const updatingAll = ref(false)

async function triggerUpdate(a: Agent) {
  updateBusy.value = a.id
  try {
    const delivered = await agentStore.triggerUpdate(props.tenantId, a.id)
    updateNotice.value = delivered
      ? `Update pushed to ${a.name} — it will download, install, and restart shortly.`
      : `${a.name} is offline — it will update on its next periodic check instead.`
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    updateBusy.value = null
  }
}

async function doUpdateAll() {
  updatingAll.value = true
  try {
    const res = await agentStore.triggerUpdateAll(props.tenantId)
    updateNotice.value =
      `Update pushed to ${res.delivered} of ${res.requested} agents — ` +
      `offline agents update on their next periodic check.`
    updateAllDialogOpen.value = false
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    updatingAll.value = false
  }
}

// Crash-reports modal state (Task 9 Phase 3). Re-fetches on open via
// the dialog's own watcher; no caching here.
const crashesDialogOpen = ref(false)
const crashesTarget = ref<Agent | null>(null)

function openCrashes(a: Agent) {
  crashesTarget.value = a
  crashesDialogOpen.value = true
}

// Logs-viewer modal state (rc.74). Same no-cache, fetch-on-open
// pattern as the crashes dialog — the AgentLogsDialog refetches the
// recent uploaded log batches each time it opens.
const logsDialogOpen = ref(false)
const logsTarget = ref<Agent | null>(null)

function openLogs(a: Agent) {
  logsTarget.value = a
  logsDialogOpen.value = true
}

function osIcon(os: string) {
  switch (os) {
    case 'linux': return 'mdi-linux'
    case 'macos': return 'mdi-apple'
    case 'windows': return 'mdi-microsoft-windows'
    default: return 'mdi-desktop-classic'
  }
}

/** Phase A-1 — tolerate older API responses without `presence`. */
function presenceOf(a: Agent): 'online' | 'stale' | 'offline' {
  return a.presence ?? (a.is_online ? 'online' : 'offline')
}

function presenceColor(a: Agent) {
  const p = presenceOf(a)
  if (p === 'online') return 'success'
  if (p === 'stale') return 'warning'
  return 'grey'
}

function presenceIcon(a: Agent) {
  const p = presenceOf(a)
  if (p === 'online') return 'mdi-circle'
  if (p === 'stale') return 'mdi-circle-half-full'
  return 'mdi-circle-outline'
}

function presenceTitle(a: Agent): string {
  if (presenceOf(a) !== 'stale') return ''
  return (
    'Socket stale — the agent heartbeats but no server holds its live connection ' +
    '(half-open network leg or a recent server roll). It should self-heal within ' +
    'minutes; otherwise restart the Roomler service on the machine.'
  )
}

function statusColor(a: Agent) {
  const p = presenceOf(a)
  if (p === 'online') return 'success'
  if (p === 'stale') return 'warning'
  if (a.status === 'quarantined') return 'error'
  return 'grey'
}

function statusLabel(a: Agent): string {
  const p = presenceOf(a)
  if (p === 'online') return 'Online'
  if (p === 'stale') return 'Stale'
  return a.status
}

function fmtDate(iso: string): string {
  if (!iso) return '—'
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return iso
  }
}

function fmtRelative(iso: string): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const diff = Date.now() - d.getTime()
  if (diff < 0) return 'just now'
  const mins = Math.floor(diff / 60000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  const days = Math.floor(hours / 24)
  if (days < 30) return `${days}d ago`
  const months = Math.floor(days / 30)
  if (months < 12) return `${months}mo ago`
  return `${Math.floor(months / 12)}y ago`
}

async function openEnrollDialog() {
  enrollDialogOpen.value = true
  enrollLoading.value = true
  enrollToken.value = null
  enrollError.value = null
  try {
    enrollToken.value = await agentStore.issueEnrollmentToken(props.tenantId)
  } catch (e) {
    enrollError.value = (e as Error).message
  } finally {
    enrollLoading.value = false
  }
}

function closeEnrollDialog() {
  enrollDialogOpen.value = false
  enrollToken.value = null
}

/**
 * Copy an agent's hex ObjectId to the clipboard so the operator can
 * paste it into `roomler-tunnel forward --agent <hex>` (or any other
 * CLI / API call that needs the agent identifier).
 *
 * Surfaces transient success state via [`copiedAgentId`] for 2 s —
 * the row's copy button swaps to mdi-check during that window.
 *
 * Best-effort: a clipboard-write failure is logged but otherwise
 * silently swallowed; the operator can fall back to copying from
 * the tooltip (the full id is rendered in the row's `title`
 * attribute, so a long-hover surfaces it natively).
 */
async function copyAgentId(id: string) {
  try {
    await navigator.clipboard.writeText(id)
    copiedAgentId.value = id
    if (copiedAgentIdTimer !== null) {
      clearTimeout(copiedAgentIdTimer)
    }
    copiedAgentIdTimer = setTimeout(() => {
      copiedAgentId.value = null
      copiedAgentIdTimer = null
    }, 2000)
  } catch (err) {
    console.warn('copyAgentId: clipboard write failed', err)
  }
}

/**
 * Truncate a hex id to a 6-char prefix + ellipsis for inline display.
 * The full id is always available via the cell's `title` attribute
 * (long-press / hover tooltip) and the Copy button puts the full
 * value on the clipboard. Inline truncation keeps the Agents table
 * from blowing past the Actions column on mid-width viewports.
 */
function shortId(id: string): string {
  if (!id) return '—'
  if (id.length <= 8) return id
  return `${id.slice(0, 6)}…`
}

function confirmDelete(a: Agent) {
  deleteTarget.value = a
  deleteDialogOpen.value = true
}

async function performDelete() {
  if (!deleteTarget.value) return
  deleting.value = true
  try {
    await agentStore.deleteAgent(props.tenantId, deleteTarget.value.id)
    deleteDialogOpen.value = false
    deleteTarget.value = null
  } finally {
    deleting.value = false
  }
}

// Owner reassignment (MANAGE_AGENTS). Resolve owner_user_id → a name via the
// tenant members, and pick a new owner from that list.
const reassignDialogOpen = ref(false)
const reassignTarget = ref<Agent | null>(null)
const reassignOwnerId = ref<string>('')
const reassignBusy = ref(false)
const memberSelectItems = computed(() =>
  agentStore.tenantMembers.map((m) => ({
    title: m.display_name || m.nickname || m.user_id,
    value: m.user_id,
  })),
)
function openReassign(a: Agent) {
  reassignTarget.value = a
  reassignOwnerId.value = a.owner_user_id
  reassignDialogOpen.value = true
}
async function confirmReassign() {
  if (!reassignTarget.value || !reassignOwnerId.value) return
  reassignBusy.value = true
  try {
    await agentStore.updateOwner(props.tenantId, reassignTarget.value.id, reassignOwnerId.value)
    reassignDialogOpen.value = false
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    reassignBusy.value = false
  }
}

// Subnet routes (mesh subnet-router, Phase 2 — MANAGE_AGENTS). Edit the agent's
// advertised route CIDRs; the mesh longest-prefix-matches a LAN target IP
// against these. The draft is applied atomically on Save via a full-replace PUT.
const routesDialogOpen = ref(false)
const routesTarget = ref<Agent | null>(null)
const routesDraft = ref<string[]>([])
const routeInput = ref('')
const routeInputError = ref<string | null>(null)
const routesBusy = ref(false)

function openRoutes(a: Agent) {
  routesTarget.value = a
  routesDraft.value = [...(a.routes ?? [])]
  routeInput.value = ''
  routeInputError.value = null
  routesDialogOpen.value = true
}

/**
 * Validate a CIDR and return its canonical network form — IPv4 host bits are
 * masked to the network address, so `10.66.24.53/24` → `10.66.24.0/24`.
 * Returns null for anything that isn't valid CIDR notation: a bare IP (no
 * prefix), a bad octet, or an out-of-range prefix. IPv6 is validated loosely
 * and returned as-typed; the server canonicalizes authoritatively (both ends
 * share the same `ipnet` semantics). Mirrors `normalize_routes` in the API.
 */
function canonicalizeCidr(raw: string): string | null {
  const t = raw.trim()
  const m = t.match(/^([^/]+)\/(\d{1,3})$/)
  if (!m) return null
  const addr = m[1]!
  const prefix = Number(m[2])
  if (addr.includes(':')) {
    // IPv6 — loose check; the server has the authoritative parser.
    if (prefix < 0 || prefix > 128 || !/^[0-9a-fA-F:]+$/.test(addr)) return null
    return `${addr}/${prefix}`
  }
  const octets = addr.split('.')
  if (octets.length !== 4) return null
  if (!octets.every((o) => /^\d{1,3}$/.test(o) && Number(o) <= 255)) return null
  if (prefix < 0 || prefix > 32) return null
  const nums = octets.map((o) => Number(o))
  const bits = ((nums[0]! << 24) | (nums[1]! << 16) | (nums[2]! << 8) | nums[3]!) >>> 0
  const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0
  const net = (bits & mask) >>> 0
  const masked = [(net >>> 24) & 255, (net >>> 16) & 255, (net >>> 8) & 255, net & 255].join('.')
  return `${masked}/${prefix}`
}

function addRoute() {
  const canonical = canonicalizeCidr(routeInput.value)
  if (!canonical) {
    routeInputError.value = 'Enter a valid CIDR, e.g. 10.66.24.0/24 or 10.66.24.53/32'
    return
  }
  if (!routesDraft.value.includes(canonical)) {
    routesDraft.value.push(canonical)
  }
  routeInput.value = ''
  routeInputError.value = null
}

function removeRoute(cidr: string) {
  routesDraft.value = routesDraft.value.filter((r) => r !== cidr)
}

/** Advertised routes the agent proposed that aren't already in the draft —
 *  canonicalized + deduped so they compare cleanly against approved routes.
 *  Approving one moves it into the routes draft (and it drops off this list). */
const advertisedSuggestions = computed<string[]>(() => {
  const adv = routesTarget.value?.advertised_routes ?? []
  const seen = new Set(routesDraft.value)
  const out: string[] = []
  for (const raw of adv) {
    const c = canonicalizeCidr(raw)
    if (c && !seen.has(c) && !out.includes(c)) out.push(c)
  }
  return out
})

/** Approve an advertised CIDR (already canonical from `advertisedSuggestions`)
 *  into the routes draft; Save then persists it to the agent's approved routes. */
function approveAdvertised(cidr: string) {
  if (!routesDraft.value.includes(cidr)) {
    routesDraft.value.push(cidr)
  }
}

async function saveRoutes() {
  if (!routesTarget.value) return
  routesBusy.value = true
  try {
    await agentStore.updateRoutes(props.tenantId, routesTarget.value.id, routesDraft.value)
    routesDialogOpen.value = false
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    routesBusy.value = false
  }
}

onMounted(() => {
  agentStore.fetchAgents(props.tenantId)
  agentStore.fetchTenantMembers(props.tenantId)
  overlayStore.fetchNodes(props.tenantId)
  tunnelClientStore.fetchTunnelClients(props.tenantId)
})

watch(() => props.tenantId, (tid) => {
  if (tid) {
    agentStore.fetchAgents(tid)
    overlayStore.fetchNodes(tid)
    tunnelClientStore.fetchTunnelClients(tid)
  }
})
</script>

<style scoped>
/* Action column: keep it leftmost AND narrow so mid-width viewports never
   push it off-screen. Two icon buttons fit in ~96px. */
.agents-table :deep(th.agents-actions-col),
.agents-table :deep(td.agents-actions-col) {
  width: 96px;
  min-width: 96px;
  white-space: nowrap;
}
</style>
