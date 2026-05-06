<template>
  <v-container fluid class="pa-0 remote-control-wrapper">
    <!-- ============================================================
         Toolbar Row 1: primary controls — Back / Title / status /
         Ctrl-Alt-Del / Connect-Disconnect / Fullscreen. Designed to
         fit on a single line at every viewport (down to ~320px) so
         the user never loses access to the primary actions. The
         five session-time tools that previously crowded this row
         (Quality / Scale / Resolution / Codec, Crystal-Clear,
         Low-Latency, clipboard send/get, file upload) moved to
         Row 2 below; on `<md` Row 2 collapses into the bottom-sheet.
         ============================================================ -->
    <v-toolbar density="compact" color="surface" class="px-2 rc-toolbar-primary">
      <v-btn
        icon="mdi-arrow-left"
        variant="text"
        :to="{ name: 'admin-agents', params: { tenantId } }"
        aria-label="Back to Agents"
      />
      <v-toolbar-title class="d-flex align-center text-truncate">
        <v-icon :color="statusColor" size="small" class="mr-2 flex-shrink-0">
          mdi-circle
        </v-icon>
        <span class="text-truncate">{{ agent?.name || 'Agent' }}</span>
        <!-- OS + version subtitle hidden on phone-sized viewports;
             they're useful context on a desktop but on mobile they
             push Connect/Disconnect off the right edge. -->
        <span v-if="agent" class="text-caption text-medium-emphasis ml-2 d-none d-sm-inline">
          {{ agent.os }} · {{ agent.agent_version || '—' }}
        </span>
      </v-toolbar-title>
      <v-spacer />
      <v-chip
        v-if="rc.phase.value !== 'idle'"
        size="small"
        :color="phaseColor"
        variant="flat"
        class="mr-2"
      >
        <template v-if="rc.phase.value === 'reconnecting'">
          Reconnecting ({{ rc.reconnectAttempt.value }}/{{ RC_RECONNECT_LADDER_MS.length }})…
        </template>
        <template v-else>{{ rc.phase.value }}</template>
      </v-chip>
      <!-- Host-locked indicator. Shown only during a live session
           when the agent has signalled (via the rc:host_locked
           control-DC message, agent 0.2.2+) that the input desktop
           transitioned to winsta0\Winlogon. The video stream's
           padlock overlay frame is the primary signal; this badge
           supplements it for operators who scrolled the video out
           of view or are taking a screenshot for support. Older
           agents (<0.2.2) never emit the message, so the chip
           stays hidden and the experience falls back to the
           overlay-only state. -->
      <v-chip
        v-if="rc.phase.value === 'connected' && rc.hostLocked.value"
        size="small"
        color="warning"
        variant="flat"
        prepend-icon="mdi-lock"
        class="mr-2"
        title="The remote host is on the lock screen — input is suppressed."
      >
        Host locked
      </v-chip>
      <!-- Secondary chip for the SYSTEM-context worker's input-desktop
           name (agents 0.3.0+). 'Default' = normal user desktop, no chip
           shown. 'Winlogon' / 'Screen-saver' / etc. = secure desktop, the
           operator is driving lock-screen / UAC / SAS UI. The hostLocked
           chip above and this one render side-by-side: hostLocked is the
           pre-0.3.0 binary lock signal, currentDesktop is the per-
           transition name from the SYSTEM-context path. They're not
           contradictory — on 0.3.0 perMachine MSI both fire. -->
      <v-chip
        v-if="rc.phase.value === 'connected' && rc.currentDesktop.value !== 'Default'"
        size="small"
        color="info"
        variant="flat"
        prepend-icon="mdi-shield-account"
        class="mr-2"
        :title="`Agent thread is bound to ${rc.currentDesktop.value} — type the host's password to unlock`"
      >
        On {{ rc.currentDesktop.value }}
      </v-chip>
      <!-- Mobile-only Row-2 trigger: opens the bottom-sheet that
           holds Row 2's controls (Quality / Scale / Resolution /
           Codec / Crystal-Clear / Low-Latency / clipboard / upload).
           Hidden on `md` and above where Row 2 renders inline. -->
      <v-btn
        icon="mdi-tune-variant"
        variant="text"
        size="small"
        class="d-md-none mr-1"
        aria-label="Open viewer settings"
        title="Viewer settings"
        @click="mobileSettingsOpen = true"
      />
      <!-- Ctrl+Alt+Del: the OS intercepts this key combo before the
           browser sees it, so expose an explicit toolbar button that
           emits the equivalent key sequence over the input DC.
           Visible at every viewport during a session — emergency-
           recovery action, must not hide behind an extra tap. -->
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        class="mr-1"
        aria-label="Send Ctrl+Alt+Del to remote"
        title="Send Ctrl+Alt+Del"
        @click="rc.sendCtrlAltDel()"
      >
        <v-icon>mdi-keyboard-outline</v-icon>
      </v-btn>
      <v-btn
        v-if="rc.phase.value === 'idle' || rc.phase.value === 'closed' || rc.phase.value === 'error'"
        color="primary"
        variant="flat"
        prepend-icon="mdi-play"
        :disabled="!canConnect"
        @click="startSession"
      >
        Connect
      </v-btn>
      <v-btn
        v-else
        color="error"
        variant="flat"
        prepend-icon="mdi-stop"
        @click="rc.disconnect()"
      >
        Disconnect
      </v-btn>
      <!-- Fullscreen: gated on (a) connected session AND (b) the
           document supports the Fullscreen API. iOS Safari only
           supports `webkitEnterFullscreen` on `<video>` elements,
           and won't show overlay canvases (cursor / stats), so we
           hide the button on iOS rather than pretend it works.
           ESC exits natively. -->
      <v-btn
        v-if="rc.phase.value === 'connected' && fullscreenEnabled"
        icon
        variant="text"
        size="small"
        class="ml-1"
        :aria-label="isFullscreen ? 'Exit fullscreen' : 'Enter fullscreen'"
        :title="isFullscreen ? 'Exit fullscreen (Esc)' : 'Fullscreen'"
        @click="toggleFullscreen"
      >
        <v-icon>{{ isFullscreen ? 'mdi-fullscreen-exit' : 'mdi-fullscreen' }}</v-icon>
      </v-btn>
    </v-toolbar>

    <!-- ============================================================
         Toolbar Row 2: session controls. Visible inline on `md+`,
         collapsed into the bottom-sheet on `<md`. `flex-wrap: wrap`
         lets it spill to a second row on borderline viewports
         (~960-1100px) instead of pushing controls off-screen.
         ============================================================ -->
    <div class="rc-tools-row d-none d-md-flex align-center flex-wrap ga-2 px-2 py-1">
      <v-select
        v-model="quality"
        :items="qualityOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 140px;"
        prepend-inner-icon="mdi-quality-high"
        aria-label="Quality preference"
      />
      <v-select
        v-model="scaleMode"
        :items="scaleOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 160px;"
        prepend-inner-icon="mdi-image-size-select-actual"
        aria-label="View scale"
      />
      <v-text-field
        v-if="scaleMode === 'custom'"
        v-model.number="scalePercent"
        type="number"
        min="5"
        max="1000"
        step="5"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 110px;"
        suffix="%"
        aria-label="Custom scale percent"
      />
      <v-select
        v-model="resolutionPresetValue"
        :items="resolutionOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 190px;"
        prepend-inner-icon="mdi-monitor-screenshot"
        aria-label="Remote capture resolution"
        :title="resolutionButtonTitle"
      />
      <v-select
        v-model="codecOverride"
        :items="codecOptions"
        density="compact"
        hide-details
        variant="outlined"
        style="max-width: 160px;"
        prepend-inner-icon="mdi-video-outline"
        aria-label="Codec override"
        :title="codecOverride == null
          ? 'Agent picks the best available codec'
          : `Forcing ${codecOverride.toUpperCase()} — takes effect on next Connect`"
      />
      <!-- Crystal-clear (VP9 4:4:4) toggle. Persists across reloads;
           takes effect on next Connect. Disabled when the browser
           lacks WebCodecs VideoDecoder vp09.01.10.08. -->
      <v-btn
        icon
        variant="text"
        size="small"
        :color="vp9_444On ? 'primary' : undefined"
        :disabled="!rc.vp9_444Supported.value"
        :aria-label="vp9_444On ? 'Disable crystal-clear (VP9 4:4:4) path' : 'Enable crystal-clear (VP9 4:4:4) path'"
        :title="vp9_444Tooltip"
        @click="toggleVp9_444"
      >
        <v-icon>{{ vp9_444On ? 'mdi-format-color-fill' : 'mdi-format-color-marker-cancel' }}</v-icon>
      </v-btn>
      <!-- Low-latency (WebCodecs) toggle. Bypasses Chrome's <video>
           jitter-buffer floor (~80ms). Disabled when the browser
           lacks RTCRtpScriptTransform / VideoDecoder. -->
      <v-btn
        icon
        variant="text"
        size="small"
        :color="webcodecsOn ? 'primary' : undefined"
        :disabled="!rc.webcodecsSupported.value"
        :aria-label="webcodecsOn ? 'Disable low-latency path' : 'Enable low-latency (WebCodecs) path'"
        :title="webcodecsTooltip"
        @click="toggleWebCodecs"
      >
        <v-icon>{{ webcodecsOn ? 'mdi-flash' : 'mdi-flash-outline' }}</v-icon>
      </v-btn>
      <!-- Connected-only tools: clipboard send/get + file upload.
           Both DCs only open while connected, so the buttons are
           hidden otherwise. -->
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        :loading="clipboardBusy"
        aria-label="Send my clipboard to the remote host"
        title="Send my clipboard → remote"
        @click="onSendClipboard"
      >
        <v-icon>mdi-content-paste</v-icon>
      </v-btn>
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        :loading="clipboardBusy"
        aria-label="Get the remote host's clipboard"
        title="Get remote clipboard → me"
        @click="onGetClipboard"
      >
        <v-icon>mdi-content-copy</v-icon>
      </v-btn>
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        :loading="uploadBusy"
        aria-label="Upload a file to the remote host"
        title="Upload file → remote"
        @click="fileInput?.click()"
      >
        <v-icon>mdi-upload</v-icon>
      </v-btn>
      <v-btn
        v-if="rc.phase.value === 'connected'"
        icon
        variant="text"
        size="small"
        :disabled="!agentSupportsBrowse"
        aria-label="Browse files on the remote host"
        :title="
          agentSupportsBrowse
            ? 'Browse remote files (download)'
            : isLegacyFileDc
              ? 'Browse needs agent 0.3.0+ — upgrade host agent'
              : 'Browse disabled by host config'
        "
        @click="filesDrawer = !filesDrawer"
      >
        <v-icon>mdi-folder-open</v-icon>
      </v-btn>
    </div>
    <!-- File input (hidden); shared between Row 2 inline upload
         button and the bottom-sheet upload button. `multiple`
         lets the operator queue several files in one picker
         dialog (Phase 1 of file-DC v2). -->
    <input
      ref="fileInput"
      type="file"
      multiple
      style="display: none"
      @change="onFilePicked"
    />

    <!-- Viewer. Wrapped in a Material-elevation v-card so the
         render pane reads as a window into another machine —
         distinct from the host UI's toolbar/page chrome. The card
         is OUTSIDE the fullscreen target (`.video-frame`), so it
         disappears in fullscreen automatically (operator wants
         edge-to-edge pixels in that mode). -->
    <v-card
      variant="elevated"
      elevation="2"
      rounded="lg"
      border
      class="ma-2 ma-md-3 remote-stage-card flex-grow-1 d-flex"
    >
    <div class="remote-stage">
      <v-alert
        v-if="rc.error.value"
        type="error"
        variant="tonal"
        class="ma-4"
        closable
        @click:close="rc.error.value = null"
      >
        {{ rc.error.value }}
      </v-alert>

      <div v-if="rc.phase.value === 'idle' || rc.phase.value === 'closed'" class="empty-state">
        <v-icon size="96" color="grey-lighten-1">mdi-desktop-classic</v-icon>
        <p class="text-body-1 mt-2">
          Click <strong>Connect</strong> to start a remote-control session.
        </p>
        <p v-if="agent && !agent.is_online" class="text-caption text-medium-emphasis">
          This agent is currently offline. The session will fail until the agent
          reconnects.
        </p>
      </div>

      <div
        v-else-if="['requesting', 'awaiting_consent', 'negotiating'].includes(rc.phase.value)"
        class="empty-state"
      >
        <v-progress-circular indeterminate size="64" />
        <p class="text-body-1 mt-4">{{ phaseLabel }}</p>
      </div>

      <div
        v-else-if="rc.phase.value === 'connected'"
        ref="stageEl"
        class="video-frame"
        :class="[`scale-${rc.scaleMode.value}`, { 'drag-over': isDragOver }]"
        tabindex="0"
        @pointermove="onStagePointerMove"
        @pointerleave="cursorVisible = false"
        @pointerenter="cursorVisible = true"
        @dragenter.prevent.stop="onStageDragEnter"
        @dragover.prevent.stop="onStageDragOver"
        @dragleave.prevent.stop="onStageDragLeave"
        @drop.prevent.stop="onStageDrop"
      >
        <!-- Classic render path: <video> bound to the remote MediaStream.
             Used unless the viewer opted into the WebCodecs path AND
             the browser supports it. We still render the <video>
             element in WebCodecs mode but hide it — input + cursor
             math hang off `rc.mediaIntrinsicW/H` which the composable
             keeps in sync either way. -->
        <video
          v-show="!isWebCodecsRender && !isVp9_444Render"
          ref="videoEl"
          autoplay
          playsinline
          muted
          class="remote-video"
          :class="`scale-${rc.scaleMode.value}`"
          :style="videoScaleStyle"
        />
        <!-- Low-latency render path: canvas fed by the Worker-driven
             VideoDecoder. transferControlToOffscreen() happens once
             per session in the composable; this element is the main-
             thread handle we bind the canvas ref on. Same scale
             classes + style as the video so the existing layout +
             cursor overlays keep working. -->
        <canvas
          v-if="isWebCodecsRender"
          :ref="bindWebcodecsCanvas"
          class="remote-video webcodecs-canvas"
          :class="`scale-${rc.scaleMode.value}`"
          :style="videoScaleStyle"
        />
        <!-- Phase Y.4 render path: canvas fed by the VP9-444 worker
             over a `video-bytes` DataChannel (no WebRTC video track,
             no RTCRtpScriptTransform). Mounts when the composable's
             `vp9_444Active` flag flips true (DC opened + worker
             initialised). The composable's watcher transfers control
             of this canvas to the worker via `transferControlToOffscreen`,
             replacing the synthetic OffscreenCanvas it started with.
             Same scale classes + style as the video for layout
             parity. -->
        <canvas
          v-if="isVp9_444Render"
          :ref="bindVp9_444Canvas"
          class="remote-video vp9-444-canvas"
          :class="`scale-${rc.scaleMode.value}`"
          :style="videoScaleStyle"
        />
        <!-- Live stats readout: codec + bitrate + fps. Populated from
             RTCPeerConnection.getStats() every 500 ms inside the
             composable. Pill format keeps it unobtrusive over the
             video content. Hidden until at least the codec is known. -->
        <div
          v-if="rc.hasMedia.value && statsCodecLabel"
          class="stats-readout"
          role="status"
          aria-live="polite"
        >
          <span class="stats-pill">{{ statsCodecLabel }}</span>
          <span class="stats-pill">{{ statsBitrateLabel }}</span>
          <span class="stats-pill">{{ statsFpsLabel }}</span>
        </div>
        <div v-if="!rc.hasMedia.value" class="no-media-overlay">
          <v-icon size="72" color="grey-lighten-1">mdi-video-off</v-icon>
          <p class="text-body-1 mt-3">Connected — waiting for agent to publish a video track.</p>
          <p class="text-caption text-medium-emphasis mt-1">
            The agent needs to be built with the media feature
            (<code>--features media</code>) to send video.
            Input events flow as soon as the input channel is open.
          </p>
        </div>
        <!-- Remote cursor overlay: canvas painted with the real OS
             cursor bitmap received over the `cursor` data channel
             (1E.3). Position is translated from agent-source pixels
             into viewer-local pixels using the same letterbox
             correction the input coords use. If no shape bitmap has
             arrived yet, fall back to the initials badge. -->
        <canvas
          v-if="remoteCursorVisible"
          ref="cursorCanvas"
          class="remote-cursor-canvas"
          :width="remoteCursorSize.w"
          :height="remoteCursorSize.h"
          :style="{ transform: `translate(${remoteCursorX}px, ${remoteCursorY}px)` }"
        />
        <!-- Synthetic cursor with the controller's initials. Hidden
             native cursor over the surface (cursor: none) so this is
             the only pointer indicator; floats at the last
             pointermove position. Shows when the remote cursor
             hasn't advertised yet or to mark additional controllers
             in multi-watcher sessions. -->
        <div
          v-if="!remoteCursorVisible && cursorVisible && controllerInitials"
          class="cursor-badge"
          :style="{ transform: `translate(${cursorX}px, ${cursorY}px)` }"
        >
          <div class="cursor-arrow" />
          <div class="cursor-chip">{{ controllerInitials }}</div>
        </div>
      </div>
    </div>
    </v-card>
    <!-- Custom-resolution dialog. Opened when the operator picks the
         "Custom…" option in the Resolution dropdown; submits an
         rc:resolution {mode:'custom'} message on confirm. -->
    <v-dialog v-model="customResolutionDialog" max-width="480">
      <v-card>
        <v-card-title>Custom remote resolution</v-card-title>
        <v-card-text>
          <div class="d-flex align-center mb-3">
            <v-text-field
              v-model.number="customResolutionW"
              type="number"
              min="160"
              max="7680"
              step="10"
              density="compact"
              hide-details
              variant="outlined"
              label="Width"
              class="mr-2"
            />
            <span class="text-medium-emphasis mr-2">×</span>
            <v-text-field
              v-model.number="customResolutionH"
              type="number"
              min="120"
              max="4320"
              step="10"
              density="compact"
              hide-details
              variant="outlined"
              label="Height"
            />
          </div>
          <v-chip-group column>
            <v-chip
              v-for="p in customResolutionPresets"
              :key="`${p.w}x${p.h}`"
              size="small"
              variant="outlined"
              @click="pickCustomResolutionPreset(p.w, p.h)"
            >
              {{ p.w }} × {{ p.h }}{{ p.note ? ` — ${p.note}` : '' }}
            </v-chip>
          </v-chip-group>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="customResolutionDialog = false">Cancel</v-btn>
          <v-btn
            color="primary"
            variant="flat"
            :disabled="!customResolutionValid"
            @click="confirmCustomResolution"
          >
            Apply
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>

    <!-- Mobile settings bottom-sheet — holds Row 2's controls on `<md`
         viewports where the inline row is hidden. Includes the
         session-config toggles (Crystal-Clear, Low-Latency) and
         the connected-only tools (clipboard send/get, upload) so
         a phone operator never has to reach for a control that
         isn't on screen. Ctrl-Alt-Del stays in the toolbar above. -->
    <v-bottom-sheet v-model="mobileSettingsOpen" inset>
      <v-card>
        <v-card-title>Viewer settings</v-card-title>
        <v-card-text class="d-flex flex-column ga-3">
          <v-select
            v-model="quality"
            :items="qualityOptions"
            density="compact"
            hide-details
            variant="outlined"
            prepend-inner-icon="mdi-quality-high"
            label="Quality preference"
          />
          <v-select
            v-model="scaleMode"
            :items="scaleOptions"
            density="compact"
            hide-details
            variant="outlined"
            prepend-inner-icon="mdi-image-size-select-actual"
            label="View scale"
          />
          <v-text-field
            v-if="scaleMode === 'custom'"
            v-model.number="scalePercent"
            type="number"
            min="5"
            max="1000"
            step="5"
            density="compact"
            hide-details
            variant="outlined"
            suffix="%"
            label="Custom scale"
          />
          <v-select
            v-model="resolutionPresetValue"
            :items="resolutionOptions"
            density="compact"
            hide-details
            variant="outlined"
            prepend-inner-icon="mdi-monitor-screenshot"
            label="Remote capture resolution"
          />
          <v-select
            v-model="codecOverride"
            :items="codecOptions"
            density="compact"
            hide-details
            variant="outlined"
            prepend-inner-icon="mdi-video-outline"
            label="Codec override"
            :hint="codecOverride == null
              ? 'Agent picks the best available codec'
              : `Forcing ${codecOverride.toUpperCase()} - takes effect on next Connect`"
            persistent-hint
          />
          <v-divider />
          <!-- Session-config toggles. Side-by-side on phone since
               they're each one tap and the icons make the meaning
               obvious; the title on each is the full description. -->
          <div class="d-flex ga-2 align-center">
            <v-btn
              variant="tonal"
              :color="vp9_444On ? 'primary' : undefined"
              :disabled="!rc.vp9_444Supported.value"
              prepend-icon="mdi-format-color-fill"
              :title="vp9_444Tooltip"
              class="flex-grow-1"
              @click="toggleVp9_444"
            >
              Crystal-Clear {{ vp9_444On ? 'ON' : 'OFF' }}
            </v-btn>
            <v-btn
              variant="tonal"
              :color="webcodecsOn ? 'primary' : undefined"
              :disabled="!rc.webcodecsSupported.value"
              prepend-icon="mdi-flash"
              :title="webcodecsTooltip"
              class="flex-grow-1"
              @click="toggleWebCodecs"
            >
              Low-Latency {{ webcodecsOn ? 'ON' : 'OFF' }}
            </v-btn>
          </div>
          <!-- Connected-only tools. Hidden when no session is live —
               the underlying DCs are closed so the actions would
               silently fail. Each is a full-width button so it's
               easy to tap on a phone. -->
          <template v-if="rc.phase.value === 'connected'">
            <v-divider />
            <v-btn
              variant="tonal"
              prepend-icon="mdi-content-paste"
              :loading="clipboardBusy"
              @click="onSendClipboard"
            >
              Send my clipboard → remote
            </v-btn>
            <v-btn
              variant="tonal"
              prepend-icon="mdi-content-copy"
              :loading="clipboardBusy"
              @click="onGetClipboard"
            >
              Get remote clipboard → me
            </v-btn>
            <v-btn
              variant="tonal"
              prepend-icon="mdi-upload"
              :loading="uploadBusy"
              @click="fileInput?.click()"
            >
              Upload file → remote
            </v-btn>
          </template>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="mobileSettingsOpen = false">Close</v-btn>
        </v-card-actions>
      </v-card>
    </v-bottom-sheet>

    <!-- Files browser drawer (Phase 3 of file-DC v2). Opens via the
         mdi-folder-open toolbar button. Lets the operator navigate
         the host's filesystem and download files. Folder download
         lights up in Phase 4. Multi-select via checkboxes; Ctrl+C
         to copy-as-download (Phase 5). -->
    <v-navigation-drawer
      v-model="filesDrawer"
      location="right"
      width="420"
      temporary
      class="files-drawer"
      tabindex="0"
      @paste="onDrawerPaste"
      @keydown="onDrawerKeyDown"
    >
      <v-toolbar density="compact" color="primary">
        <v-icon class="ml-4">mdi-folder-open</v-icon>
        <v-toolbar-title>Remote files</v-toolbar-title>
        <v-spacer />
        <v-btn icon variant="text" :disabled="dirLoading" title="Refresh" @click="navigateTo(currentDirPath)">
          <v-icon>mdi-refresh</v-icon>
        </v-btn>
        <v-btn icon variant="text" title="Close" @click="filesDrawer = false">
          <v-icon>mdi-close</v-icon>
        </v-btn>
      </v-toolbar>
      <div class="px-3 pt-2 pb-1 d-flex align-center" style="gap: 4px">
        <v-btn
          icon
          variant="text"
          size="small"
          :disabled="!currentParent || dirLoading"
          title="Parent directory"
          @click="navigateTo(currentParent || '')"
        >
          <v-icon>mdi-arrow-up</v-icon>
        </v-btn>
        <v-btn
          icon
          variant="text"
          size="small"
          :disabled="dirLoading"
          title="Drives / roots"
          @click="navigateTo('')"
        >
          <v-icon>mdi-monitor</v-icon>
        </v-btn>
        <v-text-field
          v-model="dirPathInput"
          density="compact"
          hide-details
          variant="outlined"
          placeholder="Path (Enter to go)"
          @keyup.enter="navigateTo(dirPathInput)"
        />
      </div>
      <v-divider />
      <div v-if="dirError" class="pa-3 text-error text-caption">
        {{ dirError }}
      </div>
      <v-progress-linear v-if="dirLoading" indeterminate />
      <v-list density="compact" class="pa-0">
        <v-list-item
          v-for="entry in dirEntries"
          :key="entry.name"
          :class="{ 'files-entry-selected': selectedDirEntries.has(entry.name) }"
          @click="onEntryClick(entry, $event)"
          @dblclick="onEntryDblClick(entry)"
        >
          <template #prepend>
            <v-icon :color="entry.is_dir ? 'amber-darken-2' : 'grey-darken-1'">
              {{ entry.is_dir ? 'mdi-folder' : 'mdi-file-outline' }}
            </v-icon>
          </template>
          <v-list-item-title>{{ entry.name }}</v-list-item-title>
          <v-list-item-subtitle v-if="!entry.is_dir">
            {{ formatFileSize(entry.size) }}
          </v-list-item-subtitle>
          <template #append>
            <v-btn
              icon
              size="x-small"
              variant="text"
              :disabled="entry.is_dir ? !agentSupportsFolderDownload : !agentSupportsDownload"
              :title="
                entry.is_dir
                  ? agentSupportsFolderDownload
                    ? `Download ${entry.name} as zip (Chrome/Edge only)`
                    : 'Folder download needs agent 0.3.0+'
                  : agentSupportsDownload
                    ? `Download ${entry.name}`
                    : 'Download needs agent 0.3.0+'
              "
              @click.stop="downloadEntry(entry)"
            >
              <v-icon>{{ entry.is_dir ? 'mdi-folder-zip' : 'mdi-download' }}</v-icon>
            </v-btn>
          </template>
        </v-list-item>
        <v-list-item v-if="!dirLoading && dirEntries.length === 0 && !dirError">
          <v-list-item-subtitle class="text-disabled">
            (empty directory)
          </v-list-item-subtitle>
        </v-list-item>
      </v-list>
    </v-navigation-drawer>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onBeforeUnmount, watch } from 'vue'
import { useRoute } from 'vue-router'
import { useAgentStore, type Agent } from '@/stores/agents'
import { useAuthStore } from '@/stores/auth'
import {
  useRemoteControl,
  RC_RECONNECT_LADDER_MS,
  type RcQuality,
  type RcPreferredCodec,
  type RcScaleMode,
  type RcResolutionSetting,
  type RcRenderPath,
  type RcVideoTransport,
} from '@/composables/useRemoteControl'
import { useSnackbar } from '@/composables/useSnackbar'

const route = useRoute()
const tenantId = computed(() => route.params.tenantId as string)
const agentId = computed(() => route.params.agentId as string)

const agentStore = useAgentStore()
const authStore = useAuthStore()
const agent = ref<Agent | null>(null)
const rc = useRemoteControl()
const { showSuccess, showError } = useSnackbar()
const clipboardBusy = ref(false)

// Push the controller's local clipboard to the agent's OS clipboard.
// Driven by a toolbar button so the `navigator.clipboard.readText()`
// call happens in a user-gesture context (Chrome throws otherwise).
// Short-lived busy spinner during the round-trip; toast on
// success/failure. Fire-and-forget — the agent doesn't ack writes.
async function onSendClipboard() {
  if (clipboardBusy.value) return
  clipboardBusy.value = true
  try {
    const ok = await rc.sendClipboardToAgent()
    if (ok) {
      showSuccess('Clipboard sent to remote')
    } else {
      showError('Could not read your clipboard (permission denied?)')
    }
  } finally {
    clipboardBusy.value = false
  }
}

// Pull the agent's clipboard text and copy it into the controller's
// local clipboard. The round-trip goes: button click → send
// `clipboard:read` on the DC → await `clipboard:content` → paste
// into `navigator.clipboard.writeText`. 5 s timeout inside the
// composable; we render the error as a snackbar.
async function onGetClipboard() {
  if (clipboardBusy.value) return
  clipboardBusy.value = true
  try {
    const text = await rc.getAgentClipboard()
    try {
      await globalThis.navigator.clipboard.writeText(text)
      showSuccess(`Copied remote clipboard (${text.length} chars)`)
    } catch (e) {
      showError(`Could not write to your clipboard: ${(e as Error).message}`)
    }
  } catch (e) {
    showError(`Remote clipboard read failed: ${(e as Error).message}`)
  } finally {
    clipboardBusy.value = false
  }
}

// Template refs. Declared before the computeds / watches below that
// reference them — Vue 3 <script setup> executes top-to-bottom, and
// `watch` evaluates its source getter eagerly during setup to wire
// reactivity. Reading `cursorCanvas.value` in a watch source while
// `cursorCanvas` is still in the temporal dead zone manifests as the
// minified TDZ crash "Cannot access 'Z' before initialization" at
// setup time, which kills the whole RemoteControl page before it
// can paint. Keep template refs at the top of setup to avoid this.
const videoEl = ref<HTMLVideoElement | null>(null)
const stageEl = ref<HTMLElement | null>(null)
const cursorCanvas = ref<HTMLCanvasElement | null>(null)
const fileInput = ref<HTMLInputElement | null>(null)
// Mobile-only viewer settings bottom-sheet visibility. Wraps the
// same Quality / Scale / Resolution / Codec selects shown inline on
// `md and up` so phone operators retain access without losing the
// rest of the toolbar to a 4-select overflow.
const mobileSettingsOpen = ref(false)
const uploadBusy = ref(false)
// Visual cue when a draggable item is hovering the stage. Toggled on
// dragenter/dragover (true) + drop/dragleave (false). The
// `.prevent.stop` modifiers on the v-on bindings are what actually
// suppress the browser's default open-image-in-new-tab; the ref
// just drives the dashed-border CSS state.
const isDragOver = ref(false)

// Drop a file onto the stage to upload it to the remote host.
// Browsers default to opening dragged images / files in a new tab —
// `preventDefault` on every drag event in the chain (enter, over,
// drop) is what suppresses that.
function onStageDragEnter(ev: DragEvent) {
  if (!ev.dataTransfer || !hasFileDrag(ev.dataTransfer)) return
  isDragOver.value = true
}
function onStageDragOver(ev: DragEvent) {
  if (!ev.dataTransfer || !hasFileDrag(ev.dataTransfer)) return
  ev.dataTransfer.dropEffect = 'copy'
  isDragOver.value = true
}
function onStageDragLeave(ev: DragEvent) {
  // `dragleave` fires when crossing into child elements too. Use the
  // related-target test to ignore child traversals — only flip the
  // cue off when the pointer leaves the stage entirely.
  const stage = stageEl.value
  const next = ev.relatedTarget as Node | null
  if (stage && next && stage.contains(next)) return
  isDragOver.value = false
}
function onStageDrop(ev: DragEvent) {
  isDragOver.value = false
  if (!ev.dataTransfer) return
  // Iterate `items` (NOT `files`) so we can preflight each entry via
  // `webkitGetAsEntry()` and skip directories with a clear toast.
  // Field repro pre-rc.11: dragging a folder onto the viewer uploaded
  // a 0-byte file named after the folder because Chrome reports the
  // dropped folder in `dataTransfer.files[0]` as a synthetic File
  // object that throws on read. webkitGetAsEntry() exposes the real
  // type so we can refuse cleanly. (Folder upload is on the
  // post-0.3.0 deferred list.)
  const files: File[] = []
  let folderCount = 0
  const items = ev.dataTransfer.items
  if (items && items.length > 0) {
    for (let i = 0; i < items.length; i++) {
      const item = items[i]
      if (item.kind !== 'file') continue
      // webkitGetAsEntry is the de-facto-standard API across Chrome,
      // Firefox, Safari, Edge for telling files from directories.
      const entry = (item as DataTransferItem & {
        webkitGetAsEntry?: () => { isDirectory?: boolean; isFile?: boolean } | null
      }).webkitGetAsEntry?.()
      if (entry?.isDirectory) {
        folderCount++
        continue
      }
      const f = item.getAsFile()
      if (f) files.push(f)
    }
  } else if (ev.dataTransfer.files) {
    // Browser without items API — fall back to files but warn that
    // we can't tell folders from empty files.
    for (let i = 0; i < ev.dataTransfer.files.length; i++) {
      files.push(ev.dataTransfer.files[i])
    }
  }
  if (folderCount > 0) {
    showError(
      folderCount === 1
        ? 'Folder upload not supported yet — drop individual files'
        : `${folderCount} folders skipped — folder upload not supported yet`
    )
  }
  if (files.length > 0) void uploadMany(files)
}
function hasFileDrag(dt: DataTransfer): boolean {
  // `types` is the only field populated during dragenter / dragover
  // for security reasons (the actual file list isn't readable until
  // drop). 'Files' is the documented marker for an OS file drag.
  for (let i = 0; i < dt.types.length; i++) {
    if (dt.types[i] === 'Files') return true
  }
  return false
}

// Stream the user-picked file(s) to the remote's Downloads folder
// via the `files` DC. 64 KiB chunks with backpressure on
// `RTCDataChannel.bufferedAmount` so large files don't OOM the tab.
// `multiple` on the input lets the operator queue several at once.
async function onFilePicked(ev: Event) {
  const input = ev.target as HTMLInputElement | null
  const list = input?.files
  if (!list || list.length === 0) return
  const files: File[] = []
  for (let i = 0; i < list.length; i++) files.push(list[i])
  try {
    await uploadMany(files)
  } finally {
    if (input) input.value = '' // allow re-selecting the same file(s)
  }
}

// --- Files browser drawer state (Phase 3 of file-DC v2) ---
const filesDrawer = ref(false)
const dirLoading = ref(false)
const dirError = ref<string | null>(null)
const dirEntries = ref<{ name: string; is_dir: boolean; size: number | null; mtime_unix: number | null }[]>([])
const currentDirPath = ref('')
const currentParent = ref<string | null>(null)
const dirPathInput = ref('')
const selectedDirEntries = ref<Set<string>>(new Set())
let lastSelectedDirIndex: number | null = null

async function navigateTo(path: string) {
  dirLoading.value = true
  dirError.value = null
  selectedDirEntries.value = new Set()
  lastSelectedDirIndex = null
  try {
    const listing = await rc.listDir(path)
    currentDirPath.value = listing.path
    currentParent.value = listing.parent
    dirPathInput.value = listing.path
    dirEntries.value = listing.entries
  } catch (e) {
    dirError.value = (e as Error).message
    dirEntries.value = []
  } finally {
    dirLoading.value = false
  }
}

// Auto-load roots view the first time the drawer opens.
watch(filesDrawer, (open) => {
  if (open && dirEntries.value.length === 0 && !dirLoading.value) {
    void navigateTo('')
  }
})

function onEntryClick(
  entry: { name: string; is_dir: boolean },
  // Vuetify's `<v-list-item @click>` fires BOTH on mouse click and
  // on keyboard activation (Enter / Space — accessibility), so the
  // handler receives `MouseEvent | KeyboardEvent`. Both event types
  // expose `shiftKey` / `ctrlKey` / `metaKey` so the modifier-key
  // logic below works uniformly.
  ev: MouseEvent | KeyboardEvent
) {
  // Ctrl/Cmd+click toggles selection; Shift+click extends; plain
  // click selects only this entry. Multi-select is what makes
  // Ctrl+C-as-download work cleanly across multiple entries.
  const idx = dirEntries.value.findIndex((e) => e.name === entry.name)
  if (ev.shiftKey && lastSelectedDirIndex !== null) {
    const lo = Math.min(lastSelectedDirIndex, idx)
    const hi = Math.max(lastSelectedDirIndex, idx)
    const range = new Set(selectedDirEntries.value)
    for (let i = lo; i <= hi; i++) range.add(dirEntries.value[i].name)
    selectedDirEntries.value = range
  } else if (ev.ctrlKey || ev.metaKey) {
    const next = new Set(selectedDirEntries.value)
    if (next.has(entry.name)) next.delete(entry.name)
    else next.add(entry.name)
    selectedDirEntries.value = next
    lastSelectedDirIndex = idx
  } else {
    selectedDirEntries.value = new Set([entry.name])
    lastSelectedDirIndex = idx
  }
}

function onEntryDblClick(entry: { name: string; is_dir: boolean }) {
  if (entry.is_dir) {
    const sep = /[\\/]$/.test(currentDirPath.value) ? '' : pathSeparator()
    void navigateTo(currentDirPath.value + sep + entry.name)
  }
}

function pathSeparator(): string {
  // Heuristic: Windows paths contain a drive letter or `\`. Anything
  // else is Unix.
  if (/^[A-Za-z]:[\\\/]/.test(currentDirPath.value) || currentDirPath.value.includes('\\')) {
    return '\\'
  }
  return '/'
}

function formatFileSize(bytes: number | null): string {
  if (bytes === null || bytes === undefined) return ''
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`
}

async function downloadEntry(entry: { name: string; is_dir: boolean }) {
  const sep = /[\\/]$/.test(currentDirPath.value) ? '' : pathSeparator()
  const fullPath = currentDirPath.value + sep + entry.name
  try {
    if (entry.is_dir) {
      const r = await rc.downloadFolder(fullPath, `${entry.name}.zip`)
      showSuccess(`Downloaded ${r.name} (${formatFileSize(r.bytes)})`)
    } else {
      const r = await rc.downloadFile(fullPath, entry.name)
      showSuccess(`Downloaded ${r.name} (${formatFileSize(r.bytes)})`)
    }
  } catch (e) {
    showError(`Download failed: ${(e as Error).message}`)
  }
}

// Drawer-scope paste: `paste` event fires when the operator hits
// Ctrl+V with the drawer focused. If the OS clipboard has files,
// upload them. Phase 5 of file-DC v2 also wires Ctrl+V over the
// viewer with deferral; this drawer-scope path is the simpler
// half (no keystroke-forward conflict).
function onDrawerPaste(ev: ClipboardEvent) {
  const dt = ev.clipboardData
  if (!dt || !dt.files || dt.files.length === 0) return
  ev.preventDefault()
  ev.stopPropagation()
  const files: File[] = []
  for (let i = 0; i < dt.files.length; i++) files.push(dt.files[i])
  void uploadMany(files)
}

// Drawer-scope Ctrl+C / Cmd+C — copy selected entries as downloads.
// When `selectedDirEntries` is non-empty, queue a `downloadFile` per
// file entry and `downloadFolder` per directory. Sequential per the
// single-active-outgoing-transfer invariant; the registry's queue
// serialises them automatically.
function onDrawerKeyDown(ev: KeyboardEvent) {
  if (ev.code !== 'KeyC') return
  if (!(ev.ctrlKey || ev.metaKey)) return
  // Skip if focus is in the path-input field — let it copy text
  // natively instead of intercepting.
  const target = ev.target as Element | null
  if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA')) {
    return
  }
  if (selectedDirEntries.value.size === 0) return
  ev.preventDefault()
  ev.stopPropagation()
  // Snapshot — selectedDirEntries can mutate during the await chain.
  const entries = Array.from(selectedDirEntries.value)
    .map((name) => dirEntries.value.find((e) => e.name === name))
    .filter((e): e is NonNullable<typeof e> => !!e)
  void downloadEntries(entries)
}

async function downloadEntries(
  entries: { name: string; is_dir: boolean }[]
) {
  let success = 0
  let failed = 0
  for (const entry of entries) {
    try {
      await downloadEntry(entry)
      success++
    } catch (e) {
      failed++
      void e
    }
  }
  if (entries.length > 1) {
    if (failed === 0) {
      showSuccess(`Downloaded ${success} entries`)
    } else if (success === 0) {
      showError(`All ${failed} downloads failed`)
    } else {
      showError(`Downloaded ${success}/${entries.length} — ${failed} failed`)
    }
  }
}

async function uploadMany(files: File[]) {
  if (files.length === 0) return
  uploadBusy.value = true
  try {
    const results = await rc.uploadFiles(files)
    const ok = results.filter((r) => r.ok).length
    const failed = results.filter((r) => !r.ok)
    if (failed.length === 0) {
      showSuccess(
        ok === 1
          ? `Uploaded ${results[0].name}`
          : `Uploaded ${ok} files`
      )
    } else if (ok === 0) {
      const first = failed[0] as { name: string; error: string }
      showError(
        failed.length === 1
          ? `Upload failed: ${first.error}`
          : `${failed.length} uploads failed (first: ${first.name} — ${first.error})`
      )
    } else {
      const first = failed[0] as { name: string; error: string }
      showError(
        `Uploaded ${ok}/${files.length} — ${failed.length} failed (e.g. ${first.name}: ${first.error})`
      )
    }
  } finally {
    uploadBusy.value = false
  }
}

// Quality preference: v-select emits immediately on change. We proxy
// through a computed so the composable stays the source of truth
// (persists + pushes to agent). The v-select's inner value is
// whatever the composable already holds, so reloads show the
// restored preference without an extra effect.
const qualityOptions = [
  { title: 'Auto', value: 'auto' },
  { title: 'Low', value: 'low' },
  { title: 'High', value: 'high' },
] as const
const quality = computed<RcQuality>({
  get: () => rc.quality.value,
  set: (v: RcQuality) => rc.setQuality(v),
})

// Codec override: null = let the agent pick the best available. The
// value is persisted via the composable (localStorage) so it survives
// a page reload; it only takes effect on the next connect — live
// sessions keep whatever SDP they negotiated at start.
const codecOptions = [
  { title: 'Codec: Auto', value: null },
  { title: 'H.264', value: 'h264' },
  { title: 'H.265 / HEVC', value: 'h265' },
  { title: 'AV1', value: 'av1' },
  { title: 'VP9', value: 'vp9' },
] as const
const codecOverride = computed<RcPreferredCodec | null>({
  get: () => rc.preferredCodec.value,
  set: (v: RcPreferredCodec | null) => rc.setPreferredCodec(v),
})

// Scale mode + custom percent. Proxy through a computed so the
// composable stays the source of truth (persists across reloads).
const scaleOptions = [
  { title: 'Adaptive', value: 'adaptive' },
  { title: 'Original', value: 'original' },
  { title: 'Custom…', value: 'custom' },
] as const
const scaleMode = computed<RcScaleMode>({
  get: () => rc.scaleMode.value,
  set: (v: RcScaleMode) => rc.setScaleMode(v),
})
const scalePercent = computed<number>({
  get: () => rc.scaleCustomPercent.value,
  set: (v: number) => rc.setScaleCustomPercent(v),
})

// Intrinsic remote-frame dimensions. The composable is the source of
// truth — it's fed by `<video>.onresize` in classic mode and by the
// WebCodecs worker's `first-frame` message in the low-latency path.
// That way any consumer downstream (scale style, coord math, cursor
// overlay) reads one set of refs regardless of render path.
const videoIntrinsicW = rc.mediaIntrinsicW
const videoIntrinsicH = rc.mediaIntrinsicH

// Render-path toggle. The composable persists the preference across
// reloads; `webcodecsSupported` is true only when the browser exposes
// both RTCRtpScriptTransform + VideoDecoder (Chrome 94+). The
// `isWebCodecsRender` computed drives template rendering — it's only
// true when the session is actively using the WebCodecs path (the
// user opted in AND the browser supports it). We read `rc.renderPath`
// directly so the UI state matches what the next Connect would do.
const webcodecsOn = computed<boolean>(() => rc.renderPath.value === 'webcodecs')
// Which element the viewer actually mounts — driven by the runtime
// `webcodecsActive` flag that the composable flips to true ONLY when
// the RTCRtpScriptTransform is successfully installed for this
// session. A user preference of `renderPath === 'webcodecs'` on a
// session where we fall back (HEVC, browser without the API,
// transferControlToOffscreen throwing) flips to `<video>` transparently
// instead of mounting an empty canvas.
const isWebCodecsRender = computed<boolean>(() => rc.webcodecsActive.value)
const webcodecsTooltip = computed<string>(() => {
  if (!rc.webcodecsSupported.value) {
    return 'Low-latency (WebCodecs) path requires Chrome 94+ — not supported in this browser'
  }
  return webcodecsOn.value
    ? 'Low-latency (WebCodecs) ON — bypasses <video> jitter buffer. Takes effect on next Connect.'
    : 'Low-latency (WebCodecs) OFF — using <video> render path'
})
function toggleWebCodecs() {
  const next: RcRenderPath = webcodecsOn.value ? 'video' : 'webcodecs'
  rc.setRenderPath(next)
}

// Phase Y.4: VP9 4:4:4 over DataChannel toggle. The composable's
// `videoTransport` ref persists the choice across reloads;
// `vp9_444Supported` is true only when the browser exposes
// `VideoDecoder.isConfigSupported({codec:'vp09.01.10.08'})`. The
// agent honours the preference only when its caps probe passed
// (libvpx Vp9Encoder activated successfully at startup); otherwise
// the session silently falls back to the legacy WebRTC video
// transport. Takes effect on the next Connect — switching mid-
// session would require tearing down + rebuilding the entire
// PC, which is more disruptive than "reconnect to apply".
const vp9_444On = computed<boolean>(
  () => rc.videoTransport.value === 'data-channel-vp9-444',
)
const vp9_444Tooltip = computed<string>(() => {
  if (!rc.vp9_444Supported.value) {
    return 'Crystal-clear (VP9 4:4:4) requires WebCodecs VideoDecoder vp09.01.10.08 — not supported in this browser'
  }
  return vp9_444On.value
    ? 'Crystal-clear (VP9 4:4:4) ON — bypasses Chrome\'s 4:2:0 video pipeline. Takes effect on next Connect.'
    : 'Crystal-clear (VP9 4:4:4) OFF — using standard WebRTC video transport'
})
function toggleVp9_444() {
  const next: RcVideoTransport = vp9_444On.value ? 'webrtc' : 'data-channel-vp9-444'
  rc.setVideoTransport(next)
}
/** Bind callback for the webcodecs canvas ref. Vue calls this with
 *  the element (or null on unmount) — we forward to the composable's
 *  writable canvas ref so `pc.ontrack` can see it. */
function bindWebcodecsCanvas(el: Element | unknown) {
  rc.webcodecsCanvasEl.value = (el as HTMLCanvasElement | null) ?? null
}

// Phase Y.4 view-side render gate. Flips true when the composable
// has opened the `video-bytes` DC AND spun up the VP9-444 worker
// (Y.3 sets `vp9_444Active` in `startVp9_444Path()`). Drives the
// template `<canvas>` swap below — the legacy `<video>` element
// stays hidden in this mode because the agent doesn't ship a
// WebRTC video track when the negotiated transport is
// `data-channel-vp9-444`.
const isVp9_444Render = computed<boolean>(() => rc.vp9_444Active.value)
/** Bind callback for the VP9-444 canvas. The composable's watcher
 *  on `vp9_444CanvasEl` transfers OffscreenCanvas control to the
 *  worker as soon as we set the ref, replacing the synthetic
 *  OffscreenCanvas the worker started with so decoded frames land
 *  on the visible element. */
function bindVp9_444Canvas(el: Element | unknown) {
  rc.vp9_444CanvasEl.value = (el as HTMLCanvasElement | null) ?? null
}

// Fullscreen toggle. Drives the stage element into/out of the browser's
// Fullscreen API. `isFullscreen` tracks the real DOM state via the
// fullscreenchange event so ESC (which the browser handles natively)
// updates the icon without us polling.
//
// `fullscreenEnabled` gates the toolbar button: iOS Safari only supports
// `webkitEnterFullscreen` on `<video>` elements, NOT on arbitrary divs,
// and won't show overlay canvases (cursor / stats / no-media-overlay)
// because they aren't part of the <video>. Rather than render a button
// that does nothing, we hide it on browsers where the API isn't usable.
// `document.fullscreenEnabled` is the standard property; reads false on
// iPhone Safari, true on Chrome/Firefox/Safari desktop, true in Chromium
// Android (where it works on divs).
const fullscreenEnabled = computed<boolean>(() => {
  if (typeof document === 'undefined') return false
  return document.fullscreenEnabled === true
})
const isFullscreen = ref(false)
function toggleFullscreen() {
  const el = stageEl.value
  if (!el) return
  if (document.fullscreenElement) {
    void document.exitFullscreen().catch(() => { /* user cancelled; ignore */ })
  } else {
    void el.requestFullscreen().catch(() => { /* user gesture / API missing; ignore */ })
  }
}
function onFullscreenChange() {
  isFullscreen.value = document.fullscreenElement !== null
}

// Inline style for the <video> element. In `original` and `custom`
// modes we set explicit pixel dims so the outer `.video-frame` can
// detect overflow and show scrollbars; the `<video>` element's
// `width: auto` default is unreliable inside a flex container. In
// `adaptive` mode the CSS class handles sizing (100%/100% +
// object-fit: contain).
const videoScaleStyle = computed<Record<string, string> | undefined>(() => {
  const w = videoIntrinsicW.value
  const h = videoIntrinsicH.value
  if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return undefined
  if (rc.scaleMode.value === 'custom') {
    const pct = rc.scaleCustomPercent.value / 100
    return { width: `${w * pct}px`, height: `${h * pct}px` }
  }
  if (rc.scaleMode.value === 'original') {
    return { width: `${w}px`, height: `${h}px` }
  }
  return undefined
})

// -----------------------------------------------------------------
// Remote resolution (Phase 2 of the viewer-controls sprint)
// -----------------------------------------------------------------

// The v-select value is a discriminator string — not the full
// RcResolutionSetting — because v-select items need primitive values
// for equality. `original` / `fit` map directly; `custom` maps to
// `custom:<w>x<h>` for display and opens a dialog when picked so the
// operator can edit dims.
const resolutionOptions = computed(() => {
  const opts: { title: string; value: string }[] = [
    { title: 'Original resolution', value: 'original' },
    { title: 'Fit to local viewport', value: 'fit' },
  ]
  if (rc.resolution.value.mode === 'custom') {
    const w = rc.resolution.value.width ?? 0
    const h = rc.resolution.value.height ?? 0
    opts.push({ title: `Custom: ${w} × ${h}`, value: 'custom-current' })
  }
  opts.push({ title: 'Custom…', value: 'custom-edit' })
  return opts
})

const resolutionPresetValue = computed<string>({
  get: () => {
    if (rc.resolution.value.mode === 'original') return 'original'
    if (rc.resolution.value.mode === 'fit') return 'fit'
    return 'custom-current'
  },
  set: (v) => {
    if (v === 'original') {
      rc.setResolution({ mode: 'original' })
    } else if (v === 'fit') {
      applyFitResolution()
    } else if (v === 'custom-edit') {
      // Seed the dialog from the current values (or the stage's
      // dimensions if we have none yet) so the user isn't starting
      // from a blank field.
      const cur = rc.resolution.value
      customResolutionW.value = cur.width ?? 1920
      customResolutionH.value = cur.height ?? 1080
      customResolutionDialog.value = true
    }
    // 'custom-current' is a noop — it's only used as the v-select's
    // "display the existing custom dims" slot.
  },
})

const resolutionButtonTitle = computed(() => {
  const s = rc.resolution.value
  if (s.mode === 'original') return 'Agent streams at native monitor resolution'
  if (s.mode === 'fit') {
    return `Agent downscales to fit local viewport (currently ${s.width ?? '?'} × ${s.height ?? '?'})`
  }
  return `Custom: ${s.width ?? '?'} × ${s.height ?? '?'}`
})

const customResolutionDialog = ref(false)
const customResolutionW = ref(1920)
const customResolutionH = ref(1080)
const customResolutionPresets: Array<{ w: number; h: number; note?: string }> = [
  { w: 1280, h: 720, note: '720p' },
  { w: 1920, h: 1080, note: '1080p' },
  { w: 1920, h: 1200, note: 'WUXGA' },
  { w: 2560, h: 1440, note: '1440p' },
  { w: 2560, h: 1600, note: 'WQXGA' },
  { w: 3840, h: 2160, note: '4K UHD' },
]
const customResolutionValid = computed(() => {
  const w = customResolutionW.value
  const h = customResolutionH.value
  return (
    Number.isFinite(w) && Number.isFinite(h) &&
    w >= 160 && w <= 7680 &&
    h >= 120 && h <= 4320
  )
})
function pickCustomResolutionPreset(w: number, h: number) {
  customResolutionW.value = w
  customResolutionH.value = h
}
function confirmCustomResolution() {
  if (!customResolutionValid.value) return
  const setting: RcResolutionSetting = {
    mode: 'custom',
    width: Math.round(customResolutionW.value),
    height: Math.round(customResolutionH.value),
  }
  rc.setResolution(setting)
  customResolutionDialog.value = false
}

/** Apply Fit mode using the current stage dimensions × devicePixelRatio
 *  — captures "what fits in my browser right now at its native pixel
 *  density". Also re-emitted on stage resize via `ResizeObserver`
 *  below, debounced 250 ms so drag-resize doesn't churn the encoder. */
function applyFitResolution() {
  const el = stageEl.value
  if (!el) return
  const rect = el.getBoundingClientRect()
  const dpr = window.devicePixelRatio || 1
  // Floor to even numbers — MF HEVC encoder requires even dims and
  // fail-closes to NoopEncoder otherwise (permanent black screen
  // until reconnect). The agent also rounds defensively, but
  // emitting clean numbers here avoids the log churn. Clamp mins
  // to 160×90 so weird <= 1 layouts don't flood with tiny requests.
  const w = Math.max(160, Math.round(rect.width * dpr) & ~1)
  const h = Math.max(90, Math.round(rect.height * dpr) & ~1)
  rc.setResolution({ mode: 'fit', width: w, height: h })
}

// ResizeObserver on the stage so Fit mode tracks viewport changes.
// Debounced — drag-resize fires dozens of events per second and each
// rc:resolution change rebuilds the encoder on the agent side.
let fitResizeTimer: ReturnType<typeof setTimeout> | null = null
let fitResizeObserver: ResizeObserver | null = null
function startFitResizeObserver() {
  if (fitResizeObserver || !stageEl.value || !('ResizeObserver' in window)) return
  fitResizeObserver = new ResizeObserver(() => {
    if (rc.resolution.value.mode !== 'fit') return
    if (fitResizeTimer) clearTimeout(fitResizeTimer)
    fitResizeTimer = setTimeout(() => {
      applyFitResolution()
    }, 250)
  })
  fitResizeObserver.observe(stageEl.value)
}
function stopFitResizeObserver() {
  if (fitResizeTimer) {
    clearTimeout(fitResizeTimer)
    fitResizeTimer = null
  }
  if (fitResizeObserver) {
    fitResizeObserver.disconnect()
    fitResizeObserver = null
  }
}

// Stats readout formatters. Pure computeds — the composable already
// polls getStats() every 500 ms and updates rc.stats.value.
//
// The codec label is enriched with HW/SW based on the agent's
// advertised AgentCaps.hw_encoders (2A.2 wired). This makes the
// pill informative ("H.265 HW") rather than ambiguous ("H265").
const statsCodecLabel = computed(() => {
  const raw = rc.stats.value.codec
  if (!raw) return ''
  const lower = raw.toLowerCase()
  // Prettify well-known names. H264 → H.264, H265 → H.265; others
  // pass through uppercased.
  const display = lower
    .replace(/^h(\d{3})$/, (_m, n) => `H.${n}`)
    .toUpperCase()
  // Guess HW/SW from the agent's caps if available; default to SW
  // (the safe assumption — reporting HW when uncertain would
  // mislead the operator about latency expectations).
  const enc = agent.value?.capabilities?.hw_encoders ?? []
  const hasHw = enc.some(
    (e) => e.toLowerCase().includes(lower) && e.toLowerCase().includes('-hw'),
  )
  return `${display} ${hasHw ? 'HW' : 'SW'}`
})
const statsBitrateLabel = computed(() => {
  const bps = rc.stats.value.bitrate_bps
  if (bps <= 0) return '— bps'
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mbps`
  return `${Math.round(bps / 1_000)} kbps`
})
const statsFpsLabel = computed(() => {
  const fps = rc.stats.value.fps
  if (fps <= 0) return '— fps'
  return `${Math.round(fps)} fps`
})

// Remote cursor overlay (1E.3). Requires both a position and a
// matching shape bitmap; hides during paint if either is missing.
const remoteCursorVisible = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return false
  return rc.cursor.value.shapes.has(pos.id)
})

const remoteCursorSize = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return { w: 0, h: 0 }
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return { w: 0, h: 0 }
  return { w: shape.bitmap.width, h: shape.bitmap.height }
})

// Translate agent-source pixels → viewer-local pixels using the
// same letterbox correction the pointer input uses, so the cursor
// lands at the exact spot on the video. `agent.value` comes from
// load step below and carries the agent's native resolution via
// its capability payload (the displays list); we fall back to the
// video element's intrinsic size otherwise.
const remoteCursorX = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return 0
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return 0
  const scale = cursorMapping()
  return scale.offsetX + pos.x * scale.sx - shape.hotspotX
})
const remoteCursorY = computed(() => {
  const pos = rc.cursor.value.pos
  if (!pos) return 0
  const shape = rc.cursor.value.shapes.get(pos.id)
  if (!shape) return 0
  const scale = cursorMapping()
  return scale.offsetY + pos.y * scale.sy - shape.hotspotY
})

/** Map an agent-source pixel coordinate to the logical coordinate
 *  space of `.video-frame` (which the cursor canvas + synthetic
 *  badge are positioned inside). Scale-mode aware:
 *
 *  - `adaptive`: `<video>` fills the frame with `object-fit: contain`.
 *    Use the original letterbox math to find the scale factor + any
 *    letterbox padding.
 *  - `original`: video at intrinsic 1:1, anchored to the frame's
 *    top-left (flex: flex-start). Scale = 1, no offsets. If the frame
 *    is scrolled the transform stays in logical space → visually
 *    tracks the content.
 *  - `custom`: scale = `scalePercent / 100`, no offsets (same flex
 *    anchor). */
function cursorMapping(): { sx: number; sy: number; offsetX: number; offsetY: number } {
  const stage = stageEl.value
  const video = videoEl.value
  // Source dimensions: in VP9-444 / WebCodecs render modes the
  // `<video>` is hidden + unfed (videoWidth=0), so the agent's encode
  // resolution we cached from the worker's `first-frame` message is
  // the only ground truth for source pixel size.
  const useIntrinsic = rc.vp9_444Active.value || rc.webcodecsActive.value
  const srcW = useIntrinsic
    ? rc.mediaIntrinsicW.value
    : (video?.videoWidth ?? 0)
  const srcH = useIntrinsic
    ? rc.mediaIntrinsicH.value
    : (video?.videoHeight ?? 0)
  if (!stage || !srcW || !srcH) {
    return { sx: 1, sy: 1, offsetX: 0, offsetY: 0 }
  }
  if (rc.scaleMode.value === 'original') {
    return { sx: 1, sy: 1, offsetX: 0, offsetY: 0 }
  }
  if (rc.scaleMode.value === 'custom') {
    const pct = rc.scaleCustomPercent.value / 100
    return { sx: pct, sy: pct, offsetX: 0, offsetY: 0 }
  }
  // Adaptive: the video fills the frame with object-fit: contain,
  // producing letterbox bars on the axis where aspect ratios disagree.
  const fw = stage.clientWidth
  const fh = stage.clientHeight
  const vAR = srcW / srcH
  const fAR = fw / fh
  let visibleW: number
  let visibleH: number
  let offsetX: number
  let offsetY: number
  if (vAR > fAR) {
    visibleW = fw
    visibleH = fw / vAR
    offsetX = 0
    offsetY = (fh - visibleH) / 2
  } else {
    visibleW = fh * vAR
    visibleH = fh
    offsetX = (fw - visibleW) / 2
    offsetY = 0
  }
  return {
    sx: visibleW / srcW,
    sy: visibleH / srcH,
    offsetX,
    offsetY,
  }
}

// Paint the current cursor shape onto the canvas every time the
// shape or pos changes. drawImage is cheap (O(cursor pixels), ≤32×32
// for classic cursors) so we don't need an explicit RAF loop.
watch(
  () => {
    const p = rc.cursor.value.pos
    return [p?.id ?? null, cursorCanvas.value] as const
  },
  ([id, canvas]) => {
    if (!canvas || id == null) return
    const shape = rc.cursor.value.shapes.get(id)
    if (!shape) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return
    ctx.clearRect(0, 0, canvas.width, canvas.height)
    ctx.drawImage(shape.bitmap, 0, 0)
  },
  { immediate: false },
)

let detachInput: (() => void) | null = null

// Synthetic cursor overlay. The native pointer is hidden over the video
// (cursor: none in CSS below) so this badge is the only pointer indicator.
// Initials come from the logged-in controller so it stays meaningful if
// multi-watcher sessions land later (today it's 1:1, but the label is
// already user-scoped).
const cursorX = ref(0)
const cursorY = ref(0)
const cursorVisible = ref(false)
const controllerInitials = computed(() => {
  const u = authStore.user
  const src = u?.display_name || u?.username || ''
  const parts = src.trim().split(/\s+/).filter(Boolean)
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase()
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase()
  return ''
})
function onStagePointerMove(ev: PointerEvent) {
  const host = stageEl.value
  if (!host) return
  const rect = host.getBoundingClientRect()
  // `transform: translate()` on the badge/canvas is in the logical
  // coordinate space of `.video-frame` — the space that includes
  // scroll offset. `ev.clientX - rect.left` gives viewport-relative
  // offset from the frame's *visible* left edge; add scrollLeft to
  // reach logical space so the overlay tracks the pointer after
  // scrolling in Original / Custom modes.
  cursorX.value = ev.clientX - rect.left + host.scrollLeft
  cursorY.value = ev.clientY - rect.top + host.scrollTop
}

const canConnect = computed(() => !!agent.value)
const statusColor = computed(() => (agent.value?.is_online ? 'success' : 'grey'))
const phaseColor = computed(() => {
  switch (rc.phase.value) {
    case 'connected': return 'success'
    case 'reconnecting': return 'warning'
    case 'error': return 'error'
    case 'closed': return 'grey'
    default: return 'info'
  }
})
const phaseLabel = computed(() => {
  switch (rc.phase.value) {
    case 'requesting': return 'Requesting session…'
    case 'awaiting_consent': return 'Waiting for the agent to allow the connection…'
    case 'negotiating': return 'Negotiating the peer connection…'
    default: return ''
  }
})

// File-DC v2 capability gates (0.3.0+). The agent advertises a
// per-feature `files: ["upload","download","download-folder","browse"]`
// list in `rc:agent.hello`. Old agents (<0.3.0) leave the field empty;
// browsers fall back to the coarse `supports_file_transfer` bool which
// only marks upload availability. We grey out toolbar buttons when
// the capability isn't advertised so operators get instant "feature
// unavailable" feedback instead of waiting for a 5 s timeout on an
// unanswered request.
const agentFilesCaps = computed<string[]>(() => agent.value?.capabilities?.files ?? [])
const agentSupportsBrowse = computed(() => agentFilesCaps.value.includes('browse'))
const agentSupportsDownload = computed(() =>
  agentFilesCaps.value.includes('download') || agentFilesCaps.value.includes('download-folder')
)
const agentSupportsFolderDownload = computed(() =>
  agentFilesCaps.value.includes('download-folder')
)
// Old-agent flag: file-DC v1 (only upload) — coarse caps absent. Used
// to render a "agent doesn't support download — upgrade to 0.3.0+"
// hint when the operator opens the drawer.
const isLegacyFileDc = computed(() => agentFilesCaps.value.length === 0)

async function loadAgent() {
  if (!agentStore.agents.length) {
    await agentStore.fetchAgents(tenantId.value)
  }
  agent.value = agentStore.agents.find((a) => a.id === agentId.value) || null
}

function startSession() {
  if (!agent.value) return
  rc.connect(agent.value.id)
}

// When the remote stream becomes available, attach it to the video element.
// Race to watch out for: ontrack can fire during `phase === 'negotiating'`
// (before the <video> element is even mounted, since it lives inside a
// v-else-if="phase === 'connected'"). A single watcher on the stream would
// see videoEl.value = null at that moment and silently skip the assignment;
// when the element mounts later no watcher re-fires. Watch both refs and
// attach whenever both are present.
let rvfcHandle: number | null = null
// Keep our intrinsic-dimension refs in sync with the actual video
// element. `resize` fires on every resolution change from the agent
// (docking, DPI flip, rc:resolution control message in Phase 2);
// `loadedmetadata` covers the first-frame bootstrap. In WebCodecs
// mode the <video> element never receives decoded frames (the
// receiver transform swallows them), so `videoWidth` stays 0 — we
// skip writing zeros here to avoid clobbering the worker's first-
// frame dims. The worker's `first-frame` message is the authoritative
// source in that mode.
function refreshVideoDims(el: HTMLVideoElement) {
  if (isWebCodecsRender.value) return
  videoIntrinsicW.value = el.videoWidth || 0
  videoIntrinsicH.value = el.videoHeight || 0
}
watch(
  () => [rc.remoteStream.value, videoEl.value] as const,
  ([stream, el]) => {
    if (stream && el && el.srcObject !== stream) {
      el.srcObject = stream
      // Track intrinsic video size so `custom` scale mode can compute
      // pixel dimensions + the coordinate mapper can fall back cleanly.
      el.addEventListener('loadedmetadata', () => refreshVideoDims(el))
      el.addEventListener('resize', () => refreshVideoDims(el))
      refreshVideoDims(el)
      // requestVideoFrameCallback keeps the tab "hot" against
      // Chrome's background throttling AND gives us a cheap hook to
      // recover from the video element's paused-for-optimization
      // state that sometimes triggers on identical-frame runs (e.g.
      // long idle screens).
      const elWithRvfc = el as HTMLVideoElement & {
        requestVideoFrameCallback?: (cb: (now: number, metadata: unknown) => void) => number
      }
      const rvfc = elWithRvfc.requestVideoFrameCallback
      if (typeof rvfc === 'function') {
        const tick = () => {
          if (!videoEl.value) {
            rvfcHandle = null
            return
          }
          if (videoEl.value.paused) {
            videoEl.value.play().catch(() => { /* autoplay gating — ignore */ })
          }
          rvfcHandle = (videoEl.value as typeof elWithRvfc)
            .requestVideoFrameCallback!(tick)
        }
        rvfcHandle = rvfc.call(el, tick)
      }
    }
  },
  { immediate: true },
)

// Once the connected stage mounts, wire input listeners to it. Detach
// when we leave the "connected" phase so keystrokes don't escape after
// a disconnect.
watch(
  () => [rc.phase.value, stageEl.value] as const,
  ([phase, el]) => {
    if (phase === 'connected' && el && !detachInput) {
      detachInput = rc.attachInput(el as HTMLElement, {
        // Phase 5: when the operator hits Ctrl+V over the viewer
        // with files in their OS clipboard, route to the upload
        // pipeline. The composable suppresses the Ctrl+V keystroke
        // so the remote app doesn't see a stray paste.
        onFilesPasted: (files) => {
          if (files.length === 0) return
          showSuccess(
            files.length === 1
              ? `Uploading ${files[0].name}…`
              : `Uploading ${files.length} files…`
          )
          void uploadMany(files)
        },
      })
      ;(el as HTMLElement).focus()
      // Start watching the stage for size changes so Fit mode
      // auto-updates the agent's target resolution.
      startFitResizeObserver()
      // If the restored preference was Fit (from localStorage) the
      // stored width/height are from the previous session — re-emit
      // with the current window size so the agent uses today's dims.
      if (rc.resolution.value.mode === 'fit') applyFitResolution()
    } else if (phase !== 'connected' && detachInput) {
      detachInput()
      detachInput = null
      stopFitResizeObserver()
    }
  },
)

onMounted(() => {
  void loadAgent()
  document.addEventListener('fullscreenchange', onFullscreenChange)
})
onBeforeUnmount(() => {
  if (detachInput) detachInput()
  stopFitResizeObserver()
  document.removeEventListener('fullscreenchange', onFullscreenChange)
  // Exit fullscreen on unmount so navigating away doesn't leave the
  // browser in a weird fullscreen state.
  if (document.fullscreenElement) void document.exitFullscreen().catch(() => {})
  rc.disconnect()
})
</script>

<style scoped>
.remote-control-wrapper {
  height: 100%;
  display: flex;
  flex-direction: column;
}
/* Row 1 (primary toolbar): keep on a single line at every viewport
   so Back / title / Connect-Disconnect / Fullscreen never push off-
   screen. Vuetify's outer wrapper clips overflow by default; the
   `overflow-x: auto` on `__content` is a defensive fallback for
   borderline viewports where a long agent name + chips push past
   320px. The `flex-shrink: 1` on the title lets it ellipsis instead
   of forcing the end-of-row buttons off-screen. Field bug
   PC50045 mobile 2026-05-01 ('cannot fullscreen, button is gone'
   after Connect mounted +5 buttons in the single-row toolbar). */
.remote-control-wrapper :deep(.rc-toolbar-primary .v-toolbar__content) {
  overflow-x: auto;
}
.remote-control-wrapper :deep(.rc-toolbar-primary .v-toolbar-title) {
  flex-shrink: 1;
  min-width: 0;
}
/* Row 2 (session controls): visible on `md+`, hidden on `<md`.
   `flex-wrap: wrap` lets controls spill to a second visual line on
   borderline desktops (~960-1100px) instead of overflowing the
   toolbar. `border-bottom` separates it from the video stage. */
.remote-control-wrapper .rc-tools-row {
  background: rgb(var(--v-theme-surface));
  border-bottom: 1px solid rgba(var(--v-border-color), var(--v-border-opacity));
  min-height: 44px;
}
/* The v-select density="compact" inside the wrap row otherwise
   forces a 56px tall input on touch devices, which makes the row
   feel chunky. Trim to match the toolbar density. */
.remote-control-wrapper .rc-tools-row :deep(.v-field) {
  --v-field-padding-top: 4px;
}
/* The card wrapping `.remote-stage` provides Material elevation +
   rounded corners + theme-aware border. `overflow: hidden` clips
   the dark stage at the rounded corners; `min-height: 0` keeps
   the flex `min-height: 0` chain unbroken so `scale-original` mode
   at 4K doesn't push past the viewport. */
.remote-stage-card {
  overflow: hidden;
  min-height: 0;
  background: #0b0b0b;
}
.remote-stage {
  flex: 1;
  display: flex;
  align-items: stretch;
  justify-content: stretch;
  background: #0b0b0b;
  position: relative;
}
.empty-state {
  margin: auto;
  text-align: center;
  padding: 32px;
  color: rgba(255, 255, 255, 0.7);
}
.video-frame {
  position: relative;
  width: 100%;
  height: 100%;
  /* `min-height: 0` is required for the flex parent (`.remote-stage`
     with `align-items: stretch`) to allow this child to actually
     shrink to the available space. Without it, a large intrinsic
     child (e.g. a 4K <video> in Original mode) could balloon the
     frame past the viewport — showing a cropped view instead of
     scrollbars. `min-width: 0` is the horizontal counterpart. */
  min-width: 0;
  min-height: 0;
  /* Hide the native OS pointer so the synthetic cursor below is the only
     thing the controller sees — matches collaborative-tool semantics. */
  cursor: none;
}
.video-frame.drag-over::after {
  content: 'Drop file to upload';
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 1.2rem;
  font-weight: 600;
  color: #fff;
  background: rgba(33, 150, 243, 0.25);
  border: 3px dashed rgba(33, 150, 243, 0.85);
  pointer-events: none;
  z-index: 10;
}
/* Files browser drawer: selected rows highlight so multi-select
   (Ctrl+click / Shift+click) feedback is visible at a glance. */
.files-drawer .files-entry-selected {
  background: rgba(33, 150, 243, 0.18);
}
/* Scrollable frame for the scale modes where the video can overflow
   the viewport (original = 1:1 always if source > viewer, custom ≥
   100%). `block` display is intentional — a flex container would
   impose item-sizing rules and some browsers shrink the flex item
   even with `flex: none` when the container's overflow engages.
   Block + explicit child pixel dims (via :style) is the simplest
   path that reliably shows scrollbars. */
.video-frame.scale-original,
.video-frame.scale-custom {
  overflow: auto;
  display: block;
}
.remote-video {
  background: #000;
  display: block;
}
.remote-video.scale-adaptive {
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.remote-video.scale-original,
.remote-video.scale-custom {
  /* Explicit pixel dims come from the :style binding. object-fit:
     fill stretches to exactly the CSS-declared dimensions with no
     letterbox, which is what we want for 1:1 Original and the
     user-driven Custom scale. */
  object-fit: fill;
}
.remote-cursor-canvas {
  position: absolute;
  top: 0;
  left: 0;
  pointer-events: none;
  z-index: 2;
  image-rendering: pixelated;
  will-change: transform;
}
.cursor-badge {
  position: absolute;
  top: 0;
  left: 0;
  pointer-events: none;
  z-index: 2;
  /* translate is applied inline from (cursorX, cursorY). Offset the
     arrow tip to the exact pointer hotspot (top-left of the arrow). */
  will-change: transform;
}
.cursor-arrow {
  width: 0;
  height: 0;
  border-left: 14px solid #4fc3f7;
  border-top: 14px solid transparent;
  border-bottom: 14px solid transparent;
  filter: drop-shadow(0 1px 2px rgba(0, 0, 0, 0.45));
  transform: rotate(-20deg);
  transform-origin: 0 0;
}
.cursor-chip {
  position: absolute;
  top: 14px;
  left: 10px;
  background: #4fc3f7;
  color: #0b2530;
  font: 600 11px/1 system-ui, sans-serif;
  padding: 2px 6px;
  border-radius: 8px 8px 8px 2px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.4);
  letter-spacing: 0.5px;
  white-space: nowrap;
}
.no-media-overlay {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.6);
  color: #fff;
  text-align: center;
  padding: 24px;
}
.stats-readout {
  position: absolute;
  top: 8px;
  right: 8px;
  display: flex;
  gap: 6px;
  pointer-events: none;
  z-index: 3;
}
.stats-pill {
  background: rgba(0, 0, 0, 0.55);
  color: rgba(255, 255, 255, 0.9);
  font: 500 11px/1 ui-monospace, "SF Mono", Menlo, monospace;
  padding: 4px 8px;
  border-radius: 999px;
  letter-spacing: 0.3px;
  backdrop-filter: blur(4px);
}
</style>
