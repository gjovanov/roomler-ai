<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <v-card>
    <!-- No title text: the hosting view's h1 already says "Devices"
         (consistent with /rooms and /analytics) — the bar holds actions only. -->
    <v-card-title class="d-flex align-center">
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
      <!-- ONE enroll entry point for both kinds — the tunnel-clients card
           (and its enroll button) is mobile-only now that desktop rows are
           unified into the grid. -->
      <v-menu>
        <template #activator="{ props: menuProps }">
          <v-btn
            v-bind="menuProps"
            data-tour="enroll-button"
            prepend-icon="mdi-key-plus"
            append-icon="mdi-menu-down"
            color="primary"
            variant="flat"
            size="small"
          >
            Enroll
          </v-btn>
        </template>
        <v-list density="compact">
          <v-list-item prepend-icon="mdi-monitor" title="Device (remote desktop + mesh)" @click="openEnrollDialog" />
          <v-list-item prepend-icon="mdi-lan-pending" title="Tunnel client (CLI-only)" @click="openTunnelEnrollDialog" />
        </v-list>
      </v-menu>
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

      <!-- Mobile-only initial spinner — the desktop grid carries its own
           :loading state and must render during the first fetch. -->
      <div
        v-if="mobile && agentStore.loading && agentStore.agents.length === 0"
        class="d-flex justify-center pa-8"
      >
        <v-progress-circular indeterminate />
      </div>

      <!-- Desktop / tablet (≥ sm): the UNIFIED server-driven grid — agents +
           tunnel clients in ONE v-data-table-server. Search / sort /
           pagination run on the SERVER (GET /tenant/{tid}/device — q matches
           across the overlay join incl. IP + MagicDNS), and column
           visibility/order are per-user (useGridColumns → localStorage).
           The grid rows are the lean DeviceRow feed; rich agent-only cells
           (consent, codecs, the action menu) look the full Agent up from
           agentStore by id. Action column stays LEFTMOST so it never falls
           off the right edge (field bug 2026-05-01). Below sm the dedicated
           card lists render further down. -->
      <template v-else-if="!mobile">
        <div class="d-flex align-center mb-2 flex-wrap">
          <v-text-field
            v-model="gridSearch"
            data-tour="device-search"
            density="compact"
            variant="outlined"
            hide-details
            clearable
            prepend-inner-icon="mdi-magnify"
            placeholder="Search name, tag, IP, MagicDNS…"
            style="max-width: 340px"
            aria-label="Search devices"
          />
          <!-- Kind filter. Default = devices WITHOUT tunnel clients — the
               remote-desktop fleet is what this page is about day-to-day;
               tunnels are one radio away. Server-side (`kind` param). -->
          <v-radio-group
            v-model="gridKind"
            inline
            hide-details
            density="compact"
            class="ml-4 flex-grow-0"
            aria-label="Filter by device kind"
          >
            <v-radio label="Devices" value="agent" density="compact" />
            <v-radio label="Tunnels" value="tunnel_client" density="compact" />
            <v-radio label="Both" value="both" density="compact" />
          </v-radio-group>
          <v-spacer />
          <span class="text-caption text-medium-emphasis mr-2">
            {{ deviceStore.total }}
            {{ gridKind === 'tunnel_client' ? (deviceStore.total === 1 ? 'tunnel' : 'tunnels') : deviceStore.total === 1 ? 'device' : 'devices' }}
          </span>
          <v-btn
            icon="mdi-cog-outline"
            size="small"
            variant="text"
            :color="colsCustomized ? 'primary' : undefined"
            title="Configure columns"
            aria-label="Configure columns"
            @click="colDialogOpen = true"
          />
        </div>
        <v-data-table-server
          data-tour="device-grid"
          v-model:page="gridPage"
          v-model:items-per-page="gridPerPage"
          :headers="effectiveHeaders"
          :items="deviceStore.items"
          :items-length="deviceStore.total"
          :loading="deviceStore.loading"
          :items-per-page-options="[10, 25, 50, 100]"
          density="compact"
          class="agents-table"
          item-value="id"
          @update:options="onGridOptions"
        >
          <template #item.actions="{ item }">
            <div class="agents-actions-col d-flex align-center">
              <v-btn
                v-if="item.kind === 'agent'"
                icon="mdi-remote-desktop"
                size="small"
                variant="text"
                color="primary"
                :disabled="!item.is_online"
                :to="{ name: 'agent-remote', params: { tenantId, agentId: item.id } }"
                :aria-label="`Connect to agent ${item.name}`"
              />
              <v-menu v-if="item.kind === 'agent' && agentFor(item)">
                <template #activator="{ props: menuProps }">
                  <v-btn
                    v-bind="menuProps"
                    icon="mdi-dots-vertical"
                    size="small"
                    variant="text"
                    :aria-label="`Actions for ${item.name}`"
                  />
                </template>
                <v-list density="compact" min-width="230">
                  <v-list-subheader>Maintenance</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-update"
                    title="Update now"
                    :disabled="updateBusy === item.id"
                    @click="triggerUpdate(agentFor(item)!)"
                  />
                  <v-list-item
                    v-if="caps.has('network')"
                    prepend-icon="mdi-key-change"
                    title="Rotate overlay key…"
                    :disabled="rotateKeyBusy === item.id"
                    @click="openRotateKey(agentFor(item)!)"
                  />
                  <v-list-subheader>Diagnostics</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-alert-circle-outline"
                    title="Crash reports"
                    @click="openCrashes(agentFor(item)!)"
                  />
                  <v-list-item
                    prepend-icon="mdi-text-box-search-outline"
                    title="Agent logs"
                    @click="openLogs(agentFor(item)!)"
                  />
                  <v-list-item
                    prepend-icon="mdi-console"
                    title="Device console"
                    @click="openConsole(agentFor(item)!)"
                  />
                  <v-list-subheader>Access</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-rename-box"
                    title="Edit name &amp; tags"
                    @click="openEdit(item)"
                  />
                  <v-list-item
                    prepend-icon="mdi-domain-plus"
                    title="Add to another organization"
                    @click="openJoinOrg(agentFor(item)!)"
                  />
                  <v-list-item
                    prepend-icon="mdi-account-switch"
                    title="Reassign owner"
                    @click="openReassign(agentFor(item)!)"
                  />
                  <v-list-item
                    prepend-icon="mdi-shield-key-outline"
                    title="Execution policy"
                    @click="openExecPolicy(agentFor(item)!)"
                  />
                  <v-list-item
                    v-if="caps.has('network')"
                    prepend-icon="mdi-console-network-outline"
                    title="SSH policy"
                    @click="openSshPolicy(agentFor(item)!)"
                  />
                  <!-- Writes an INTENT the device may refuse (step 5 of
                       docs/remote-config.md) — unlike the two policies above,
                       which take effect the moment they save. The dialog leads
                       with that difference. -->
                  <v-list-item
                    prepend-icon="mdi-cog-transfer-outline"
                    title="Device configuration"
                    @click="openRemoteConfig(agentFor(item)!)"
                  />
                  <v-list-subheader>Network</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-ip-network-outline"
                    title="Tunnel mesh routes"
                    @click="openRoutes(agentFor(item)!)"
                  />
                  <v-list-item
                    v-if="nodeForAgent(item.id)"
                    prepend-icon="mdi-lan-disconnect"
                    :title="nodeForAgent(item.id)?.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
                    @click="confirmEvict(nodeForAgent(item.id)!)"
                  />
                  <v-list-item
                    v-if="nodeForAgent(item.id)"
                    prepend-icon="mdi-lan-connect"
                    title="Overlay routes"
                    :to="{ path: `/tenant/${tenantId}/network/subnet-routes`, query: { node: nodeForAgent(item.id)!.id } }"
                  />
                  <v-list-item
                    prepend-icon="mdi-shield-lock-outline"
                    title="Overlay ACL"
                    :to="{ path: `/tenant/${tenantId}/network/acl`, query: { tab: 'overlay' } }"
                  />
                  <v-divider />
                  <v-list-item
                    prepend-icon="mdi-delete"
                    title="Delete device"
                    base-color="error"
                    @click="confirmDelete(agentFor(item)!)"
                  />
                </v-list>
              </v-menu>
              <v-menu v-else-if="item.kind === 'tunnel_client'">
                <template #activator="{ props: menuProps }">
                  <v-btn
                    v-bind="menuProps"
                    icon="mdi-dots-vertical"
                    size="small"
                    variant="text"
                    :aria-label="`Actions for ${item.name}`"
                  />
                </template>
                <v-list density="compact" min-width="230">
                  <v-list-item
                    prepend-icon="mdi-rename-box"
                    title="Edit name &amp; tags"
                    @click="openEdit(item)"
                  />
                  <v-list-subheader>Network</v-list-subheader>
                  <v-list-item
                    v-if="nodeForTunnelClient(item.id)"
                    prepend-icon="mdi-lan-disconnect"
                    :title="nodeForTunnelClient(item.id)?.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
                    @click="confirmEvict(nodeForTunnelClient(item.id)!)"
                  />
                  <v-divider />
                  <v-list-item
                    v-if="clientFor(item)"
                    prepend-icon="mdi-delete"
                    title="Delete tunnel client"
                    base-color="error"
                    @click="confirmTunnelDelete(clientFor(item)!)"
                  />
                </v-list>
              </v-menu>
            </div>
          </template>
          <template #item.name="{ item }">
            <div class="d-flex align-center">
              <v-icon
                :color="rowPresenceColor(item)"
                size="small"
                class="mr-2"
                :title="rowPresenceTitle(item)"
              >
                {{ rowPresenceIcon(item) }}
              </v-icon>
              <span class="font-weight-medium">{{ item.display_name || item.name }}</span>
              <!-- FR-51 — the vanishing must never be a surprise: this device
                   removes itself (reaped after silence; immediately on a clean
                   stop), and a later enrollment is a NEW device. -->
              <v-chip
                v-if="item.ephemeral"
                size="x-small"
                color="warning"
                variant="tonal"
                class="ml-2"
                prepend-icon="mdi-clock-fast"
                title="Ephemeral device — removes itself after inactivity or on clean shutdown; removal is final"
              >
                ephemeral
              </v-chip>
              <v-btn
                :icon="copiedAgentId === item.id ? 'mdi-check' : 'mdi-content-copy'"
                size="x-small"
                variant="text"
                :color="copiedAgentId === item.id ? 'success' : undefined"
                class="ml-1"
                @click="copyAgentId(item.id)"
                :aria-label="`Copy device ID for ${item.name}`"
                :title="copiedAgentId === item.id ? 'Copied!' : 'Copy device ID'"
              />
            </div>
            <div class="text-caption text-medium-emphasis d-flex align-center flex-nowrap">
              <!-- The machine-reported title collapses away by default once a
                   display name is set (column-picker checkbox). -->
              <span v-if="item.display_name && !hideNameWhenDisplay" class="mr-1">{{ item.name }} ·</span>
              <span class="agent-id-preview" :title="`Device ID: ${item.id}`">
                id: {{ shortId(item.id) }}
              </span>
              <span class="mx-1">·</span>
              <span :title="`machine_id: ${item.machine_id}`">{{ shortId(item.machine_id) }}</span>
              <span v-if="item.version"> · v{{ item.version }}</span>
              <!-- FR-27 — the companion's own version, and only when it
                   DISAGREES with the daemon's. Shown at all because the two
                   update by different mechanisms on every platform, so
                   "Update all" moving the daemon says nothing about the
                   desktop; hidden when they match because a second identical
                   number on every row is noise, and the whole point of the
                   field is the disagreement. -->
              <span
                v-if="companionSkew(item)"
                class="companion-skew ml-1"
                :title="`roomler-desktop is on v${companionSkew(item)} while the daemon is on v${item.version} — the companion updates separately (its own .deb on Linux, the .pkg on macOS, a daemon-side swap on Windows)`"
              >
                · desktop v{{ companionSkew(item) }}
              </span>
            </div>
          </template>
          <template #item.kind="{ item }">
            <v-chip size="x-small" variant="tonal" :color="item.kind === 'agent' ? 'primary' : 'secondary'">
              {{ item.kind === 'agent' ? 'device' : 'tunnel' }}
            </v-chip>
          </template>
          <template #item.status="{ item }">
            <v-chip
              size="small"
              :color="rowStatusColor(item)"
              variant="flat"
              :title="rowPresenceTitle(item)"
            >
              {{ rowStatusLabel(item) }}
            </v-chip>
            <v-chip
              v-if="item.kind === 'agent' && agentFor(item) && remoteConfigChip(agentFor(item)!)"
              size="x-small"
              variant="tonal"
              class="mt-1"
              :color="remoteConfigChip(agentFor(item)!)!.color"
              :prepend-icon="remoteConfigChip(agentFor(item)!)!.icon"
              :title="remoteConfigChip(agentFor(item)!)!.tooltip"
              @click="openRemoteConfig(agentFor(item)!)"
            >
              {{ remoteConfigChip(agentFor(item)!)!.text }}
            </v-chip>
            <v-chip
              v-if="item.kind === 'agent' && agentFor(item) && keyRotationChip(agentFor(item)!)"
              size="x-small"
              variant="tonal"
              class="mt-1"
              :color="keyRotationChip(agentFor(item)!)!.color"
              :prepend-icon="keyRotationChip(agentFor(item)!)!.icon"
              :title="keyRotationChip(agentFor(item)!)!.tooltip"
            >
              {{ keyRotationChip(agentFor(item)!)!.text }}
            </v-chip>
          </template>
          <template #item.os="{ item }">
            <v-chip size="x-small" :prepend-icon="osIcon(item.os as any)" variant="tonal">
              {{ item.os }}
            </v-chip>
          </template>
          <template #item.overlay_ip="{ item }">
            <template v-if="item.overlay_ip">
              <div class="text-caption font-mono">{{ item.overlay_ip }}</div>
              <div
                class="text-caption text-medium-emphasis font-mono"
                :title="deriveOverlayV6(item.overlay_ip) ?? undefined"
              >
                {{ deriveOverlayV6(item.overlay_ip) }}
              </div>
              <!-- FR-40 — the node's public key, so a rotation is visible as a
                   changed key rather than trusted from a chip. -->
              <div
                v-if="item.overlay_public_key"
                class="text-caption text-medium-emphasis font-mono"
                :title="`Overlay public key, epoch ${item.overlay_key_epoch ?? 0}: ${item.overlay_public_key}`"
              >
                key {{ item.overlay_public_key.slice(0, 10) }}… · e{{ item.overlay_key_epoch ?? 0 }}
              </div>
            </template>
            <span v-else class="text-caption text-medium-emphasis">—</span>
          </template>
          <template #item.magic_dns="{ item }">
            <div v-if="item.magic_dns_fqdn" class="d-flex align-center">
              <span class="text-caption font-mono">{{ item.magic_dns_fqdn }}</span>
              <v-btn
                :icon="copiedAgentId === item.magic_dns_fqdn ? 'mdi-check' : 'mdi-content-copy'"
                size="x-small"
                variant="text"
                class="ml-1"
                @click="copyAgentId(item.magic_dns_fqdn!)"
                :aria-label="`Copy MagicDNS name for ${item.name}`"
              />
            </div>
            <span v-else-if="item.magic_dns_name" class="text-caption font-mono" title="Tenant has no MagicDNS domain configured — bare overlay label">
              {{ item.magic_dns_name }}
            </span>
            <span v-else class="text-caption text-medium-emphasis">—</span>
          </template>
          <template #item.tags="{ item }">
            <div v-if="item.tags?.length" class="d-flex flex-nowrap gap-1">
              <v-chip v-for="t in item.tags.slice(0, 3)" :key="t" size="x-small" variant="tonal">
                {{ t }}
              </v-chip>
              <v-chip
                v-if="item.tags.length > 3"
                size="x-small"
                variant="tonal"
                :title="item.tags.slice(3).join(', ')"
              >
                +{{ item.tags.length - 3 }}
              </v-chip>
            </div>
            <span v-else class="text-caption text-medium-emphasis">—</span>
          </template>
          <template #item.consent="{ item }">
            <template v-if="item.kind === 'agent' && agentFor(item)">
              <v-select
                :model-value="agentFor(item)!.access_policy.consent_mode ?? 'prompt'"
                :items="CONSENT_MODE_ITEMS"
                density="compact"
                variant="plain"
                hide-details
                :disabled="consentBusy === item.id"
                :loading="consentBusy === item.id"
                class="consent-select"
                :aria-label="`Consent mode for ${item.name}`"
                @update:model-value="(m) => onConsentModeChange(agentFor(item)!, m as ConsentMode)"
              />
              <!-- FR-27 — the owner shortcut, made visible. Controlling your
                   OWN device auto-consents server-side BEFORE this mode is
                   read, so on a fleet where one person owns everything the
                   select above had no observable effect and nothing said so.
                   Shown only on devices YOU own; for everyone else the mode
                   already applies and the row would be noise. -->
              <v-checkbox
                v-if="isOwnedByMe(agentFor(item)!)"
                :model-value="agentFor(item)!.access_policy.prompt_owner ?? false"
                density="compact"
                hide-details
                :disabled="consentBusy === item.id"
                :label="ownerPromptLabel(agentFor(item)!)"
                :aria-label="`Apply the consent mode to me, the owner of ${item.name}`"
                class="consent-owner-toggle"
                @update:model-value="(v) => onPromptOwnerChange(agentFor(item)!, v === true)"
              />
              <!-- P6 — multi-user input mode (free-for-all | exclusive). -->
              <v-select
                :model-value="agentFor(item)!.access_policy.input_mode ?? 'free'"
                :items="INPUT_MODE_ITEMS"
                density="compact"
                variant="plain"
                hide-details
                :disabled="consentBusy === item.id"
                class="consent-select"
                :aria-label="`Input mode for ${item.name}`"
                @update:model-value="(m) => onInputModeChange(agentFor(item)!, m as 'free' | 'exclusive')"
              />
            </template>
            <span v-else class="text-caption text-medium-emphasis">—</span>
          </template>
          <template #item.codecs="{ item }">
            <template v-if="item.kind === 'agent' && agentFor(item)">
              <div v-if="codecChips(agentFor(item)!).length === 0" class="text-caption text-medium-emphasis">—</div>
              <div v-else-if="lgAndDown" class="d-flex flex-nowrap gap-1 align-center">
                <v-chip
                  size="x-small"
                  :color="codecChips(agentFor(item)!)[0].color"
                  variant="tonal"
                  :title="codecChips(agentFor(item)!).map(c => c.tooltip).join(', ')"
                >
                  {{ codecChips(agentFor(item)!)[0].label }}
                </v-chip>
                <v-chip
                  v-if="codecChips(agentFor(item)!).length > 1"
                  size="x-small"
                  variant="tonal"
                  :title="codecChips(agentFor(item)!).slice(1).map(c => c.tooltip).join(', ')"
                >
                  +{{ codecChips(agentFor(item)!).length - 1 }}
                </v-chip>
              </div>
              <div v-else class="d-flex flex-nowrap gap-1">
                <v-chip
                  v-for="codec in codecChips(agentFor(item)!)"
                  :key="codec.label"
                  size="x-small"
                  :color="codec.color"
                  variant="tonal"
                  :title="codec.tooltip"
                >
                  {{ codec.label }}
                </v-chip>
              </div>
              <!-- A device the OS has muzzled looks identical to a healthy one
                   until you connect and get a black screen, because macOS
                   reports success either way. Say it here instead. -->
              <div v-if="permissionWarnings(agentFor(item)!).length" class="d-flex flex-nowrap gap-1 mt-1">
                <v-chip
                  v-for="w in permissionWarnings(agentFor(item)!)"
                  :key="w.label"
                  size="x-small"
                  color="warning"
                  variant="tonal"
                  :title="w.tooltip"
                >
                  {{ w.label }}
                </v-chip>
              </div>
            </template>
            <span v-else class="text-caption text-medium-emphasis">—</span>
          </template>
          <template #item.last_seen_at="{ item }">
            <span class="text-caption" :title="fmtDate(item.last_seen_at)">{{ fmtRelative(item.last_seen_at) }}</span>
          </template>
          <template #no-data>
            <div class="text-center pa-8 text-medium-emphasis">
              <template v-if="gridSearch">
                No devices match "{{ gridSearch }}".
              </template>
              <template v-else>
                <v-icon size="48" color="grey-lighten-1" class="mb-2">mdi-desktop-classic</v-icon>
                <p class="mb-1">No devices enrolled yet.</p>
                <p class="text-body-2">
                  Click "Enroll" for a one-line installer per platform — the
                  machine appears here as soon as it enrolls.
                </p>
                <!-- FR-12 — the empty state is exactly where the tour helps. -->
                <v-btn
                  size="small"
                  variant="text"
                  color="primary"
                  prepend-icon="mdi-school-outline"
                  :to="{ name: 'tutorial', params: { tenantId }, hash: '#devices' }"
                  class="mt-2"
                >
                  Walk me through it
                </v-btn>
              </template>
            </div>
          </template>
        </v-data-table-server>
      </template>

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
              <v-chip
                v-if="remoteConfigChip(a)"
                size="x-small"
                variant="tonal"
                class="ml-1"
                :color="remoteConfigChip(a)!.color"
                :prepend-icon="remoteConfigChip(a)!.icon"
                :title="remoteConfigChip(a)!.tooltip"
                @click="openRemoteConfig(a)"
              >
                {{ remoteConfigChip(a)!.text }}
              </v-chip>
              <v-chip
                v-if="keyRotationChip(a)"
                size="x-small"
                variant="tonal"
                class="ml-1"
                :color="keyRotationChip(a)!.color"
                :prepend-icon="keyRotationChip(a)!.icon"
                :title="keyRotationChip(a)!.tooltip"
              >
                {{ keyRotationChip(a)!.text }}
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
              <!-- FR-27 — same rule as the grid caption: only when the
                   companion disagrees with the daemon. Warning-coloured
                   because a skew here means the desktop is running code the
                   daemon has moved past, which is what the operator hit. -->
              <v-chip
                v-if="a.companion_version && a.companion_version !== a.agent_version"
                size="x-small"
                color="warning"
                variant="tonal"
                :title="`roomler-desktop is on v${a.companion_version} while the daemon is on v${a.agent_version} — the companion updates separately`"
              >
                desktop v{{ a.companion_version }}
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
            <!-- FR-27 — see the desktop table above: the owner shortcut is
                 applied before the mode is read, so it has to be visible. -->
            <v-checkbox
              v-if="isOwnedByMe(a)"
              :model-value="a.access_policy.prompt_owner ?? false"
              density="compact"
              hide-details
              :disabled="consentBusy === a.id"
              :label="ownerPromptLabel(a)"
              :aria-label="`Apply the consent mode to me, the owner of ${a.name}`"
              class="mb-2"
              @update:model-value="(v) => onPromptOwnerChange(a, v === true)"
            />
            <!-- P6 — multi-user input mode. -->
            <v-select
              :model-value="a.access_policy.input_mode ?? 'free'"
              :items="INPUT_MODE_ITEMS"
              label="Input"
              density="compact"
              variant="outlined"
              hide-details
              :disabled="consentBusy === a.id"
              class="mb-2"
              :aria-label="`Input mode for ${a.name}`"
              @update:model-value="(m) => onInputModeChange(a, m as 'free' | 'exclusive')"
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
                  <v-list-item
                    v-if="caps.has('network')"
                    prepend-icon="mdi-key-change"
                    title="Rotate overlay key…"
                    :disabled="rotateKeyBusy === a.id"
                    @click="openRotateKey(a)"
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
                  <v-list-item
                    prepend-icon="mdi-console"
                    title="Device console"
                    @click="openConsole(a)"
                  />
                  <v-list-subheader>Access</v-list-subheader>
                  <v-list-item
                    prepend-icon="mdi-domain-plus"
                    title="Add to another organization"
                    @click="openJoinOrg(a)"
                  />
                  <v-list-item
                    prepend-icon="mdi-account-switch"
                    title="Reassign owner"
                    @click="openReassign(a)"
                  />
                  <v-list-item
                    prepend-icon="mdi-shield-key-outline"
                    title="Execution policy"
                    @click="openExecPolicy(a)"
                  />
                  <v-list-item
                    v-if="caps.has('network')"
                    prepend-icon="mdi-console-network-outline"
                    title="SSH policy"
                    @click="openSshPolicy(a)"
                  />
                  <!-- Writes an INTENT the device may refuse (step 5 of
                       docs/remote-config.md) — unlike the two policies above,
                       which take effect the moment they save. The dialog leads
                       with that difference. -->
                  <v-list-item
                    prepend-icon="mdi-cog-transfer-outline"
                    title="Device configuration"
                    @click="openRemoteConfig(a)"
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
                  <v-list-item
                    v-if="nodeForAgent(a.id)"
                    prepend-icon="mdi-lan-connect"
                    title="Overlay routes"
                    :to="{ path: `/tenant//network/subnet-routes`, query: { node: nodeForAgent(a.id)!.id } }"
                  />
                  <v-list-item
                    prepend-icon="mdi-shield-lock-outline"
                    title="Overlay ACL"
                    :to="{ path: `/tenant//network/acl`, query: { tab: 'overlay' } }"
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

  <!-- Tunnel clients — MOBILE ONLY since the unified desktop grid absorbed
       their rows (kind=tunnel). CLI-only endpoints, no remote-desktop/
       consent/codec cells. -->
  <v-card v-if="mobile" class="mt-4">
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
                  :title="deriveOverlayV6(nodeForTunnelClient(c.id)!.overlay_ip) ?? undefined"
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

  <!-- Per-user column visibility + order for the unified grid. -->
  <GridColumnPickerDialog
    v-model="colDialogOpen"
    :entries="colEntries"
    @toggle="colToggle"
    @reorder="colReorder"
    @reset="colReset"
  >
    <template #append>
      <v-divider class="my-1" />
      <v-checkbox-btn
        v-model="hideNameWhenDisplay"
        density="compact"
        class="px-2"
        label="Hide device name when a display name is set"
      />
    </template>
  </GridColumnPickerDialog>

  <!-- Name / display-name / tags (both kinds; rename propagates to MagicDNS). -->
  <DeviceEditDialog
    v-model="editDialogOpen"
    :tenant-id="tenantId"
    :device="editTarget"
    @saved="onDeviceSaved"
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

  <!-- Fleet RPC — run a command on the device, and edit the policy that
       decides whether anyone may. Both are guarded by `v-if` on the target
       so a null agent can never reach a dialog that assumes one. -->
  <DeviceConsoleDialog
    v-if="consoleTarget"
    v-model="consoleDialogOpen"
    :tenant-id="tenantId"
    :agent="consoleTarget"
  />
  <ExecPolicyDialog
    v-if="execPolicyTarget"
    v-model="execPolicyDialogOpen"
    :tenant-id="tenantId"
    :agent="execPolicyTarget"
  />
  <SshPolicyDialog
    v-if="sshPolicyTarget"
    v-model="sshPolicyDialogOpen"
    :tenant-id="tenantId"
    :agent="sshPolicyTarget"
  />
  <RemoteConfigDialog
    v-if="remoteConfigTarget"
    v-model="remoteConfigDialogOpen"
    :tenant-id="tenantId"
    :agent="remoteConfigTarget"
    @saved="agentStore.fetchAgents(tenantId); fetchGrid()"
  />

  <!-- FR-40 — rotate-overlay-key confirmation -->
  <v-dialog v-model="rotateKeyDialogOpen" max-width="520">
    <v-card>
      <v-card-title>Rotate the overlay key of {{ rotateKeyTarget?.name }}?</v-card-title>
      <v-card-text>
        The device mints a fresh WireGuard key <strong>on the device</strong>, saves it and
        re-joins the mesh under it. Every peer picks the new key up within seconds and the old
        key stops working everywhere. The server never sees a private key.
        <br /><br />
        Immediate and disruptive by design: every session this device carries in this
        organization (remote control, SSH over the overlay, tunnels) ends and reconnects.
        <template v-if="rotateKeyTarget && !rotateKeyTarget.is_online">
          <br /><br />
          The device is offline — the order is queued and runs on its next connect.
        </template>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="rotateKeyDialogOpen = false">Cancel</v-btn>
        <v-btn
          color="warning"
          variant="flat"
          :loading="rotateKeyBusy !== null"
          @click="doRotateKey"
        >
          Rotate key
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

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
  <!-- Multi-org — add an already-enrolled device to a second organization
       without touching the machine. Needs MANAGE_AGENTS in both; the picker
       only lists orgs where the caller actually has it. -->
  <v-dialog v-model="joinOrgDialogOpen" max-width="520">
    <v-card>
      <v-card-title>Add to another organization</v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          <strong>{{ joinOrgTarget?.name }}</strong> keeps its current enrollment and gains a
          second one. The device connects to both organizations at once; each sees only its
          own sessions, and the new org gets its own encryption key.
        </p>
        <v-alert
          v-if="joinOrgLoaded && !joinOrgSupported"
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-3"
          text="This device runs an agent version that predates remote org-join. Update it
                first, or enroll it at the keyboard."
        />
        <v-alert
          v-else-if="joinOrgLoaded && !joinOrgOnline"
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-3"
          text="This device is offline. It has to be connected to receive the invitation."
        />
        <v-alert
          v-else-if="joinOrgLoaded && joinOrgItems.length === 0"
          type="info"
          variant="tonal"
          density="compact"
          class="mb-3"
          text="You don't manage devices in any other organization. Ask an admin there to
                grant you device management, then try again."
        />
        <v-select
          v-model="joinOrgTargetTenant"
          :items="joinOrgSelectItems"
          :loading="joinOrgLoading"
          :disabled="!joinOrgSupported || !joinOrgOnline || joinOrgItems.length === 0"
          label="Organization"
          density="compact"
          variant="outlined"
          class="mb-3"
          hide-details
        />
        <v-select
          v-model="joinOrgOverlayMode"
          :items="[
            { title: 'No mesh access (default)', value: 'off' },
            { title: 'Join the mesh (needs multi-org TUN enabled)', value: 'tun' },
          ]"
          :disabled="!joinOrgSupported || !joinOrgOnline"
          label="Private mesh"
          density="compact"
          variant="outlined"
          hide-details
        />
        <v-alert
          v-if="joinOrgOverlayMode === 'tun' && joinOrgLoaded && !joinOrgMeshReady"
          type="info"
          variant="tonal"
          density="compact"
          class="mt-3"
          text="This device's agent started with a single organization, so it holds its
                network adapter exclusively. The join is applied right away, but mesh
                access begins after the agent restarts — the next auto-update does that."
        />
        <v-alert
          v-if="joinOrgError"
          type="error"
          variant="tonal"
          density="compact"
          class="mt-3"
          :text="joinOrgError"
        />
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="joinOrgDialogOpen = false">Cancel</v-btn>
        <v-btn
          color="primary"
          variant="flat"
          :loading="joinOrgBusy"
          :disabled="!joinOrgTargetTenant || !joinOrgSupported || !joinOrgOnline"
          @click="confirmJoinOrg"
        >
          Add device
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

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
      <v-card-title>Tunnel mesh routes</v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          CIDRs that <strong>{{ routesTarget?.name }}</strong> advertises for the
          mesh (<code>roomler socks5</code>). A LAN target IP matching one of
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
import { useRoute, useRouter } from 'vue-router'
import { useDisplay } from 'vuetify'
import {
  useAgentStore,
  type Agent,
  type ConsentMode,
  type EnrollmentToken,
} from '@/stores/agents'
import { codecChips, permissionWarnings } from './agentCodecChips'
import AgentCrashesDialog from './AgentCrashesDialog.vue'
import AgentLogsDialog from './AgentLogsDialog.vue'
import DeviceConsoleDialog from './DeviceConsoleDialog.vue'
import ExecPolicyDialog from './ExecPolicyDialog.vue'
import SshPolicyDialog from './SshPolicyDialog.vue'
import { useCapabilitiesStore } from '@/stores/capabilities'
import RemoteConfigDialog from './RemoteConfigDialog.vue'
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
import { useDeviceStore, type DeviceRow } from '@/stores/devices'
import { useAuthStore } from '@/stores/auth'
import { useGridColumns } from '@/composables/useGridColumns'
import GridColumnPickerDialog from '@/components/common/GridColumnPickerDialog.vue'
import DeviceEditDialog, { type EditableDevice } from '@/components/admin/DeviceEditDialog.vue'

const props = defineProps<{ tenantId: string }>()

const agentStore = useAgentStore()
const overlayStore = useOverlayRoutesStore()
const tunnelClientStore = useTunnelClientStore()
const deviceStore = useDeviceStore()
// FR-69 P9 — the overlay-key rotation, the SSH policy and the peer-relay
// policy are the network module's routes; a `remote` profile has no network
// module, and a menu item that leads to a 404 is worse than none.
const caps = useCapabilitiesStore()
const auth = useAuthStore()
// Declared before the grid state below — gridKind's initializer reads
// route.query.
const route = useRoute()
const router = useRouter()

// ── Unified server-driven grid state ─────────────────────────────
const gridSearch = ref('')
const gridPage = ref(1)
// Default 10 (operator, 2026-08-27): the devices page leads the product —
// a screenful beats a scroll; the footer still offers 25/50/100.
const gridPerPage = ref(10)
const gridSort = ref<string | undefined>(undefined)
const gridDir = ref<'asc' | 'desc' | undefined>(undefined)
/** Kind radio: devices-only by default — 'both' widens to tunnel clients.
 *  Mirrored to/from the `?type=` querystring (device | tunnel | both) so the
 *  dashboard's Tunnels tile can land here pre-filtered and the URL stays
 *  shareable. */
const gridKind = ref<'agent' | 'tunnel_client' | 'both'>(kindFromQuery(route.query.type))
const colDialogOpen = ref(false)

function kindFromQuery(v: unknown): 'agent' | 'tunnel_client' | 'both' {
  return v === 'tunnel' ? 'tunnel_client' : v === 'both' ? 'both' : 'agent'
}
// Query → radio (e.g. in-app navigation to ?type=tunnel while mounted).
watch(
  () => route.query.type,
  (t) => {
    const k = kindFromQuery(t)
    if (k !== gridKind.value) gridKind.value = k
  },
)
// Radio → query (replace, not push — a filter flip is not a history entry).
watch(gridKind, (k) => {
  const wanted = k === 'tunnel_client' ? 'tunnel' : k === 'both' ? 'both' : undefined
  if ((route.query.type ?? undefined) !== wanted) {
    router.replace({ query: { ...route.query, type: wanted } })
  }
})

/** Column-picker extra: collapse the Name cell to the display name alone
 *  when one is set (default ON — the machine-reported title is noise once
 *  someone has named the device). Per user+org, like the column prefs. */
const HIDE_NAME_KEY = () =>
  `roomler:grid-name-pref:${auth.user?.id ?? 'anon'}:${props.tenantId}:devices`
function loadHideName(): boolean {
  try {
    const v = localStorage.getItem(HIDE_NAME_KEY())
    return v === null ? true : v === '1'
  } catch {
    return true
  }
}
const hideNameWhenDisplay = ref(loadHideName())
watch(hideNameWhenDisplay, (v) => {
  try {
    localStorage.setItem(HIDE_NAME_KEY(), v ? '1' : '0')
  } catch {
    /* private browsing */
  }
})
watch(
  () => props.tenantId,
  () => {
    hideNameWhenDisplay.value = loadHideName()
  },
)

const deviceHeaders = computed(() => [
  // Leftmost on purpose — see the template comment (field bug 2026-05-01).
  { title: 'Actions', key: 'actions', sortable: false, width: 96 },
  { title: 'Name', key: 'name', sortable: true },
  { title: 'Kind', key: 'kind', sortable: true },
  { title: 'Status', key: 'status', sortable: true },
  { title: 'OS', key: 'os', sortable: true },
  { title: 'Overlay', key: 'overlay_ip', sortable: true },
  { title: 'MagicDNS', key: 'magic_dns', sortable: true },
  { title: 'Tags', key: 'tags', sortable: false },
  { title: 'Consent', key: 'consent', sortable: false },
  { title: 'Codecs', key: 'codecs', sortable: false },
  { title: 'Last seen', key: 'last_seen_at', sortable: true },
])
const {
  effectiveHeaders,
  entries: colEntries,
  toggle: colToggle,
  reorder: colReorder,
  reset: colReset,
  customized: colsCustomized,
} = useGridColumns({
  headers: deviceHeaders,
  gridId: 'devices',
  scope: () => `${auth.user?.id ?? 'anon'}:${props.tenantId}`,
})

function fetchGrid() {
  deviceStore
    .fetchDevices(props.tenantId, {
      page: gridPage.value,
      perPage: gridPerPage.value,
      q: gridSearch.value || undefined,
      sort: gridSort.value,
      dir: gridDir.value,
      kind: gridKind.value === 'both' ? undefined : gridKind.value,
    })
    .catch(() => {})
}

watch(gridKind, () => {
  if (gridPage.value !== 1) gridPage.value = 1 // options handler fetches
  else fetchGrid()
})

/** v-data-table-server fires this once on mount too — it is the grid's ONLY
 *  fetch trigger for page/sort changes (a separate onMounted fetch would
 *  double-load). */
function onGridOptions(opts: {
  page: number
  itemsPerPage: number
  sortBy: Array<{ key: string; order: 'asc' | 'desc' }>
}) {
  gridPage.value = opts.page
  gridPerPage.value = opts.itemsPerPage
  gridSort.value = opts.sortBy[0]?.key
  gridDir.value = opts.sortBy[0]?.order
  fetchGrid()
}

let gridSearchTimer: ReturnType<typeof setTimeout> | undefined
watch(gridSearch, () => {
  if (gridSearchTimer) clearTimeout(gridSearchTimer)
  gridSearchTimer = setTimeout(() => {
    // Back to page 1: if the page actually changes, the options handler
    // fetches; when already there, fetch directly.
    if (gridPage.value !== 1) gridPage.value = 1
    else fetchGrid()
  }, 300)
})

// Rich agent-only cells (consent selects, codec chips, the action menu)
// need the FULL Agent — the grid rows are the lean DeviceRow feed.
const agentById = computed(() => {
  const m = new Map<string, Agent>()
  for (const a of agentStore.agents) m.set(a.id, a)
  return m
})
function agentFor(row: DeviceRow): Agent | undefined {
  return row.kind === 'agent' ? agentById.value.get(row.id) : undefined
}

/** FR-27 — the companion version to display, or `''` when there is nothing
 *  worth saying.
 *
 *  Returns a value ONLY when the companion disagrees with the daemon, because
 *  agreement is the expected state and rendering it on every row would bury
 *  the one case an operator needs to see. Absent (`undefined`) is deliberately
 *  silent rather than rendered as "unknown": it covers a pre-FR-27 agent, a
 *  host with no companion, and a failed probe alike, and guessing between them
 *  in a table cell would be worse than saying nothing. */
function companionSkew(row: DeviceRow): string {
  const cv = agentFor(row)?.companion_version
  return cv && cv !== row.version ? cv : ''
}
const clientById = computed(() => {
  const m = new Map<string, TunnelClient>()
  for (const c of tunnelClientStore.clients) m.set(c.id, c)
  return m
})
function clientFor(row: DeviceRow): TunnelClient | undefined {
  return row.kind === 'tunnel_client' ? clientById.value.get(row.id) : undefined
}

// Row-level presence rendering (DeviceRow carries presence directly).
function rowPresenceColor(r: DeviceRow): string {
  return r.presence === 'online' ? 'success' : r.presence === 'stale' ? 'warning' : 'grey'
}
function rowPresenceIcon(r: DeviceRow): string {
  return r.presence === 'online'
    ? 'mdi-circle'
    : r.presence === 'stale'
      ? 'mdi-circle-half-full'
      : 'mdi-circle-outline'
}
function rowPresenceTitle(r: DeviceRow): string {
  if (r.kind === 'tunnel_client')
    return r.is_online ? 'Online (stored status)' : 'Offline (stored status)'
  return r.presence === 'online'
    ? 'Online — a control socket is registered'
    : r.presence === 'stale'
      ? 'Stale — heartbeat fresh but no socket registered'
      : 'Offline'
}
function rowStatusColor(r: DeviceRow): string {
  return rowPresenceColor(r)
}
function rowStatusLabel(r: DeviceRow): string {
  return r.presence
}

// ── Edit name & tags ─────────────────────────────────────────────
const editDialogOpen = ref(false)
const editTarget = ref<EditableDevice | null>(null)
function openEdit(row: DeviceRow) {
  editTarget.value = {
    kind: row.kind,
    id: row.id,
    name: row.name,
    display_name: row.display_name,
    tags: row.tags,
  }
  editDialogOpen.value = true
}
function onDeviceSaved(result: {
  id: string
  name: string
  display_name?: string
  tags: string[]
  dnsRenamed?: boolean
  dnsName?: string
}) {
  deviceStore.patchRow({
    id: result.id,
    name: result.name,
    display_name: result.display_name,
    tags: result.tags,
    ...(result.dnsName ? { magic_dns_name: result.dnsName } : {}),
  })
  // Refetch so sort/search reflect the new values server-side.
  fetchGrid()
}

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
    fetchGrid()
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
    fetchGrid()
  } finally {
    tunnelDeleting.value = false
  }
}

// Per-device consent mode. Email/Push route the request to the device OWNER
// (approve-link) for unattended hosts. `prompt_then_email` runs BOTH legs in
// parallel — first answer wins — with the on-host modal bounded to the attended
// window while the emailed link keeps the full async one; a host that goes
// unanswered hands over to the owner rather than ending the session.
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

/** FR-27 — do I own this device? Controlling your own device skips consent
 *  server-side, so the mode select above is inert for these rows unless
 *  `prompt_owner` is on. Only the owner sees the toggle: for anyone else the
 *  mode already applies, and an always-visible row explaining someone else's
 *  exemption would be noise. */
function isOwnedByMe(a: Agent): boolean {
  return !!auth.user?.id && a.owner_user_id === auth.user.id
}

/** The label says what is TRUE right now, not what the checkbox is called —
 *  "you own this device, so consent is currently skipped" is the fact an
 *  operator needs; "Ask me too" alone would leave them guessing what the
 *  unchecked state means. */
function ownerPromptLabel(a: Agent): string {
  const mode = a.access_policy.consent_mode ?? 'prompt'
  return a.access_policy.prompt_owner
    ? `Ask me too (you own this device; "${consentModeTitle(mode)}" applies to you)`
    : 'Ask me too (you own this device, so consent is skipped for you)'
}

function consentModeTitle(mode: ConsentMode): string {
  return CONSENT_MODE_ITEMS.find((i) => i.value === mode)?.title ?? mode
}

async function onPromptOwnerChange(a: Agent, prompt_owner: boolean) {
  if ((a.access_policy.prompt_owner ?? false) === prompt_owner) return
  consentBusy.value = a.id
  try {
    await agentStore.updateAccessPolicy(props.tenantId, a.id, {
      ...a.access_policy,
      prompt_owner,
    })
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    consentBusy.value = null
  }
}

// P6 — per-device multi-user input arbitration mode. `free` (default) =
// every INPUT-granted viewer injects (agent-fenced); `exclusive` = one
// floor holder with request/grant. Applies to NEW sessions.
const INPUT_MODE_ITEMS: { title: string; value: 'free' | 'exclusive' }[] = [
  { title: 'Free-for-all (fenced)', value: 'free' },
  { title: 'Exclusive (one controller)', value: 'exclusive' },
]
async function onInputModeChange(a: Agent, mode: 'free' | 'exclusive') {
  if ((a.access_policy.input_mode ?? 'free') === mode) return
  consentBusy.value = a.id
  try {
    await agentStore.updateAccessPolicy(props.tenantId, a.id, {
      ...a.access_policy,
      input_mode: mode,
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

// FR-40 — overlay-key rotation (docs/fr/FR-40-overlay-key-rotation.md)
const rotateKeyDialogOpen = ref(false)
const rotateKeyTarget = ref<Agent | null>(null)
const rotateKeyBusy = ref<string | null>(null)

function openRotateKey(a: Agent) {
  rotateKeyTarget.value = a
  rotateKeyDialogOpen.value = true
}

async function doRotateKey() {
  const a = rotateKeyTarget.value
  if (!a) return
  rotateKeyBusy.value = a.id
  try {
    const res = await agentStore.rotateOverlayKey(props.tenantId, a.id)
    updateNotice.value = res.delivered
      ? `Key rotation ordered on ${a.name} — it mints a new key, re-joins the mesh and reports back within seconds.`
      : `${a.name} is offline — the key rotation is queued and runs on its next connect.`
    rotateKeyDialogOpen.value = false
    await agentStore.fetchAgents(props.tenantId)
    fetchGrid()
  } catch (e) {
    agentStore.error = (e as Error).message
  } finally {
    rotateKeyBusy.value = null
  }
}

/** One chip per resolved state. The states are the SERVER's resolution
 *  (`key_rotation_view`) — the client only names them. */
function keyRotationChip(
  a: Agent,
): { color: string; icon: string; text: string; tooltip: string } | null {
  const kr = a.key_rotation
  if (!kr) return null
  switch (kr.state) {
    case 'queued':
      return {
        color: 'info',
        icon: 'mdi-key-chain',
        text: 'key rotation queued',
        tooltip: 'Ordered while the device was offline; it rotates on its next connect.',
      }
    case 'delivered':
      return {
        color: 'info',
        icon: 'mdi-key-chain',
        text: 'rotating key…',
        tooltip: 'The order reached the device; waiting for its answer.',
      }
    case 'rotating':
      return {
        color: 'info',
        icon: 'mdi-key-chain',
        text: 'rotating key…',
        tooltip: 'The device minted its new key and is re-joining the mesh.',
      }
    case 'rotated':
      return {
        color: 'success',
        icon: 'mdi-key-change',
        text: `key rotated · e${kr.report?.key_epoch ?? '?'}`,
        tooltip: `Rotated ${kr.report?.reported_at ?? ''} and joined the mesh under the new key.`,
      }
    case 'reported_not_joined':
      return {
        color: 'error',
        icon: 'mdi-key-alert',
        text: 'key not re-joined',
        tooltip:
          'The device reported a rotation but has not joined the mesh under the new key — read its log.',
      }
    case 'refused':
      return {
        color: 'warning',
        icon: 'mdi-hand-back-left-outline',
        text: `key rotation refused (${kr.report?.outcome ?? 'refused'})`,
        tooltip: kr.report?.detail ?? 'The device refused the rotation order.',
      }
    case 'failed':
      return {
        color: 'error',
        icon: 'mdi-alert-circle-outline',
        text: 'key rotation failed',
        tooltip:
          kr.report?.detail ??
          'The device could not mint or save a new key; its identity is unchanged.',
      }
    case 'unsupported':
      return {
        color: 'grey',
        icon: 'mdi-update',
        text: 'agent too old for key rotation',
        tooltip:
          "This device's agent predates key rotation (0.4.25+). Update it — the order stays queued.",
      }
    default:
      return null
  }
}

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

// Fleet RPC — the device console and its policy editor. Same fetch-on-open
// pattern as the dialogs above; the console additionally reads the ORG
// kill-switch so it can explain a refusal before the operator types anything.
const consoleDialogOpen = ref(false)
const consoleTarget = ref<Agent | null>(null)

function openConsole(a: Agent) {
  consoleTarget.value = a
  consoleDialogOpen.value = true
}

const execPolicyDialogOpen = ref(false)
const execPolicyTarget = ref<Agent | null>(null)

function openExecPolicy(a: Agent) {
  execPolicyTarget.value = a
  execPolicyDialogOpen.value = true
}

const sshPolicyDialogOpen = ref(false)
const sshPolicyTarget = ref<Agent | null>(null)

function openSshPolicy(a: Agent) {
  sshPolicyTarget.value = a
  sshPolicyDialogOpen.value = true
}

const remoteConfigDialogOpen = ref(false)
const remoteConfigTarget = ref<Agent | null>(null)

function openRemoteConfig(a: Agent) {
  remoteConfigTarget.value = a
  remoteConfigDialogOpen.value = true
}

/** The row badge for a device with a pending / refused / failed config
 *  request. `null` for `applied` and for devices nobody is managing — a
 *  steady-state chip on every row is noise that trains people to stop reading
 *  the row, and the states worth interrupting for are the ones that need
 *  someone to do something. */
function remoteConfigChip(
  a: Agent,
): { color: string; icon: string; text: string; tooltip: string } | null {
  const state = a.remote_config?.state
  switch (state) {
    case 'needs_restart':
      return {
        color: 'warning',
        icon: 'mdi-restart-alert',
        text: 'restart needed',
        tooltip: 'Config saved on the device but not yet in effect.',
      }
    case 'refused':
      return {
        color: 'warning',
        icon: 'mdi-hand-back-left-outline',
        text: 'config refused',
        tooltip:
          a.remote_config?.report?.outcome === 'not_primary'
            ? 'This org is a secondary on that host; only its primary enrollment may change machine-wide config.'
            : 'The device has not opted in to remote configuration.',
      }
    case 'failed':
      return {
        color: 'error',
        icon: 'mdi-alert-circle-outline',
        text: 'config failed',
        tooltip: a.remote_config?.report?.detail ?? 'The device could not write its config.',
      }
    case 'pending':
      return {
        color: 'info',
        icon: 'mdi-clock-outline',
        text: 'config pending',
        tooltip: 'Waiting for the device to reconnect and reconcile.',
      }
    case 'reports_unsupported':
    case 'push_unsupported':
      return {
        color: 'grey',
        icon: 'mdi-update',
        text: 'agent too old',
        tooltip:
          state === 'push_unsupported'
            ? 'This agent predates remote configuration; nothing is sent to it.'
            : 'This agent applies pushed config but cannot report what it did.',
      }
    default:
      return null
  }
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

// FR-12 — `/devices?enroll=1` opens this dialog, so a tutorial step can land
// on the real thing instead of describing a button. The param is stripped
// immediately: opening MINTS a single-use enrollment token, and a bookmarked
// or refreshed URL must not keep minting them.
watch(
  () => route.query.enroll,
  (v) => {
    if (!v) return
    const { enroll: _dropped, ...rest } = route.query
    void router.replace({ query: rest })
    void openEnrollDialog()
  },
  { immediate: true },
)

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
 * paste it into `roomler forward --agent <hex>` (or any other
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
    fetchGrid()
  } finally {
    deleting.value = false
  }
}

// Owner reassignment (MANAGE_AGENTS). Resolve owner_user_id → a name via the
// tenant members, and pick a new owner from that list.
// Multi-org — "Add to another organization". The target list comes from the
// server (orgs where this caller holds MANAGE_AGENTS), so the picker can
// never offer a choice that 403s.
const joinOrgDialogOpen = ref(false)
const joinOrgTarget = ref<Agent | null>(null)
const joinOrgTargetTenant = ref<string>('')
const joinOrgOverlayMode = ref<string>('off')
const joinOrgBusy = ref(false)
const joinOrgLoading = ref(false)
const joinOrgLoaded = ref(false)
const joinOrgSupported = ref(true)
const joinOrgOnline = ref(true)
// Whether picking "Join the mesh" gets a mesh now or after a restart. Only
// consulted when `tun` is selected — an `off` join never touches the TUN.
const joinOrgMeshReady = ref(true)
const joinOrgError = ref('')
const joinOrgItems = ref<
  Array<{ tenant_id: string; name: string; slug: string; already_enrolled: boolean }>
>([])
const joinOrgSelectItems = computed(() =>
  joinOrgItems.value.map((o) => ({
    title: o.already_enrolled ? `${o.name} — already added` : o.name,
    value: o.tenant_id,
    props: { disabled: o.already_enrolled },
  })),
)
async function openJoinOrg(a: Agent) {
  joinOrgTarget.value = a
  joinOrgTargetTenant.value = ''
  joinOrgOverlayMode.value = 'off'
  joinOrgError.value = ''
  joinOrgItems.value = []
  joinOrgLoaded.value = false
  joinOrgDialogOpen.value = true
  joinOrgLoading.value = true
  try {
    const res = await agentStore.fetchJoinTargets(props.tenantId, a.id)
    joinOrgItems.value = res.items
    joinOrgSupported.value = res.supported
    joinOrgOnline.value = res.online
    // Older servers omit the field; assume ready so we never invent a
    // restart warning for a device that doesn't need one.
    joinOrgMeshReady.value = res.mesh_ready !== false
  } catch (e) {
    joinOrgError.value = (e as Error).message
  } finally {
    joinOrgLoading.value = false
    joinOrgLoaded.value = true
  }
}
async function confirmJoinOrg() {
  if (!joinOrgTarget.value || !joinOrgTargetTenant.value) return
  joinOrgBusy.value = true
  joinOrgError.value = ''
  try {
    const res = await agentStore.joinOrg(
      props.tenantId,
      joinOrgTarget.value.id,
      joinOrgTargetTenant.value,
      { overlayMode: joinOrgOverlayMode.value },
    )
    joinOrgDialogOpen.value = false
    // The device enrolls itself a beat later; say what actually happened
    // rather than implying the row is already there.
    const base = res.already_enrolled
      ? `${joinOrgTarget.value.name} was already in that organization — its enrollment was refreshed.`
      : `${joinOrgTarget.value.name} is joining as "${res.label}". It appears in that organization's device list shortly.`
    // Say the quiet part: the enrollment landed, the MESH did not.
    updateNotice.value = res.restart_required
      ? `${base} Mesh access is configured but starts after the device's agent restarts — the next auto-update delivers that.`
      : base
  } catch (e) {
    joinOrgError.value = (e as Error).message
  } finally {
    joinOrgBusy.value = false
  }
}

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

/** The two network-owned stores, loaded only where the server mounts `network`.
 *
 *  ⚠️ The TEMPLATE has always been gated (`v-if="caps.has('network')"`), but
 *  these fetches were not — so on a `remote` profile the devices page fired
 *  `/tenant/{id}/tunnel-client` and `/tenant/{id}/overlay-node` at a server
 *  that correctly does not mount them, collecting four 404s and four console
 *  errors on every visit. The server was right; the client was asking for
 *  doors it had already decided not to draw. FR-69 P9's rule is that the SPA
 *  gates on `/api/capabilities` — that has to cover what it FETCHES, not only
 *  what it renders. (Found by the FR-75 profile matrix, #1447.) */
function loadNetworkStores(tid: string) {
  if (!caps.has('network')) return
  overlayStore.fetchNodes(tid)
  tunnelClientStore.fetchTunnelClients(tid)
}

onMounted(() => {
  // The GRID's own fetch fires from @update:options on mount — these load
  // the rich per-device stores the action menus and mobile cards read.
  agentStore.fetchAgents(props.tenantId)
  agentStore.fetchTenantMembers(props.tenantId)
  loadNetworkStores(props.tenantId)
})

watch(() => props.tenantId, (tid) => {
  if (tid) {
    agentStore.fetchAgents(tid)
    loadNetworkStores(tid)
    // Reset the grid to a clean first page for the new org.
    gridSearch.value = ''
    if (gridPage.value !== 1) gridPage.value = 1
    else fetchGrid()
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

/* Never squeeze the columns into the viewport — every cell keeps its
   natural width (max two lines per cell: e.g. name over id·version,
   consent over input mode) and the WRAPPER scrolls horizontally.
   (House rule: wide tables scroll in their own container.) */
.agents-table :deep(.v-table__wrapper) {
  overflow-x: auto;
}
.agents-table :deep(table) {
  width: max-content;
  min-width: 100%;
}
.agents-table :deep(th),
.agents-table :deep(td) {
  white-space: nowrap;
}

/* The two stacked selects (consent / input mode) need room to render
   their labels on one line each instead of clipping. */
.agents-table :deep(.consent-select) {
  min-width: 170px;
}

/* FR-27 — the companion-version skew note. Warning-toned so it reads as
   "something here is behind", not as a second neutral fact next to the
   daemon version. */
.companion-skew {
  color: rgb(var(--v-theme-warning));
}
</style>
