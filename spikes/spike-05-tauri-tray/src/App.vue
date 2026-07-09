<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

interface LevelMeterTick {
  tick: number;
  elapsed_ms: number;
  rms: number;
  peak: number;
}

const tick = ref<LevelMeterTick>({ tick: 0, elapsed_ms: 0, rms: 0, peak: 0 });
const receivedCount = ref(0);

function formatElapsed(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`;
}

let unlistenTick: UnlistenFn | undefined;
let unlistenSnapshot: UnlistenFn | undefined;

onMounted(async () => {
  // マウント直後にRust側の現在値を即座に取得する(次のtickを待たずに復元表示するため)。
  tick.value = await invoke<LevelMeterTick>("get_snapshot");

  unlistenTick = await listen<LevelMeterTick>("level-meter", (event) => {
    tick.value = event.payload;
    receivedCount.value += 1;
  });
  unlistenSnapshot = await listen<LevelMeterTick>("level-meter-snapshot", (event) => {
    tick.value = event.payload;
  });
});

onUnmounted(() => {
  unlistenTick?.();
  unlistenSnapshot?.();
});
</script>

<template>
  <main class="container">
    <h1>SPIKE-05: Tauri常駐 レベルメーター</h1>
    <p class="elapsed">{{ formatElapsed(tick.elapsed_ms) }}</p>
    <div class="meter">
      <div class="meter-fill" :style="{ width: `${Math.min(100, tick.rms * 100)}%` }"></div>
      <div class="meter-peak" :style="{ left: `${Math.min(100, tick.peak * 100)}%` }"></div>
    </div>
    <p class="stats">tick={{ tick.tick }} rms={{ tick.rms.toFixed(3) }} peak={{ tick.peak.toFixed(3) }}</p>
    <p class="stats">received (this window instance): {{ receivedCount }}</p>
  </main>
</template>

<style scoped>
.container {
  margin: 0;
  padding-top: 8vh;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.75em;
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
}

.elapsed {
  font-size: 2em;
  font-variant-numeric: tabular-nums;
  margin: 0;
}

.meter {
  position: relative;
  width: 80%;
  height: 24px;
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
  top: -4px;
  width: 2px;
  height: 32px;
  background: white;
}

.stats {
  font-family: monospace;
  opacity: 0.7;
  margin: 0;
}
</style>
