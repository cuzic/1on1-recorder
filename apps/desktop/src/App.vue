<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";

interface Status {
  recording: boolean;
  elapsed_ms: number;
  self_rms: number;
  self_peak: number;
  remote_rms: number;
  remote_peak: number;
  consent_confirmed: boolean;
  uploaded_segments: number;
  pending_segments: number;
  last_error: string | null;
  last_session_id: string | null;
  last_total_duration_ms: number | null;
}

const status = ref<Status>({
  recording: false,
  elapsed_ms: 0,
  self_rms: 0,
  self_peak: 0,
  remote_rms: 0,
  remote_peak: 0,
  consent_confirmed: false,
  uploaded_segments: 0,
  pending_segments: 0,
  last_error: null,
  last_session_id: null,
  last_total_duration_ms: null,
});
const busy = ref(false);
const actionError = ref<string | null>(null);

let pollHandle: ReturnType<typeof setInterval> | undefined;

function formatElapsed(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`;
}

async function refreshStatus() {
  status.value = await invoke<Status>("get_status");
}

async function confirmConsent() {
  actionError.value = null;
  try {
    status.value = await invoke<Status>("confirm_consent");
  } catch (e) {
    actionError.value = String(e);
  }
}

async function startRecording() {
  actionError.value = null;
  busy.value = true;
  try {
    status.value = await invoke<Status>("start_recording");
  } catch (e) {
    actionError.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function stopRecording() {
  actionError.value = null;
  busy.value = true;
  try {
    status.value = await invoke<Status>("stop_recording");
  } catch (e) {
    actionError.value = String(e);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  await refreshStatus();
  pollHandle = setInterval(refreshStatus, 250);
});

onUnmounted(() => {
  if (pollHandle) clearInterval(pollHandle);
});
</script>

<template>
  <main class="container">
    <h1>1on1 Recorder</h1>

    <section v-if="!status.recording" class="panel">
      <h2>Ready to record</h2>
      <p class="hint">Mic: default &middot; Remote source: default</p>

      <label class="consent">
        <input type="checkbox" :checked="status.consent_confirmed" @change="confirmConsent" />
        I consent to this meeting being recorded and uploaded.
      </label>

      <button class="primary" :disabled="busy || !status.consent_confirmed" @click="startRecording">
        Start recording
      </button>

      <p v-if="status.last_session_id" class="hint">
        Last session: {{ status.last_session_id }}
        <span v-if="status.last_total_duration_ms !== null">({{ formatElapsed(status.last_total_duration_ms) }})</span>
      </p>
    </section>

    <section v-else class="panel recording">
      <p class="elapsed"><span class="dot"></span>{{ formatElapsed(status.elapsed_ms) }}</p>

      <div class="meter-row">
        <span class="meter-label">Self</span>
        <div class="meter">
          <div class="meter-fill" :style="{ width: `${Math.min(100, status.self_rms * 100)}%` }"></div>
          <div class="meter-peak" :style="{ left: `${Math.min(100, status.self_peak * 100)}%` }"></div>
        </div>
      </div>
      <div class="meter-row">
        <span class="meter-label">Remote</span>
        <div class="meter">
          <div class="meter-fill" :style="{ width: `${Math.min(100, status.remote_rms * 100)}%` }"></div>
          <div class="meter-peak" :style="{ left: `${Math.min(100, status.remote_peak * 100)}%` }"></div>
        </div>
      </div>

      <p class="stats">Uploaded segments: {{ status.uploaded_segments }} &middot; Pending: {{ status.pending_segments }}</p>

      <button class="stop" :disabled="busy" @click="stopRecording">Stop</button>
    </section>

    <p v-if="status.last_error" class="error">{{ status.last_error }}</p>
    <p v-if="actionError" class="error">{{ actionError }}</p>
  </main>
</template>

<style scoped>
.container {
  margin: 0;
  padding: 6vh 2rem;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 1.25em;
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
}

.panel {
  width: 100%;
  max-width: 360px;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.9em;
}

.hint {
  font-size: 0.85em;
  opacity: 0.7;
  margin: 0;
}

.consent {
  display: flex;
  align-items: center;
  gap: 0.5em;
  font-size: 0.9em;
  text-align: left;
}

button {
  padding: 0.6em 1.4em;
  border-radius: 6px;
  border: none;
  font-size: 1em;
  cursor: pointer;
}

button.primary {
  background: #2ecc71;
  color: #04210f;
}

button.stop {
  background: #e74c3c;
  color: #2a0703;
}

button:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.elapsed {
  font-size: 2em;
  font-variant-numeric: tabular-nums;
  margin: 0;
  display: flex;
  align-items: center;
  gap: 0.4em;
}

.dot {
  width: 12px;
  height: 12px;
  border-radius: 50%;
  background: #e74c3c;
  animation: pulse 1.2s infinite;
}

@keyframes pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.3; }
}

.meter-row {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 0.6em;
}

.meter-label {
  width: 4.5em;
  font-size: 0.85em;
  opacity: 0.8;
}

.meter {
  position: relative;
  flex: 1;
  height: 18px;
  background: #333;
  border-radius: 4px;
  overflow: visible;
}

.meter-fill {
  height: 100%;
  background: linear-gradient(90deg, #2ecc71, #f1c40f, #e74c3c);
  border-radius: 4px;
  transition: width 0.05s linear;
}

.meter-peak {
  position: absolute;
  top: -3px;
  width: 2px;
  height: 24px;
  background: white;
}

.stats {
  font-family: monospace;
  font-size: 0.85em;
  opacity: 0.7;
  margin: 0;
}

.error {
  color: #e74c3c;
  font-size: 0.9em;
  max-width: 360px;
  text-align: center;
}
</style>
