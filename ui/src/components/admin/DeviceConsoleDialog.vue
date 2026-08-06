<template>
  <v-dialog v-model="open" max-width="1100" scrollable>
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-console" color="primary" class="mr-2" />
        <span>Device console — {{ agentName }}</span>
        <v-spacer />
        <v-btn icon="mdi-close" size="small" variant="text" @click="close" aria-label="Close" />
      </v-card-title>

      <!-- Say plainly what running a command here means. This is not a
           terminal on someone's desktop session: the daemon is SYSTEM on
           Windows and root under systemd, so every command is elevated. An
           operator should learn that from the UI, not from an incident. -->
      <v-card-subtitle class="pb-0">
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="my-2"
          icon="mdi-shield-alert-outline"
        >
          Commands run with the Roomler daemon's privileges —
          <strong>SYSTEM</strong> on Windows, <strong>root</strong> on Linux.
          Every attempt is recorded in the org's execution audit log.
        </v-alert>
      </v-card-subtitle>

      <v-card-text style="max-height: 70vh">
        <!-- Explain a closed gate BEFORE the operator types a command and
             watches it get refused. Each gate names its own owner, because
             "ask your org owner" and "turn it on for this device" are
             different next steps. -->
        <v-alert
          v-if="blockedReason"
          type="info"
          variant="tonal"
          density="compact"
          class="mb-3"
        >
          {{ blockedReason }}
        </v-alert>

        <div class="d-flex align-center flex-wrap gap-2 mb-3">
          <v-select
            v-model="shell"
            :items="shellOptions"
            label="Shell"
            density="compact"
            variant="outlined"
            hide-details
            style="max-width: 180px"
          />
          <v-text-field
            v-model.number="timeoutSecs"
            label="Timeout (s)"
            type="number"
            density="compact"
            variant="outlined"
            hide-details
            style="max-width: 130px"
            :min="1"
            :max="300"
          />
          <v-spacer />
          <v-btn
            v-if="running"
            color="warning"
            variant="tonal"
            prepend-icon="mdi-stop"
            @click="cancel"
          >
            Cancel
          </v-btn>
        </div>

        <v-textarea
          v-model="command"
          label="Command"
          rows="3"
          auto-grow
          density="compact"
          variant="outlined"
          hide-details
          class="mb-2 font-mono"
          :disabled="running"
          @keydown.ctrl.enter.prevent="run"
        />
        <div class="d-flex align-center mb-4">
          <span class="text-caption text-medium-emphasis">Ctrl+Enter to run</span>
          <v-spacer />
          <v-btn
            color="primary"
            prepend-icon="mdi-play"
            :loading="running"
            :disabled="!command.trim()"
            @click="run"
          >
            Run
          </v-btn>
        </div>

        <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mb-3">
          {{ error }}
        </v-alert>

        <div v-for="(entry, i) in history" :key="entry.request_id || i" class="mb-4">
          <div class="d-flex align-center flex-wrap gap-2 mb-1">
            <v-chip size="small" :color="statusColor(entry)" variant="flat">
              {{ statusLabel(entry) }}
            </v-chip>
            <code class="text-caption">{{ entry.shell || 'default' }}</code>
            <span class="text-caption text-medium-emphasis">{{ entry.duration_ms }} ms</span>
            <v-chip v-if="entry.truncated" size="x-small" color="warning" variant="tonal">
              output truncated
            </v-chip>
            <v-spacer />
            <v-btn
              icon="mdi-content-copy"
              size="x-small"
              variant="text"
              aria-label="Copy output"
              @click="copyOutput(entry)"
            />
          </div>
          <pre class="console-cmd">{{ entry.command }}</pre>
          <pre v-if="entry.error" class="console-err">{{ entry.error }}</pre>
          <pre v-if="entry.stdout" class="console-out">{{ entry.stdout }}</pre>
          <pre v-if="entry.stderr" class="console-err">{{ entry.stderr }}</pre>
        </div>

        <div v-if="!history.length && !running" class="text-medium-emphasis text-caption">
          No commands run in this session yet.
        </div>
      </v-card-text>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useAgentStore, type Agent, type ExecResult } from '@/stores/agents'

const props = defineProps<{
  modelValue: boolean
  tenantId: string
  agent: Agent
}>()

const emit = defineEmits<{
  (e: 'update:modelValue', v: boolean): void
}>()

const open = computed({
  get: () => props.modelValue,
  set: (v) => emit('update:modelValue', v),
})

const agentStore = useAgentStore()
const agentName = computed(() => props.agent?.name ?? '')

const command = ref('')
const shell = ref('')
const timeoutSecs = ref(30)
const running = ref(false)
const error = ref<string | null>(null)
/** Newest first, so the latest result is where the eye already is. */
const history = ref<Array<ExecResult & { command: string; shell: string }>>([])
/** The in-flight request, so Cancel has something to address. */
const inFlight = ref<string | null>(null)

/** Windows and Unix hosts offer different shells; showing `bash` for a
 *  Windows box would only produce an "unsupported shell" round trip. */
const shellOptions = computed(() =>
  props.agent?.os === 'windows'
    ? [
        { title: 'Host default (PowerShell)', value: '' },
        { title: 'powershell', value: 'powershell' },
        { title: 'pwsh', value: 'pwsh' },
        { title: 'cmd', value: 'cmd' },
      ]
    : [
        { title: 'Host default (bash)', value: '' },
        { title: 'bash', value: 'bash' },
        { title: 'sh', value: 'sh' },
      ],
)

/** Which gate is shut, in the order the server evaluates them, phrased as
 *  the action the reader can actually take. `null` = nothing known to be
 *  blocking. */
const blockedReason = computed<string | null>(() => {
  if (agentStore.orgExecEnabled === false) {
    return 'Remote execution is switched off for this organization. An org owner can enable it in Settings.'
  }
  if ((props.agent?.exec_policy?.mode ?? 'off') !== 'on') {
    return 'This device does not accept remote commands. Enable it in the device’s execution policy.'
  }
  if (props.agent && !props.agent.is_online && props.agent.presence !== 'online') {
    return 'This device is offline — commands will fail until it reconnects.'
  }
  if (!(props.agent?.capabilities?.rpc ?? []).includes('exec')) {
    return `This device runs agent ${props.agent?.agent_version ?? '?'}, which predates remote execution. Update it first.`
  }
  return null
})

function statusLabel(e: ExecResult): string {
  if (e.error) return 'refused'
  if (e.exit_code === 0) return 'exit 0'
  return `exit ${e.exit_code ?? '?'}`
}

function statusColor(e: ExecResult): string {
  if (e.error) return 'error'
  return e.exit_code === 0 ? 'success' : 'warning'
}

async function run() {
  const cmd = command.value.trim()
  if (!cmd || running.value) return
  running.value = true
  error.value = null
  const usedShell = shell.value
  try {
    const res = await agentStore.execOnAgent(props.tenantId, props.agent.id, {
      shell: usedShell,
      command: cmd,
      timeout_ms: Math.max(1, Math.min(300, timeoutSecs.value || 30)) * 1000,
    })
    inFlight.value = res.request_id
    history.value.unshift({ ...res, command: cmd, shell: usedShell })
  } catch (e) {
    // Only transport / validation failures land here — a policy refusal
    // comes back as a normal result carrying `error`.
    error.value = (e as Error).message
  } finally {
    running.value = false
    inFlight.value = null
  }
}

async function cancel() {
  if (!inFlight.value) return
  try {
    await agentStore.cancelExec(props.tenantId, props.agent.id, inFlight.value)
  } catch (e) {
    error.value = (e as Error).message
  }
}

async function copyOutput(e: ExecResult & { command: string }) {
  const text = [e.command, e.stdout, e.stderr, e.error].filter(Boolean).join('\n')
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    // Clipboard permission denied — not worth an error banner over.
  }
}

function close() {
  open.value = false
}

watch(
  () => props.modelValue,
  (v) => {
    if (!v) return
    error.value = null
    if (agentStore.orgExecEnabled === null) {
      void agentStore.fetchOrgExecEnabled(props.tenantId)
    }
  },
)
</script>

<style scoped>
.console-cmd,
.console-out,
.console-err {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 0.8rem;
  white-space: pre-wrap;
  word-break: break-word;
  padding: 0.5rem 0.75rem;
  border-radius: 4px;
  margin: 0 0 2px;
  /* Scroll long output inside its own box — the dialog itself must never
     scroll sideways. */
  overflow-x: auto;
}
.console-cmd {
  background: rgba(var(--v-theme-primary), 0.08);
}
.console-out {
  background: rgba(var(--v-theme-on-surface), 0.04);
}
.console-err {
  background: rgba(var(--v-theme-error), 0.08);
}
.font-mono :deep(textarea) {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
</style>
