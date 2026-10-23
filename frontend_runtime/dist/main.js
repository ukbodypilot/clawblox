import * as threeSdk from "./sdk/three.js"
import * as inputSdk from "./sdk/input.js"
import {
  createElement as createLucideElement,
  Wrench,
  CircleAlert,
  User,
} from "https://esm.sh/lucide@0.468.0"

function icon(iconNode, size = 14) {
  const el = createLucideElement(iconNode)
  el.setAttribute('width', String(size))
  el.setAttribute('height', String(size))
  el.setAttribute('aria-hidden', 'true')
  return el
}

const API_VERSION = 1
const BASE_PATH = (() => {
  try {
    const url = new URL('..', import.meta.url)
    const path = url.pathname.replace(/\/$/, '')
    return path || ''
  } catch (_) {
    return ''
  }
})()

function withBasePath(path) {
  const value = String(path || '')
  if (!value) return BASE_PATH || '/'
  if (/^https?:\/\//i.test(value) || /^wss?:\/\//i.test(value)) return value
  if (!value.startsWith('/')) return value
  return `${BASE_PATH}${value}` || value
}

const SPECTATOR_TOKEN = (() => {
  const token = new URLSearchParams(window.location.search).get('spectator_token')
  return token ? token.trim() : ''
})()

const RUNTIME_VIEWPORT_ONLY = new URLSearchParams(window.location.search).get('viewport') === '1'

function withSpectatorToken(path) {
  const url = new URL(withBasePath(path), window.location.origin)
  if (SPECTATOR_TOKEN) url.searchParams.set('spectator_token', SPECTATOR_TOKEN)
  return `${url.pathname}${url.search}${url.hash}`
}

const statusEl = document.getElementById('status')
const metaEl = document.getElementById('meta')
const canvas = document.getElementById('stage')
const chatToggleEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-toggle')
const chatPanelEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-panel')
const chatFeedEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-feed')
const chatFormEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-form')
const chatInputEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-input')
const chatSendEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-send')
const chatConnectionEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-connection')
const chatLocalStatusEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('chat-local-status')
const playerListCountEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('player-list-count')
const playerListBodyEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('player-list-body')
const behaviorPanelEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-panel')
const behaviorPanelToggleEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-panel-toggle')
const behaviorTimelineShellEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-timeline-shell')
const behaviorRulerEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-ruler')
const behaviorTracksEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-tracks')
const behaviorGlobalPlayheadEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-global-playhead')
const behaviorDetailsEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('behavior-details')
const replayPanelEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-panel')
const replayPlayToggleEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-play-toggle')
const replayPlayIconEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-play-icon')
const replaySliderEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-slider')
const replayTimeCurrentEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-time-current')
const replayTimeTotalEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-time-total')
const replaySpeedEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-speed')
const replaySkipIdleEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('replay-skip-idle')
const agentLogPanelEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('agent-log-panel')
const agentLogFeedEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('agent-log-feed')
const workspaceFeedEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('workspace-feed')
const agentLogStatusEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('agent-log-status')
const agentLogToggleEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('agent-log-toggle')
const agentLogTabMessagesEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('agent-log-tab-messages')
const agentLogTabWorkspaceEl = RUNTIME_VIEWPORT_ONLY ? null : document.getElementById('agent-log-tab-workspace')
const replayChromeAvailable = !!replayPanelEl
const agentPanelAvailable = !!agentLogPanelEl
const workspacePanelAvailable = !!workspaceFeedEl
const behaviorPanelAvailable = !!behaviorPanelEl

const chatState = {
  open: false,
  wsConnected: false,
  loadedHistory: false,
  sending: false,
  messages: [],
  seenIds: new Set(),
  sessionToken: null,
  agentId: null,
  playerName: null,
  spectatorChat: false,
}
let selectedSpectatorPlayerId = null
let latestSpectatorState = null

function applySpectatorDelta(baseState, delta) {
  if (!baseState || !delta || delta.type !== 'spectator_delta') return null
  const changedEntityIds = []
  const removedEntityIds = []
  const addedEntityIds = []

  const next = {
    ...baseState,
    tick: typeof delta.tick === 'number' ? delta.tick : baseState.tick,
    server_time_ms: typeof delta.server_time_ms === 'number' ? delta.server_time_ms : baseState.server_time_ms,
    game_status: typeof delta.game_status === 'string' ? delta.game_status : baseState.game_status,
    players: Array.isArray(delta.players) ? delta.players : (baseState.players || []),
    entities: Array.isArray(baseState.entities) ? baseState.entities.slice() : [],
    sounds: Array.isArray(delta.sounds) ? delta.sounds : (baseState.sounds || []),
    recent_events: Array.isArray(delta.recent_events) ? delta.recent_events : [],
  }

  if (!Array.isArray(delta.entities) || delta.entities.length === 0) {
    next.__spectatorDelta = { changedEntityIds, removedEntityIds, addedEntityIds }
    return next
  }

  const entityIndex = new Map()
  for (let i = 0; i < next.entities.length; i += 1) {
    const id = next.entities[i]?.id
    if (typeof id === 'number') entityIndex.set(id, i)
  }

  const removedIds = new Set()

  for (const entry of delta.entities) {
    const id = entry?.id
    if (typeof id !== 'number') continue

    if (entry.rm) {
      removedIds.add(id)
      removedEntityIds.push(id)
      continue
    }

    if (entry.add && typeof entry.add === 'object') {
      const index = entityIndex.get(id)
      if (index == null) {
        entityIndex.set(id, next.entities.length)
        next.entities.push(entry.add)
      } else {
        next.entities[index] = entry.add
      }
      removedIds.delete(id)
      addedEntityIds.push(id)
      changedEntityIds.push(id)
      continue
    }

    const index = entityIndex.get(id)
    if (index == null) continue
    const current = next.entities[index]
    next.entities[index] = {
      ...current,
      position: Array.isArray(entry.p) ? entry.p : current.position,
      rotation: Array.isArray(entry.r) ? entry.r : current.rotation,
    }
    changedEntityIds.push(id)
  }

  if (removedIds.size > 0) {
    next.entities = next.entities.filter((entity) => !removedIds.has(entity?.id))
  }

  next.__spectatorDelta = { changedEntityIds, removedEntityIds, addedEntityIds }
  return next
}

const replayState = {
  enabled: false,
  info: null,
  pollTimer: null,
  scrubbing: false,
  agentEnabled: false,
  agentLoaded: false,
  agentMessages: [],
  workspaceEnabled: false,
  workspaceLoaded: false,
  workspaceEvents: [],
  sidePanelTab: 'messages',
  agentPanelOpen: readStoredBool('clawblox_agent_log_open', true),
  agentExpandedBlocks: new Set(),
  metadataEnabled: false,
  metadataLoaded: false,
  metadata: null,
  clientDriven: false,
  externalViewEnabled: false,
  externalViewUrl: null,
  behaviorPanelOpen: readStoredBool('clawblox_behavior_panel_open', false),
  selectedBehaviorId: null,
  selectedOccurrenceId: null,
  behaviorViewStartTick: null,
  behaviorViewEndTick: null,
  behaviorPanState: null,
}
const replayPlaybackClock = {
  initialized: false,
  tick: 0,
  carryTicks: 0,
  lastNowMs: 0,
  seekVersion: null,
}
const replayViewportWarmup = {
  minQueueDepth: 20,
  minWarmupFrames: 6,
  minPostRestoreWarmupFrames: 3,
  warmupFrames: 0,
  postRestoreWarmupFrames: 0,
  maxWarmupRenderMs: 0,
  assetsSettled: false,
  restored: false,
  postRestoreWarmupComplete: false,
  restoredFrameConsumed: false,
  renderedFrame: false,
  ready: false,
  listeners: new Set(),
}
let replayViewportWarmupPump = null
const replayFrameQueue = []
const replayFrameQueueTarget = 60
const replayFrameQueueLowWater = 20
const replayLongTasks = {
  count: 0,
  totalMs: 0,
  maxMs: 0,
}
if (typeof PerformanceObserver !== 'undefined') {
  try {
    const observer = new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        const duration = Number(entry.duration)
        if (!Number.isFinite(duration) || duration <= 0) continue
        replayLongTasks.count += 1
        replayLongTasks.totalMs += duration
        replayLongTasks.maxMs = Math.max(replayLongTasks.maxMs, duration)
      }
    })
    observer.observe({ type: 'longtask', buffered: true })
  } catch {
    // Long Task API is optional.
  }
}
let pendingReplayCameraState = null
let lastCameraReportMs = 0
const internalApiRegistry = new Map()

function normalizeInternalApiName(name) {
  const value = String(name || '').trim()
  return value || null
}

function installInternalApiFacade() {
  Object.defineProperty(window, '__clawbloxInternal', {
    value: {
      get(name) {
        const key = normalizeInternalApiName(name)
        return key ? (internalApiRegistry.get(key) || null) : null
      },
      has(name) {
        const key = normalizeInternalApiName(name)
        return !!(key && internalApiRegistry.has(key))
      },
      keys() {
        return [...internalApiRegistry.keys()]
      },
    },
    configurable: true,
    enumerable: false,
    writable: false,
  })
}

function setInternalApi(name, api) {
  const key = normalizeInternalApiName(name)
  if (!key) throw new Error('internal api name required')
  if (api == null) {
    internalApiRegistry.delete(key)
    return null
  }
  internalApiRegistry.set(key, api)
  if (key === 'camera') applyPendingReplayCameraState()
  return api
}

function removeInternalApi(name) {
  const key = normalizeInternalApiName(name)
  if (!key) return false
  return internalApiRegistry.delete(key)
}

installInternalApiFacade()

// Capture hook for server-driven frame capture (agent vision). The engine's
// capture sidecar drives this over CDP: set a camera pose, wait for the frame
// counter to advance past the returned value, then screenshot.
const captureHook = {
  frame: 0,
  tick: 0,
  setPose(pose) {
    const api = internalApiRegistry.get('camera')
    if (!api || typeof api.setPoseOverride !== 'function') {
      return { ok: false, error: 'camera api unavailable', frame: captureHook.frame, tick: captureHook.tick }
    }
    const state = api.setPoseOverride(pose)
    return { ok: !!state?.poseOverride, frame: captureHook.frame, tick: captureHook.tick }
  },
  clearPose() {
    const api = internalApiRegistry.get('camera')
    if (api && typeof api.clearPoseOverride === 'function') api.clearPoseOverride()
    return { ok: true, frame: captureHook.frame, tick: captureHook.tick }
  },
  attach(cfg) {
    const api = internalApiRegistry.get('camera')
    if (!api || typeof api.setFirstPersonAttach !== 'function') {
      return { ok: false, error: 'camera api unavailable', frame: captureHook.frame, tick: captureHook.tick }
    }
    const state = api.setFirstPersonAttach(cfg)
    return { ok: !!state?.attach, frame: captureHook.frame, tick: captureHook.tick }
  },
  detach() {
    const api = internalApiRegistry.get('camera')
    if (api && typeof api.clearFirstPersonAttach === 'function') api.clearFirstPersonAttach()
    return { ok: true, frame: captureHook.frame, tick: captureHook.tick }
  },
  status() {
    return {
      frame: captureHook.frame,
      tick: captureHook.tick,
      hasCamera: internalApiRegistry.has('camera'),
    }
  },
}
Object.defineProperty(window, '__clawbloxCapture', {
  value: captureHook,
  configurable: true,
  enumerable: false,
  writable: false,
})

function readStoredBool(key, fallback) {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return fallback
    return raw === '1'
  } catch (_) {
    return fallback
  }
}

function storeBool(key, value) {
  try {
    localStorage.setItem(key, value ? '1' : '0')
  } catch (_) {}
}

function setStatus(text, level = 'muted') {
  statusEl.textContent = text
  if (level === 'ok') statusEl.style.color = 'var(--ok)'
  else if (level === 'warn') statusEl.style.color = 'var(--warn)'
  else if (level === 'error') statusEl.style.color = 'var(--error)'
  else statusEl.style.color = 'var(--muted)'
}

function setChatConnection(text) {
  if (chatConnectionEl) chatConnectionEl.textContent = text
}

function setAgentLogStatus(text) {
  if (agentLogStatusEl) agentLogStatusEl.textContent = text
}

function setSidePanelTab(tab) {
  replayState.sidePanelTab = tab === 'workspace' ? 'workspace' : 'messages'
  const workspaceActive = replayState.sidePanelTab === 'workspace'
  if (agentLogFeedEl) agentLogFeedEl.hidden = workspaceActive
  if (workspaceFeedEl) workspaceFeedEl.hidden = !workspaceActive
  if (agentLogTabMessagesEl) {
    agentLogTabMessagesEl.classList.toggle('is-active', !workspaceActive)
    agentLogTabMessagesEl.setAttribute('aria-selected', workspaceActive ? 'false' : 'true')
  }
  if (agentLogTabWorkspaceEl) {
    agentLogTabWorkspaceEl.classList.toggle('is-active', workspaceActive)
    agentLogTabWorkspaceEl.setAttribute('aria-selected', workspaceActive ? 'true' : 'false')
  }
  renderSidePanel(replayState.info?.current_tick || 0)
}

function setAgentLogPanelOpen(open, persist = true) {
  replayState.agentPanelOpen = !!open
  if (agentLogPanelEl) {
    agentLogPanelEl.classList.toggle('is-open', replayState.agentPanelOpen)
  }
  if (agentLogToggleEl) {
    agentLogToggleEl.setAttribute('aria-expanded', replayState.agentPanelOpen ? 'true' : 'false')
    agentLogToggleEl.setAttribute('aria-label', replayState.agentPanelOpen ? 'Close agent messages' : 'Open agent messages')
    agentLogToggleEl.title = replayState.agentPanelOpen ? 'Hide agent messages' : 'Show agent messages'
  }
  if (persist) storeBool('clawblox_agent_log_open', replayState.agentPanelOpen)
}

function syncAgentLogVisibility() {
  const shouldShow = replayState.enabled && (replayState.agentEnabled || replayState.workspaceEnabled)
  if (agentLogPanelEl) {
    agentLogPanelEl.hidden = !shouldShow
    agentLogPanelEl.classList.toggle('is-open', shouldShow && replayState.agentPanelOpen)
  }
  if (agentLogToggleEl) agentLogToggleEl.hidden = !shouldShow
}

function setBehaviorPanelOpen(open, persist = true) {
  replayState.behaviorPanelOpen = !!open
  if (behaviorPanelEl) {
    behaviorPanelEl.classList.toggle('is-open', replayState.behaviorPanelOpen)
  }
  if (behaviorPanelToggleEl) {
    behaviorPanelToggleEl.setAttribute('aria-label', replayState.behaviorPanelOpen ? 'Hide behaviors' : 'Show behaviors')
    behaviorPanelToggleEl.title = replayState.behaviorPanelOpen ? 'Hide behaviors' : 'Show behaviors'
  }
  if (persist) storeBool('clawblox_behavior_panel_open', replayState.behaviorPanelOpen)
  syncBehaviorPanelVisibility()
  if (replayState.behaviorPanelOpen) renderBehaviorPanel()
}

function syncBehaviorPanelVisibility() {
  const shouldShow = replayState.enabled && replayState.metadataEnabled
  if (behaviorPanelEl) {
    behaviorPanelEl.hidden = !shouldShow
    behaviorPanelEl.classList.toggle('is-open', shouldShow && replayState.behaviorPanelOpen)
  }
  if (behaviorPanelToggleEl) behaviorPanelToggleEl.hidden = !shouldShow
}

function formatDurationMs(ms) {
  const totalSeconds = Math.max(0, Math.floor((Number(ms) || 0) / 1000))
  const hours = Math.floor(totalSeconds / 3600)
  const minutes = Math.floor((totalSeconds % 3600) / 60)
  const seconds = totalSeconds % 60
  if (hours > 0) return `${hours}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`
  return `${minutes}:${String(seconds).padStart(2, '0')}`
}

function tickToMs(tick, info) {
  if (!info || !Number.isFinite(info.tick_rate) || info.tick_rate <= 0) return 0
  return Math.max(0, Math.round(((tick - info.first_tick) / info.tick_rate) * 1000))
}

function setReplayMode(enabled) {
  replayState.enabled = !!enabled
  if (!replayState.enabled) {
    replayState.agentLoaded = false
    replayState.agentMessages = []
    replayState.workspaceLoaded = false
    replayState.workspaceEnabled = false
    replayState.workspaceEvents = []
    replayState.metadataLoaded = false
    replayState.metadataEnabled = false
    replayState.metadata = null
    replayState.selectedBehaviorId = null
    replayState.selectedOccurrenceId = null
    replayState.behaviorViewStartTick = null
    replayState.behaviorViewEndTick = null
    replayState.behaviorPanState = null
  }
  document.body.dataset.mode = replayState.enabled ? 'replay' : 'live'
  if (replayPanelEl) replayPanelEl.hidden = !replayState.enabled
  syncAgentLogVisibility()
  syncBehaviorPanelVisibility()
  if (replayState.enabled) {
    setChatLocalStatus('Replay')
    setChatConnection('Replay')
    setAgentLogStatus('Syncing...')
  }
}

function setReplayPlayUi(paused) {
  if (!replayPlayIconEl || !replayPlayToggleEl) return
  if (paused) {
    replayPlayIconEl.innerHTML = '<path d="M8 5v14l11-7z"/>'
    replayPlayToggleEl.setAttribute('aria-label', 'Play replay')
    replayPlayToggleEl.title = 'Play replay'
  } else {
    replayPlayIconEl.innerHTML = '<path d="M7 5h3v14H7zm7 0h3v14h-3z"/>'
    replayPlayToggleEl.setAttribute('aria-label', 'Pause replay')
    replayPlayToggleEl.title = 'Pause replay'
  }
}

function syncReplayPlaybackClock(info, { force = false } = {}) {
  if (!info) return
  const tick = Number(info.current_tick ?? info.first_tick ?? 0)
  const seekVersion = Number.isFinite(Number(info.seek_version)) ? Number(info.seek_version) : null
  if (!replayPlaybackClock.initialized || force || info.paused || seekVersion !== replayPlaybackClock.seekVersion) {
    replayPlaybackClock.initialized = true
    replayPlaybackClock.tick = Number.isFinite(tick) ? tick : 0
    replayPlaybackClock.carryTicks = 0
    replayPlaybackClock.lastNowMs = performance.now()
    replayPlaybackClock.seekVersion = seekVersion
  }
}

function stepReplayPlaybackClock(nowMs = performance.now()) {
  const info = replayState.info || {}
  if (!replayPlaybackClock.initialized) syncReplayPlaybackClock(info, { force: true })
  const elapsedMs = Math.max(0, nowMs - replayPlaybackClock.lastNowMs)
  replayPlaybackClock.lastNowMs = nowMs
  const tickRate = Math.max(0, Number(info.tick_rate || 60))
  const speed = Math.max(0, Number(info.speed ?? 1))
  const paused = !!info.paused
  const firstTick = Math.max(0, Number(info.first_tick || 0))
  const totalTicks = Math.max(firstTick, Number(info.total_ticks || firstTick))
  const maxStepTicks = Math.max(1, Math.ceil((tickRate * Math.max(1, speed)) / 60))
  if (paused || speed === 0 || tickRate === 0) {
    return {
      tick: Math.max(firstTick, Math.min(totalTicks, Math.round(replayPlaybackClock.tick))),
      elapsedMs,
      rawAdvanceTicks: 0,
      appliedAdvanceTicks: 0,
      droppedTicks: 0,
      paused: true,
    }
  }
  const exactAdvance = replayPlaybackClock.carryTicks + (elapsedMs / 1000) * tickRate * speed
  const rawAdvanceTicks = Math.floor(exactAdvance)
  const appliedAdvanceTicks = Math.min(rawAdvanceTicks, maxStepTicks)
  const droppedTicks = Math.max(0, rawAdvanceTicks - appliedAdvanceTicks)
  replayPlaybackClock.carryTicks = 0
  replayPlaybackClock.tick = Math.max(firstTick, Math.min(totalTicks, Math.round(replayPlaybackClock.tick + appliedAdvanceTicks)))
  return {
    tick: replayPlaybackClock.tick,
    elapsedMs,
    rawAdvanceTicks,
    appliedAdvanceTicks,
    droppedTicks,
    paused: false,
  }
}

function consumeReplayLongTaskMetrics() {
  const metrics = {
    long_task_count: replayLongTasks.count,
    long_task_total_ms: replayLongTasks.totalMs,
    long_task_max_ms: replayLongTasks.maxMs,
  }
  replayLongTasks.count = 0
  replayLongTasks.totalMs = 0
  replayLongTasks.maxMs = 0
  return metrics
}

function renderReplayInfo(info) {
  const previousInfo = replayState.info
  replayState.info = info
  if (!replayState.enabled || !info) return
  replayState.clientDriven = info.client_driven === true
  const previousSeekVersion = Number.isFinite(Number(previousInfo?.seek_version)) ? Number(previousInfo.seek_version) : null
  const seekVersion = Number.isFinite(Number(info.seek_version)) ? Number(info.seek_version) : null
  const wasPaused = !!previousInfo?.paused
  const paused = !!info.paused
  if (replayState.clientDriven && previousSeekVersion !== null && seekVersion !== previousSeekVersion) {
    clearReplayFrameQueue()
  }
  syncReplayPlaybackClock(info, {
    force: seekVersion !== previousSeekVersion || (wasPaused && !paused),
  })
  setReplayPlayUi(!!info.paused)
  if (replaySliderEl) {
    replaySliderEl.min = String(info.first_tick || 0)
    replaySliderEl.max = String(info.total_ticks || info.first_tick || 0)
    if (!replayState.scrubbing) replaySliderEl.value = String(info.current_tick || info.first_tick || 0)
  }
  if (replayTimeCurrentEl) replayTimeCurrentEl.textContent = formatDurationMs(tickToMs(info.current_tick || 0, info))
  if (replayTimeTotalEl) replayTimeTotalEl.textContent = formatDurationMs(info.duration_ms || 0)
  if (replaySpeedEl) {
    const value = String(info.speed ?? 1)
    if ([...replaySpeedEl.options].some((opt) => opt.value === value)) {
      replaySpeedEl.value = value
    }
  }
  if (replaySkipIdleEl) replaySkipIdleEl.checked = !!info.skip_idle
  if (replayState.behaviorPanelOpen) renderBehaviorPanel()
  renderSidePanel(info.current_tick || info.first_tick || 0)
}

function notifyReplayViewportWarmup() {
  if (!replayViewportWarmup.assetsSettled) return
  if (!replayState.clientDriven) {
    if (!replayViewportWarmup.renderedFrame) return
    replayViewportWarmup.ready = true
    const listeners = [...replayViewportWarmup.listeners]
    replayViewportWarmup.listeners.clear()
    for (const listener of listeners) listener()
    return
  }
  if (replayViewportWarmup.warmupFrames < replayViewportWarmup.minWarmupFrames) return
  if (!replayViewportWarmup.restored) return
  if (!replayViewportWarmup.postRestoreWarmupComplete) return
  if (replayState.clientDriven && replayFrameQueue.length < replayViewportWarmup.minQueueDepth) return
  replayViewportWarmup.ready = true
  const listeners = [...replayViewportWarmup.listeners]
  replayViewportWarmup.listeners.clear()
  for (const listener of listeners) listener()
}

function whenReplayViewportWarm() {
  notifyReplayViewportWarmup()
  if (replayViewportWarmup.ready) return Promise.resolve()
  return new Promise((resolve) => replayViewportWarmup.listeners.add(resolve))
}

function markReplayViewportAssetsSettled() {
  if (!replayViewportWarmup.assetsSettled) {
    replayViewportWarmup.assetsSettled = true
    replayViewportWarmup.warmupFrames = 0
    replayViewportWarmup.postRestoreWarmupFrames = 0
    replayViewportWarmup.maxWarmupRenderMs = 0
    replayViewportWarmup.restored = false
    replayViewportWarmup.postRestoreWarmupComplete = false
    replayViewportWarmup.restoredFrameConsumed = false
    replayViewportWarmup.ready = false
  }
  notifyReplayViewportWarmup()
  if (!replayViewportWarmup.ready && typeof replayViewportWarmupPump === 'function') {
    replayViewportWarmupPump()
  }
}

function renderAgentLogMessages(currentTick, payload = null) {
  if (!agentLogFeedEl) return
  if (payload) {
    const enabled = !!payload?.enabled
    replayState.agentEnabled = enabled
    replayState.agentLoaded = true
    replayState.agentMessages = Array.isArray(payload?.messages) ? payload.messages : []
    replayState.agentRenderedSig = null
  }
  const enabled = replayState.agentEnabled || replayState.workspaceEnabled
  syncAgentLogVisibility()

  if (!enabled) {
    agentLogFeedEl.textContent = ''
    replayState.agentRenderedSig = null
    setAgentLogStatus('No file')
    return
  }

  const allMessages = Array.isArray(replayState.agentMessages) ? replayState.agentMessages : []
  const endTick = Number((currentTick ?? replayState.info?.current_tick) || 0)
  const messages = allMessages.filter((msg) => Number(msg?.tick) <= endTick + 1e-6)
  const workspaceState = buildWorkspaceState(endTick)
  setAgentLogStatus(replayState.sidePanelTab === 'workspace'
    ? `${workspaceState.length} files`
    : `${messages.length}/${allMessages.length}`)

  if (!allMessages.length) {
    agentLogFeedEl.textContent = ''
    replayState.agentRenderedSig = 'empty'
    const empty = document.createElement('div')
    empty.className = 'agent-log-empty'
    empty.textContent = 'No agent messages in this replay.'
    agentLogFeedEl.appendChild(empty)
    return
  }
  if (!messages.length) {
    agentLogFeedEl.textContent = ''
    replayState.agentRenderedSig = 'empty-tick'
    const empty = document.createElement('div')
    empty.className = 'agent-log-empty'
    empty.textContent = 'No messages yet at this replay point.'
    agentLogFeedEl.appendChild(empty)
    return
  }

  const lastMessage = messages[messages.length - 1] || {}
  const sig = `${messages.length}|${lastMessage.id || ''}|${lastMessage.timestamp_ms || ''}`
  const sigChanged = replayState.agentRenderedSig !== sig

  if (sigChanged) {
    const shouldStick = userAtBottom(agentLogFeedEl)
    const prevScrollTop = agentLogFeedEl.scrollTop
    const prevScrollHeight = agentLogFeedEl.scrollHeight
    agentLogFeedEl.textContent = ''
    for (const msg of messages) {
      const row = buildAgentLogMessage(msg, endTick)
      if (row) agentLogFeedEl.appendChild(row)
    }
    replayState.agentRenderedSig = sig
    if (shouldStick) {
      agentLogFeedEl.scrollTop = agentLogFeedEl.scrollHeight
    } else {
      const growth = agentLogFeedEl.scrollHeight - prevScrollHeight
      agentLogFeedEl.scrollTop = Math.max(0, prevScrollTop + growth)
    }
  } else {
    updateCurrentTickHighlight(endTick)
  }
}

function applyWorkspaceEvents(payload) {
  replayState.workspaceEnabled = !!payload?.enabled
  replayState.workspaceLoaded = true
  replayState.workspaceEvents = Array.isArray(payload?.events) ? payload.events : []
  replayState.workspaceRenderedSig = null
  syncAgentLogVisibility()
}

function renderSidePanel(currentTick) {
  if (replayState.sidePanelTab === 'workspace') {
    renderWorkspaceAtTick(currentTick)
  } else {
    renderAgentLogMessages(currentTick)
  }
}

function updateCurrentTickHighlight(endTick) {
  const rows = agentLogFeedEl.querySelectorAll('.agent-log-message')
  rows.forEach((row) => {
    const tickNum = Number(row.dataset.tick)
    const isCurrent = Number.isFinite(tickNum) && Math.abs(tickNum - endTick) <= 1
    row.classList.toggle('is-current', isCurrent)
  })
}

function buildAgentLogMessage(msg, endTick) {
  const blocks = Array.isArray(msg?.blocks) ? msg.blocks : []
  if (!blocks.length) {
    return null
  }
  const row = document.createElement('div')
  row.className = 'agent-log-message'
  const role = String(msg?.role || 'assistant')
  row.dataset.role = role
  const tickNum = Number(msg?.tick)
  if (Number.isFinite(tickNum)) row.dataset.tick = String(tickNum)
  if (Number.isFinite(tickNum) && Math.abs(tickNum - endTick) <= 1) {
    row.classList.add('is-current')
  }

  const body = document.createElement('div')
  body.className = 'agent-log-message__body'
  if (role === 'user') {
    const badge = document.createElement('span')
    badge.className = 'agent-log-message__user-badge'
    badge.appendChild(icon(User, 11))
    row.appendChild(badge)
  }

  const msgId = String(msg?.id || msg?.timestamp_ms || '')
  for (let i = 0; i < blocks.length; i++) {
    const el = buildAgentLogBlock(blocks[i], `${msgId}:${i}`)
    if (el) body.appendChild(el)
  }

  row.appendChild(body)
  return row
}

function buildWorkspaceState(currentTick) {
  const files = new Map()
  const events = Array.isArray(replayState.workspaceEvents) ? replayState.workspaceEvents : []
  for (const event of events) {
    if (Number(event?.tick) > currentTick + 1e-6) continue
    const changes = Array.isArray(event?.changes) ? event.changes : []
    for (const change of changes) {
      const path = String(change?.path || '')
      if (!path) continue
      const op = String(change?.op || '')
      if (op === 'delete') {
        files.delete(path)
      } else {
        files.set(path, {
          path,
          op,
          size: change?.size,
          sha256: change?.sha256,
          omitted: change?.omitted,
          tick: Number(event?.tick) || 0,
          agent: String(event?.agent_name || event?.agent_id || ''),
        })
      }
    }
  }
  return [...files.values()].sort((a, b) => a.path.localeCompare(b.path))
}

function renderWorkspaceAtTick(currentTick) {
  if (!workspaceFeedEl) return
  const endTick = Number((currentTick ?? replayState.info?.current_tick) || 0)
  const files = buildWorkspaceState(endTick)
  const sig = `${files.length}|${files.map((file) => `${file.path}:${file.sha256 || file.omitted || ''}`).join('|')}`
  setAgentLogStatus(`${files.length} files`)

  if (!replayState.workspaceEnabled) {
    workspaceFeedEl.textContent = ''
    replayState.workspaceRenderedSig = null
    const empty = document.createElement('div')
    empty.className = 'agent-log-empty'
    empty.textContent = 'No workspace recording in this replay.'
    workspaceFeedEl.appendChild(empty)
    return
  }
  if (replayState.workspaceRenderedSig === sig) return

  workspaceFeedEl.textContent = ''
  replayState.workspaceRenderedSig = sig
  if (!files.length) {
    const empty = document.createElement('div')
    empty.className = 'agent-log-empty'
    empty.textContent = 'Workspace is empty at this replay point.'
    workspaceFeedEl.appendChild(empty)
    return
  }
  for (const file of files) {
    const row = document.createElement('div')
    row.className = 'workspace-row'

    const op = document.createElement('span')
    op.className = 'workspace-row__op'
    op.textContent = file.omitted ? '!' : '+'

    const path = document.createElement('span')
    path.className = 'workspace-row__path'
    path.title = file.path
    path.textContent = file.path

    const meta = document.createElement('span')
    meta.className = 'workspace-row__meta'
    if (file.omitted) {
      meta.textContent = 'omitted'
    } else if (Number.isFinite(Number(file.size))) {
      meta.textContent = formatBytes(Number(file.size))
    } else {
      meta.textContent = ''
    }

    row.append(op, path, meta)
    workspaceFeedEl.appendChild(row)
  }
}

function formatBytes(value) {
  const bytes = Number(value)
  if (!Number.isFinite(bytes) || bytes < 0) return ''
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

function buildAgentLogBlock(block, key) {
  const type = String(block?.type || '')
  if (type === 'text') {
    const el = document.createElement('div')
    el.className = 'agent-log-block agent-log-block--text'
    el.textContent = String(block?.text || '')
    return el
  }
  if (type === 'thinking') {
    const el = document.createElement('div')
    el.className = 'agent-log-block agent-log-block--thinking'
    el.textContent = String(block?.text || '')
    return el
  }
  if (type === 'tool_use') {
    return buildToolUseBlock(block, key)
  }
  return null
}

function buildToolUseBlock(block, key) {
  const name = String(block?.name || 'tool')
  const input = block?.input
  const result = block?.result
  const isError = !!result?.is_error

  const wrap = document.createElement('div')
  wrap.className = 'agent-log-block agent-log-block--tool'
  if (isError) wrap.classList.add('is-error')

  const expanded = replayState.agentExpandedBlocks.has(key)

  const head = document.createElement('button')
  head.type = 'button'
  head.className = 'agent-log-tool__head'
  head.setAttribute('aria-expanded', expanded ? 'true' : 'false')
  head.appendChild(icon(isError ? CircleAlert : Wrench, 11))
  const labelEl = document.createElement('span')
  labelEl.className = 'agent-log-tool__name'
  labelEl.textContent = name
  head.appendChild(labelEl)
  wrap.appendChild(head)

  const detail = document.createElement('div')
  detail.className = 'agent-log-tool__detail'
  detail.hidden = !expanded

  const hasInput = input && (typeof input === 'object' || typeof input === 'string')
  if (hasInput) {
    const inputEl = document.createElement('pre')
    inputEl.className = 'agent-log-tool__body'
    inputEl.textContent = typeof input === 'string'
      ? input
      : JSON.stringify(input, null, 2)
    detail.appendChild(inputEl)
  }

  if (result) {
    const resultEl = document.createElement('pre')
    resultEl.className = 'agent-log-tool__body agent-log-tool__body--result'
    if (isError) resultEl.classList.add('is-error')
    resultEl.textContent = String(result.content || '')
    detail.appendChild(resultEl)
  }

  if (detail.childElementCount > 0) {
    wrap.appendChild(detail)
    head.addEventListener('click', (e) => {
      e.preventDefault()
      const isOpen = replayState.agentExpandedBlocks.has(key)
      if (isOpen) {
        replayState.agentExpandedBlocks.delete(key)
      } else {
        replayState.agentExpandedBlocks.add(key)
      }
      detail.hidden = isOpen
      head.setAttribute('aria-expanded', isOpen ? 'false' : 'true')
    })
  } else {
    head.setAttribute('aria-disabled', 'true')
  }

  return wrap
}

function behaviorAccentColor(behavior) {
  const explicit = String(behavior?.color || '').trim()
  if (explicit) return explicit
  const label = String(behavior?.label || behavior?.id || 'behavior')
  let hash = 0
  for (let i = 0; i < label.length; i++) hash = (hash * 33 + label.charCodeAt(i)) >>> 0
  const hue = hash % 360
  return `hsl(${hue}deg 62% 58%)`
}

function getReplayBehaviors() {
  const behaviors = Array.isArray(replayState.metadata?.behaviors) ? replayState.metadata.behaviors : []
  return behaviors.filter((behavior) => behavior && typeof behavior === 'object' && Array.isArray(behavior.occurrences))
}

function findBehaviorById(behaviorId) {
  return getReplayBehaviors().find((behavior) => String(behavior.id || '') === String(behaviorId || '')) || null
}

function findOccurrence(behaviorId, occurrenceId) {
  const behavior = findBehaviorById(behaviorId)
  if (!behavior) return { behavior: null, occurrence: null }
  const occurrence = Array.isArray(behavior.occurrences)
    ? behavior.occurrences.find((item) => String(item?.id || '') === String(occurrenceId || '')) || null
    : null
  return { behavior, occurrence }
}

function occurrenceDurationText(occurrence, info) {
  if (!occurrence || !info) return ''
  const startTick = Number(occurrence.start_tick)
  const endTick = Number(occurrence.end_tick)
  if (!Number.isFinite(startTick) || !Number.isFinite(endTick) || !Number.isFinite(info.tick_rate) || info.tick_rate <= 0) {
    return ''
  }
  const durationTicks = Math.max(0, endTick - startTick)
  return formatDurationMs((durationTicks / info.tick_rate) * 1000)
}

function selectBehaviorOccurrence(behaviorId, occurrenceId) {
  replayState.selectedBehaviorId = String(behaviorId || '')
  replayState.selectedOccurrenceId = occurrenceId == null ? null : String(occurrenceId)
}

function minBehaviorViewSpan(info) {
  const tickRate = Number(info?.tick_rate || 0)
  return Math.max(60, tickRate > 0 ? Math.round(tickRate * 2) : 120)
}

function resetBehaviorView(info) {
  if (!info) return
  replayState.behaviorViewStartTick = Number(info.first_tick || 0)
  replayState.behaviorViewEndTick = Number(info.total_ticks || info.first_tick || 0)
}

function getBehaviorView(info) {
  if (!info) {
    return { startTick: 0, endTick: 1, span: 1 }
  }
  const fullStart = Number(info.first_tick || 0)
  const fullEnd = Number(info.total_ticks || fullStart || 1)
  if (
    !Number.isFinite(replayState.behaviorViewStartTick) ||
    !Number.isFinite(replayState.behaviorViewEndTick) ||
    replayState.behaviorViewEndTick <= replayState.behaviorViewStartTick
  ) {
    resetBehaviorView(info)
  }
  let startTick = Number(replayState.behaviorViewStartTick)
  let endTick = Number(replayState.behaviorViewEndTick)
  const minSpan = minBehaviorViewSpan(info)
  const fullSpan = Math.max(1, fullEnd - fullStart)
  let span = Math.max(minSpan, endTick - startTick)
  span = Math.min(span, fullSpan)
  startTick = Math.max(fullStart, Math.min(startTick, fullEnd - span))
  endTick = startTick + span
  replayState.behaviorViewStartTick = startTick
  replayState.behaviorViewEndTick = endTick
  return { startTick, endTick, span }
}

function setBehaviorView(startTick, endTick, info) {
  if (!info) return
  const fullStart = Number(info.first_tick || 0)
  const fullEnd = Number(info.total_ticks || fullStart || 1)
  const fullSpan = Math.max(1, fullEnd - fullStart)
  const minSpan = minBehaviorViewSpan(info)
  let nextStart = Number(startTick)
  let nextEnd = Number(endTick)
  if (!Number.isFinite(nextStart) || !Number.isFinite(nextEnd)) {
    resetBehaviorView(info)
    return
  }
  let span = Math.max(minSpan, nextEnd - nextStart)
  span = Math.min(span, fullSpan)
  nextStart = Math.max(fullStart, Math.min(nextStart, fullEnd - span))
  nextEnd = nextStart + span
  replayState.behaviorViewStartTick = nextStart
  replayState.behaviorViewEndTick = nextEnd
}

function formatBehaviorTickLabel(tick, info) {
  if (!info || !Number.isFinite(Number(info.tick_rate)) || Number(info.tick_rate) <= 0) {
    return String(Math.round(Number(tick) || 0))
  }
  const seconds = Math.max(0, Math.floor((Number(tick) - Number(info.first_tick || 0)) / Number(info.tick_rate)))
  const hours = Math.floor(seconds / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  const secs = seconds % 60
  if (hours > 0) return `${hours}:${String(minutes).padStart(2, '0')}:${String(secs).padStart(2, '0')}`
  return `${minutes}:${String(secs).padStart(2, '0')}`
}

function getBehaviorTimelineMetrics() {
  if (!behaviorTimelineShellEl) return null
  const trackEl =
    behaviorTracksEl?.querySelector('.behavior-row__track') ||
    behaviorRulerEl?.querySelector('.behavior-ruler__track')
  if (!(trackEl instanceof HTMLElement)) return null
  const shellRect = behaviorTimelineShellEl.getBoundingClientRect()
  const trackRect = trackEl.getBoundingClientRect()
  if (trackRect.width <= 0) return null
  return {
    offsetLeft: trackRect.left - shellRect.left,
    trackWidth: trackRect.width,
  }
}

function tickFromBehaviorClientX(clientX, info) {
  const metrics = getBehaviorTimelineMetrics()
  if (!metrics || !info) return null
  const rect = behaviorTimelineShellEl.getBoundingClientRect()
  const x = clientX - rect.left - metrics.offsetLeft
  const ratio = Math.max(0, Math.min(1, x / Math.max(1, metrics.trackWidth)))
  const view = getBehaviorView(info)
  return view.startTick + ratio * view.span
}

function renderBehaviorRuler(view, info) {
  if (!behaviorRulerEl) return
  behaviorRulerEl.textContent = ''
  const spacer = document.createElement('div')
  spacer.className = 'behavior-ruler__spacer'
  const track = document.createElement('div')
  track.className = 'behavior-ruler__track'
  const tickCount = 6
  for (let i = 0; i <= tickCount; i++) {
    const ratio = i / tickCount
    const tickValue = Math.round(view.startTick + ratio * view.span)
    const marker = document.createElement('div')
    marker.className = 'behavior-ruler__tick'
    marker.style.left = `${ratio * 100}%`
    const label = document.createElement('div')
    label.className = 'behavior-ruler__label'
    label.style.left = `${ratio * 100}%`
    label.textContent = formatBehaviorTickLabel(tickValue, info)
    track.append(marker, label)
  }
  behaviorRulerEl.append(spacer, track)
}

function renderBehaviorGlobalPlayhead(view, info) {
  if (!behaviorGlobalPlayheadEl || !info) return
  const metrics = getBehaviorTimelineMetrics()
  if (!metrics) {
    behaviorGlobalPlayheadEl.classList.remove('is-visible')
    return
  }
  const currentTick = Number(info.current_tick || info.first_tick || 0)
  if (currentTick < view.startTick || currentTick > view.endTick) {
    behaviorGlobalPlayheadEl.classList.remove('is-visible')
    return
  }
  const ratio = (currentTick - view.startTick) / Math.max(1, view.span)
  behaviorGlobalPlayheadEl.style.left = `${metrics.offsetLeft + ratio * metrics.trackWidth}px`
  behaviorGlobalPlayheadEl.classList.add('is-visible')
}

async function playBehaviorOccurrence(behaviorId, occurrenceId) {
  const { behavior, occurrence } = findOccurrence(behaviorId, occurrenceId)
  if (!behavior || !occurrence || !replayState.enabled) return
  selectBehaviorOccurrence(behavior.id || '', occurrence.id || '')
  if (replayState.info) {
    replayState.info = {
      ...replayState.info,
      current_tick: Number(occurrence.start_tick || 0),
      paused: false,
      pause_at_tick: Number(occurrence.end_tick || occurrence.start_tick || 0),
    }
  }
  renderBehaviorPanel()
  try {
    await postReplay(`/replay/play-range?start_tick=${encodeURIComponent(String(occurrence.start_tick || 0))}&end_tick=${encodeURIComponent(String(occurrence.end_tick || occurrence.start_tick || 0))}`)
    await refreshReplayInfo()
  } catch (err) {
    console.error(err)
    setStatus('Failed to replay behavior occurrence', 'error')
  }
}

function renderBehaviorDetails() {
  if (!behaviorDetailsEl) return
  const { behavior, occurrence } = findOccurrence(replayState.selectedBehaviorId, replayState.selectedOccurrenceId)
  if (!behavior || !occurrence) {
    behaviorDetailsEl.textContent = ''
    const empty = document.createElement('div')
    empty.className = 'behavior-details__meta'
    empty.textContent = 'Select a behavior occurrence to inspect its details.'
    behaviorDetailsEl.appendChild(empty)
    return
  }

  behaviorDetailsEl.textContent = ''

  const title = document.createElement('div')
  title.className = 'behavior-details__title'
  title.textContent = String(behavior.label || behavior.id || 'Behavior')

  const meta = document.createElement('div')
  meta.className = 'behavior-details__meta'
  const duration = occurrenceDurationText(occurrence, replayState.info)
  const metaParts = [
    `ticks ${occurrence.start_tick}..${occurrence.end_tick}`,
    duration ? `duration ${duration}` : '',
  ].filter(Boolean)
  meta.textContent = metaParts.join(' | ')

  behaviorDetailsEl.append(title, meta)

  if (behavior.description) {
    const description = document.createElement('div')
    description.className = 'behavior-details__description'
    description.textContent = String(behavior.description)
    behaviorDetailsEl.appendChild(description)
  }

  const participants = Array.isArray(occurrence.participants) ? occurrence.participants : []
  if (participants.length) {
    const participantsWrap = document.createElement('div')
    participantsWrap.className = 'behavior-details__participants'
    for (const participant of participants) {
      const chip = document.createElement('div')
      chip.className = 'behavior-details__participant'
      const parts = [
        participant?.role ? String(participant.role) : '',
        participant?.label ? String(participant.label) : '',
        !participant?.label && participant?.id ? String(participant.id) : '',
        participant?.kind ? `(${String(participant.kind)})` : '',
      ].filter(Boolean)
      chip.textContent = parts.join(' ')
      participantsWrap.appendChild(chip)
    }
    behaviorDetailsEl.appendChild(participantsWrap)
  }

  const observations = Array.isArray(occurrence.observations) ? occurrence.observations.slice(0, 3) : []
  if (observations.length) {
    const list = document.createElement('div')
    list.className = 'behavior-details__observations'
    for (const observation of observations) {
      const item = document.createElement('div')
      item.className = 'behavior-details__observation'

      const obsMeta = document.createElement('div')
      obsMeta.className = 'behavior-details__observation-meta'
      const obsParts = [
        observation?.time ? String(observation.time) : '',
        Number.isFinite(Number(observation?.tick)) ? `tick ${Number(observation.tick)}` : '',
        observation?.channel ? String(observation.channel) : '',
        observation?.speaker ? String(observation.speaker) : '',
      ].filter(Boolean)
      obsMeta.textContent = obsParts.join(' | ')

      const body = document.createElement('div')
      body.textContent = String(observation?.text || '')

      item.append(obsMeta, body)
      list.appendChild(item)
    }
    behaviorDetailsEl.appendChild(list)
  } else {
    const emptyObservations = document.createElement('div')
    emptyObservations.className = 'behavior-details__meta'
    emptyObservations.textContent = 'No observations attached to this occurrence.'
    behaviorDetailsEl.appendChild(emptyObservations)
  }
}

function renderBehaviorPanel() {
  if (!behaviorTracksEl) return
  const shouldRender = replayState.enabled && replayState.metadataEnabled && replayState.behaviorPanelOpen
  if (!shouldRender) return

  const behaviors = getReplayBehaviors()
  const info = replayState.info
  const view = getBehaviorView(info)
  const currentTick = Number(info?.current_tick || info?.first_tick || 0)

  if (!replayState.selectedBehaviorId && behaviors.length) {
    replayState.selectedBehaviorId = String(behaviors[0].id || '')
  }
  if (replayState.selectedBehaviorId && !findBehaviorById(replayState.selectedBehaviorId)) {
    replayState.selectedBehaviorId = behaviors.length ? String(behaviors[0].id || '') : null
    replayState.selectedOccurrenceId = null
  }
  if (replayState.selectedBehaviorId && !replayState.selectedOccurrenceId) {
    const selectedBehavior = findBehaviorById(replayState.selectedBehaviorId)
    const firstOccurrence = Array.isArray(selectedBehavior?.occurrences) ? selectedBehavior.occurrences[0] : null
    replayState.selectedOccurrenceId = firstOccurrence ? String(firstOccurrence.id || '') : null
  }

  renderBehaviorRuler(view, info)
  const previousScrollTop = behaviorTracksEl.scrollTop
  behaviorTracksEl.textContent = ''
  for (const behavior of behaviors) {
    const row = document.createElement('div')
    row.className = 'behavior-row'

    const labelButton = document.createElement('button')
    labelButton.type = 'button'
    labelButton.className = 'behavior-row__label'
    const behaviorId = String(behavior.id || '')
    const occurrences = Array.isArray(behavior.occurrences) ? behavior.occurrences : []
    if (behaviorId === replayState.selectedBehaviorId) {
      labelButton.classList.add('is-selected')
    }

    const name = document.createElement('div')
    name.className = 'behavior-row__name'
    name.textContent = String(behavior.label || behavior.id || 'Behavior')

    labelButton.append(name)
    const playFirstOccurrence = (ev) => {
      if (ev) ev.preventDefault()
      selectBehaviorOccurrence(behaviorId, null)
      if (occurrences.length) {
        const firstOccurrenceId = String(occurrences[0].id || '')
        void playBehaviorOccurrence(behaviorId, firstOccurrenceId)
      } else {
        renderBehaviorPanel()
      }
    }
    labelButton.addEventListener('pointerdown', playFirstOccurrence)
    labelButton.addEventListener('click', (ev) => ev.preventDefault())

    const track = document.createElement('div')
    track.className = 'behavior-row__track'
    const viewSpan = Math.max(1, view.span)

    for (const occurrence of occurrences) {
      const startTick = Number(occurrence?.start_tick || 0)
      const endTick = Number(occurrence?.end_tick || startTick)
      if (endTick < view.startTick || startTick > view.endTick) continue
      const isPointEvent = endTick <= startTick
      const startPct = ((startTick - view.startTick) / viewSpan) * 100
      const widthPct = isPointEvent
        ? 0.25
        : Math.max(((Math.max(startTick, endTick) - startTick) / viewSpan) * 100, 0.8)
      const bar = document.createElement('button')
      bar.type = 'button'
      bar.className = 'behavior-occurrence'
      if (isPointEvent) bar.classList.add('behavior-occurrence--point')
      if (String(occurrence?.id || '') === replayState.selectedOccurrenceId) {
        bar.classList.add('is-active')
      } else if (currentTick >= startTick && currentTick <= endTick) {
        bar.classList.add('is-active')
      }
      bar.style.left = `${Math.max(0, Math.min(100, startPct))}%`
      bar.style.width = `${Math.max(0.8, Math.min(100 - Math.max(0, startPct), widthPct))}%`
      bar.style.background = behaviorAccentColor(behavior)
      const duration = occurrenceDurationText(occurrence, info)
      bar.title = `${behavior.label || behavior.id || 'Behavior'} | ticks ${startTick}..${endTick}${duration ? ` | ${duration}` : ''}`
      const playOccurrence = (ev) => {
        ev.stopPropagation()
        ev.preventDefault()
        const occurrenceId = String(occurrence?.id || '')
        void playBehaviorOccurrence(behaviorId, occurrenceId)
      }
      bar.addEventListener('pointerdown', playOccurrence)
      bar.addEventListener('click', (ev) => ev.preventDefault())
      track.appendChild(bar)
    }

    row.append(labelButton, track)
    behaviorTracksEl.appendChild(row)
  }
  behaviorTracksEl.scrollTop = previousScrollTop
  renderBehaviorGlobalPlayhead(view, info)

  renderBehaviorDetails()
}

async function fetchReplayInfo() {
  const resp = await fetch(withBasePath('/replay/info'), { cache: 'no-store' })
  if (!resp.ok) throw new Error(`replay info HTTP ${resp.status}`)
  return resp.json()
}

async function fetchReplayAgentMessages() {
  const resp = await fetch(withBasePath('/replay/agent'), { cache: 'no-store' })
  if (!resp.ok) throw new Error(`replay agent HTTP ${resp.status}`)
  return resp.json()
}

async function fetchReplayWorkspaceEvents() {
  const resp = await fetch(withBasePath('/replay/workspace'), { cache: 'no-store' })
  if (!resp.ok) throw new Error(`replay workspace HTTP ${resp.status}`)
  return resp.json()
}

async function fetchReplayMetadata() {
  const resp = await fetch(withBasePath('/replay/metadata'), { cache: 'no-store' })
  if (!resp.ok) throw new Error(`replay metadata HTTP ${resp.status}`)
  return resp.json()
}

async function fetchExternalReplayView() {
  const resp = await fetch(withBasePath('/replay/external-view'), { cache: 'no-store' })
  if (!resp.ok) throw new Error(`external replay view HTTP ${resp.status}`)
  return resp.json()
}

function mountExternalReplayView(url) {
  replayState.externalViewEnabled = true
  replayState.externalViewUrl = url
  canvas.hidden = true
  let frame = document.getElementById('external-replay-view')
  if (!frame) {
    frame = document.createElement('iframe')
    frame.id = 'external-replay-view'
    frame.className = 'external-replay-view'
    frame.title = 'Replay viewport'
    frame.allow = 'fullscreen; clipboard-read; clipboard-write'
    canvas.insertAdjacentElement('afterend', frame)
  }
  frame.src = url
}

function applyReplayMetadata(payload) {
  replayState.metadataLoaded = true
  replayState.metadataEnabled = !!payload?.enabled && !!payload?.metadata
  replayState.metadata = replayState.metadataEnabled ? payload.metadata : null
  if (!replayState.metadataEnabled) {
    replayState.selectedBehaviorId = null
    replayState.selectedOccurrenceId = null
    replayState.behaviorViewStartTick = null
    replayState.behaviorViewEndTick = null
  }
  syncBehaviorPanelVisibility()
  if (replayState.behaviorPanelOpen) renderBehaviorPanel()
}

function beginBehaviorPan(clientX) {
  if (!replayState.enabled || !replayState.behaviorPanelOpen || !replayState.info) return
  const view = getBehaviorView(replayState.info)
  replayState.behaviorPanState = {
    clientX,
    startTick: view.startTick,
    endTick: view.endTick,
  }
}

function updateBehaviorPan(clientX) {
  if (!replayState.behaviorPanState || !replayState.info) return
  const metrics = getBehaviorTimelineMetrics()
  if (!metrics) return
  const deltaPx = clientX - replayState.behaviorPanState.clientX
  const deltaTicks = (deltaPx / Math.max(1, metrics.trackWidth)) * (replayState.behaviorPanState.endTick - replayState.behaviorPanState.startTick)
  setBehaviorView(
    replayState.behaviorPanState.startTick - deltaTicks,
    replayState.behaviorPanState.endTick - deltaTicks,
    replayState.info,
  )
  renderBehaviorPanel()
}

function endBehaviorPan() {
  replayState.behaviorPanState = null
}

async function probeReplayMode() {
  try {
    return await fetchReplayInfo()
  } catch (err) {
    const message = String(err?.message || err || '')
    if (!message.includes('HTTP 404')) {
      console.error(err)
      setStatus('Replay detection failed', 'warn')
    }
    return null
  }
}

async function postReplay(path) {
  const resp = await fetch(withBasePath(path), { method: 'POST' })
  if (!resp.ok) throw new Error(`${path} HTTP ${resp.status}`)
}

async function refreshReplayInfo() {
  if (!replayState.enabled) return
  try {
    const info = await fetchReplayInfo()
    renderReplayInfo(info)
    if (behaviorPanelAvailable && !replayState.metadataLoaded) {
      const metadata = await fetchReplayMetadata()
      applyReplayMetadata(metadata)
    }
    if (agentPanelAvailable && !replayState.agentLoaded) {
      const logs = await fetchReplayAgentMessages()
      if (logs) renderAgentLogMessages(info?.current_tick || 0, logs)
    } else if (agentPanelAvailable) {
      renderAgentLogMessages(info?.current_tick || 0)
    }
    if (workspacePanelAvailable && !replayState.workspaceLoaded) {
      const workspace = await fetchReplayWorkspaceEvents()
      if (workspace) {
        applyWorkspaceEvents(workspace)
        renderSidePanel(info?.current_tick || 0)
      }
    }
  } catch (err) {
    console.error(err)
    setStatus('Replay status unavailable', 'warn')
  }
}

function startReplayPolling() {
  if (replayState.pollTimer) clearInterval(replayState.pollTimer)
  replayState.pollTimer = setInterval(() => {
    void refreshReplayInfo()
  }, 300)
}

function renderPlayerList(players) {
  if (!playerListBodyEl) return
  const list = Array.isArray(players) ? [...players] : []
  const availableIds = new Set(list.map((p) => String(p?.id || '')).filter(Boolean))
  if (selectedSpectatorPlayerId && !availableIds.has(selectedSpectatorPlayerId)) {
    selectedSpectatorPlayerId = null
  }
  list.sort((a, b) => String(a?.name || '').localeCompare(String(b?.name || '')))
  if (playerListCountEl) playerListCountEl.textContent = String(list.length)
  playerListBodyEl.textContent = ''

  if (!list.length) {
    const empty = document.createElement('div')
    empty.className = 'player-list__empty'
    empty.textContent = 'No players connected'
    playerListBodyEl.appendChild(empty)
    return
  }

  for (const player of list) {
    const row = document.createElement('button')
    row.className = 'player-list__row'
    row.type = 'button'
    row.title = 'Focus player'
    const playerId = String(player?.id || '')
    if (playerId && playerId === selectedSpectatorPlayerId) {
      row.classList.add('is-selected')
      row.setAttribute('aria-pressed', 'true')
    } else {
      row.setAttribute('aria-pressed', 'false')
    }
    const applySelection = () => {
      setSelectedSpectatorPlayer(playerId, { toggle: true })
    }
    row.addEventListener('pointerdown', (ev) => {
      ev.preventDefault()
      applySelection()
    })
    row.addEventListener('click', () => {
      applySelection()
    })

    const name = document.createElement('div')
    name.className = 'player-list__name'
    name.textContent = String(player?.name || 'Player')

    const hp = document.createElement('div')
    hp.className = 'player-list__hp'
    const health = Number(player?.health)
    const healthText = Number.isFinite(health) ? `${Math.max(0, Math.round(health))} HP` : ''
    hp.textContent = playerId && playerId === selectedSpectatorPlayerId
      ? (healthText ? `${healthText} | click again for free camera` : 'Click again for free camera')
      : healthText

    row.append(name, hp)
    playerListBodyEl.appendChild(row)
  }
}

function prioritizeSelectedPlayer(state) {
  if (!state) return state
  if (!selectedSpectatorPlayerId || !Array.isArray(state.players)) {
    if (state.selected_player_id === undefined) return state
    return { ...state, selected_player_id: null }
  }
  const idx = state.players.findIndex((p) => String(p?.id || '') === selectedSpectatorPlayerId)
  if (idx <= 0) {
    if (state.selected_player_id === selectedSpectatorPlayerId) return state
    return { ...state, selected_player_id: selectedSpectatorPlayerId }
  }
  const players = [...state.players]
  const [selected] = players.splice(idx, 1)
  players.unshift(selected)
  return { ...state, players, selected_player_id: selectedSpectatorPlayerId }
}

function rerenderSpectatorState(options = null) {
  if (!latestSpectatorState || !window.__renderer?.onState) return
  const nextState = prioritizeSelectedPlayer(latestSpectatorState)
  if (options && typeof options === 'object' && Object.keys(options).length) {
    window.__renderer.onState({ ...nextState, __mesaCaptureMeta: options })
    return
  }
  window.__renderer.onState(nextState)
}

function renderInjectedSpectatorState(state, info = null, options = null) {
  latestSpectatorState = state || null
  if (info && typeof info === 'object') {
    renderReplayInfo(info)
  }
  renderPlayerList(state?.players)
  rerenderSpectatorState(options)
  return true
}

function replayCameraApi() {
  return internalApiRegistry.get('camera') || null
}

function applyReplayCameraState(state) {
  const api = replayCameraApi()
  if (!api || !state || typeof state !== 'object') {
    pendingReplayCameraState = state || null
    return false
  }
  try {
    if (state.modeOverride && typeof api.setMode === 'function') api.setMode(state.modeOverride)
    else if (typeof api.clearModeOverride === 'function') api.clearModeOverride()
    if (state.spectator && typeof api.setSpectatorPose === 'function') {
      api.setSpectatorPose(state.spectator)
    }
    pendingReplayCameraState = null
    return true
  } catch (err) {
    console.error('[replay camera] restore failed', err)
    return false
  }
}

function applyPendingReplayCameraState() {
  if (!pendingReplayCameraState) return false
  return applyReplayCameraState(pendingReplayCameraState)
}

function reportReplayCameraState(ws) {
  if (!replayState.enabled || !ws || ws.readyState !== WebSocket.OPEN) return
  const now = Date.now()
  if (now - lastCameraReportMs < 250) return
  const api = replayCameraApi()
  if (!api || typeof api.getState !== 'function') return
  try {
    const state = api.getState()
    ws.send(JSON.stringify({ type: 'replay_camera_state', state }))
    lastCameraReportMs = now
  } catch (err) {
    console.error('[replay camera] report failed', err)
  }
}

function afterNextPaint() {
  return new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve))
  })
}

function scheduleVisualFrame(callback, timeoutMs = 34) {
  let fired = false
  let rafId = 0
  let timeoutId = 0
  const run = (source) => {
    if (fired) return
    fired = true
    if (timeoutId) clearTimeout(timeoutId)
    callback(performance.now(), source)
  }
  rafId = requestAnimationFrame(() => run('raf'))
  timeoutId = window.setTimeout(() => {
    if (rafId) cancelAnimationFrame(rafId)
    run('timeout')
  }, timeoutMs)
}

let replayViewportReadySeq = 0
let replayViewportReadyReported = false
let replayViewportReadyPending = false

async function reportReplayViewportReady(state) {
  if (!replayState.enabled || replayViewportReadyReported || replayViewportReadyPending) return
  replayViewportReadyPending = true
  const seq = ++replayViewportReadySeq
  try {
    await afterNextPaint()
    if (seq !== replayViewportReadySeq || !replayState.enabled || replayViewportReadyReported) return
    await threeSdk.whenTrackedAssetsSettled()
    markReplayViewportAssetsSettled()
    await whenReplayViewportWarm()
    await afterNextPaint()
    await threeSdk.whenTrackedAssetsSettled()
    await afterNextPaint()
    if (seq !== replayViewportReadySeq || !replayState.enabled || replayViewportReadyReported) return
    consumeReplayLongTaskMetrics()
    replayViewportReadyReported = true
    window.parent?.postMessage({
      type: 'replay:viewport-ready',
      version: 1,
      href: window.location.href,
      tick: Number.isFinite(Number(state?.tick)) ? Number(state.tick) : null,
      assets: threeSdk.getTrackedAssetLoadState(),
    }, window.location.origin)
  } catch (err) {
    console.error('[replay] viewport readiness failed', err)
  } finally {
    replayViewportReadyPending = false
  }
}

function notifyRendererStateReady(state) {
  if (replayState.enabled) void reportReplayViewportReady(state)
}

function resolveSpectatorPlayerId(target) {
  if (target == null || target === '') return null
  const players = Array.isArray(latestSpectatorState?.players) ? latestSpectatorState.players : []
  const raw = String(target)
  const byId = players.find((player) => String(player?.id || '') === raw)
  if (byId?.id != null) return String(byId.id)
  const byName = players.find((player) => String(player?.name || '') === raw)
  if (byName?.id != null) return String(byName.id)
  return raw
}

function setSelectedSpectatorPlayer(playerId, { toggle = false } = {}) {
  const normalized = resolveSpectatorPlayerId(playerId)
  selectedSpectatorPlayerId = toggle && normalized && normalized === selectedSpectatorPlayerId
    ? null
    : normalized
  if (Array.isArray(latestSpectatorState?.players)) {
    renderPlayerList(latestSpectatorState.players)
  }
  rerenderSpectatorState()
  return selectedSpectatorPlayerId
}

setInternalApi('replay', {
  isEnabled() {
    return !!replayState.enabled
  },
  getInfo() {
    return replayState.info ? { ...replayState.info } : null
  },
  listPlayers() {
    return Array.isArray(latestSpectatorState?.players)
      ? latestSpectatorState.players.map((player) => ({
          id: player?.id == null ? null : String(player.id),
          name: String(player?.name || ''),
          health: Number.isFinite(Number(player?.health)) ? Number(player.health) : null,
        }))
      : []
  },
  getSelectedPlayerId() {
    return selectedSpectatorPlayerId
  },
  selectPlayer(playerId) {
    return setSelectedSpectatorPlayer(playerId)
  },
  clearSelectedPlayer() {
    return setSelectedSpectatorPlayer(null)
  },
  rerender() {
    rerenderSpectatorState()
    return true
  },
  renderState(state, info = null, options = null) {
    return renderInjectedSpectatorState(state, info, options)
  },
})

function setChatLocalStatus(text) {
  if (chatLocalStatusEl) chatLocalStatusEl.textContent = text
}

function updateChatSendUi() {
  if (chatSendEl) chatSendEl.disabled = chatState.sending
  if (chatInputEl) chatInputEl.disabled = false
}

function setChatPanelOpen(open) {
  chatState.open = !!open
  if (!chatPanelEl) return
  chatPanelEl.classList.toggle('is-open', chatState.open)
  if (chatToggleEl) {
    chatToggleEl.setAttribute('aria-expanded', chatState.open ? 'true' : 'false')
  }
  if (chatState.open) renderChatFeed()
}

function makeLocalPlayerName() {
  const key = 'clawblox_local_chat_name'
  const existing = localStorage.getItem(key)
  if (existing && existing.trim()) return existing.trim()
  const generated = `Player-${Math.random().toString(36).slice(2, 6)}`
  localStorage.setItem(key, generated)
  return generated
}

async function ensureChatSession() {
  if (chatState.sessionToken || chatState.spectatorChat) return
  const playerName = chatState.playerName || makeLocalPlayerName()
  chatState.playerName = playerName
  const resp = await fetch(withBasePath(`/join?name=${encodeURIComponent(playerName)}`), { method: 'POST' })
  if (resp.status === 400) {
    // The world refused a new player (usually full because the agent holds the
    // only slot). Chat as a spectator instead, which needs no player slot.
    chatState.spectatorChat = true
    setChatLocalStatus(`Chatting as ${playerName} (spectator)`)
    return
  }
  if (!resp.ok) throw new Error(`join failed: HTTP ${resp.status}`)
  const data = await resp.json()
  chatState.sessionToken = data.session || null
  chatState.agentId = data.agent_id || null
  setChatLocalStatus(`Chatting as ${playerName}`)
}

function userAtBottom(el) {
  return el.scrollHeight - el.scrollTop - el.clientHeight < 16
}

function nameColor(name) {
  let h = 0
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0
  const hue = h % 360
  return `hsl(${hue}deg 70% 75%)`
}

function appendChatMessage(msg) {
  const id = String(msg?.id || '')
  if (!id || chatState.seenIds.has(id)) return
  chatState.seenIds.add(id)
  chatState.messages.push(msg)
  if (chatState.messages.length > 200) {
    const removed = chatState.messages.splice(0, chatState.messages.length - 200)
    for (const item of removed) chatState.seenIds.delete(String(item.id || ''))
  }
  renderChatFeed()
}

function formatChatAudibility(msg) {
  const audibility = Array.isArray(msg?.audibility) ? msg.audibility : []
  if (!audibility.length) return ''
  const heard = audibility.filter((entry) => entry && entry.heard)
  if (!heard.length) return 'heard by nobody'
  const names = heard
    .map((entry) => String(entry.agent_name || '').trim())
    .filter(Boolean)
  if (!names.length) return `heard by ${heard.length}`
  if (names.length <= 4) return `heard by ${names.join(', ')}`
  return `heard by ${names.slice(0, 4).join(', ')} +${names.length - 4}`
}

function renderChatFeed() {
  if (!chatFeedEl) return
  const shouldStick = userAtBottom(chatFeedEl)
  chatFeedEl.textContent = ''
  if (!chatState.messages.length) {
    const empty = document.createElement('div')
    empty.className = 'chat-message chat-message--system'
    empty.textContent = 'No messages yet.'
    chatFeedEl.appendChild(empty)
  } else {
    for (const msg of chatState.messages) {
      const row = document.createElement('div')
      row.className = 'chat-message'

      const meta = document.createElement('span')
      meta.className = 'chat-message__meta'

      const name = document.createElement('span')
      name.className = 'chat-message__name'
      name.textContent = String(msg.agent_name || 'Player')
      name.style.color = nameColor(name.textContent)

      const time = document.createElement('span')
      time.className = 'chat-message__time'
      const dt = new Date(String(msg.created_at || ''))
      time.textContent = Number.isFinite(dt.getTime())
        ? dt.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
        : ''

      const body = document.createElement('span')
      body.textContent = String(msg.content || '')

      meta.append(name, time)
      const heard = formatChatAudibility(msg)
      if (heard) {
        const delivery = document.createElement('div')
        delivery.className = 'chat-message__time'
        delivery.textContent = heard
        row.append(meta, body, delivery)
      } else {
        row.append(meta, body)
      }
      chatFeedEl.appendChild(row)
    }
  }
  if (shouldStick) chatFeedEl.scrollTop = chatFeedEl.scrollHeight
}

async function loadChatHistory() {
  if (replayState.enabled) return  // In replay mode, chat comes via WebSocket
  try {
    const resp = await fetch(withSpectatorToken('/spectate/chat/messages?limit=50'))
    if (!resp.ok) throw new Error(`history HTTP ${resp.status}`)
    const data = await resp.json()
    const messages = Array.isArray(data?.messages) ? data.messages : []
    for (const msg of messages) appendChatMessage(msg)
    chatState.loadedHistory = true
  } catch (err) {
    console.error(err)
    setChatLocalStatus('Failed to load chat history')
  }
}

async function sendChatMessage(text) {
  if (replayState.enabled) return
  const content = String(text || '').trim()
  if (!content) return
  chatState.sending = true
  updateChatSendUi()
  try {
    await ensureChatSession()
    const resp = chatState.spectatorChat
      ? await fetch(withSpectatorToken('/spectate/chat'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: chatState.playerName, content }),
      })
      : await fetch(withBasePath('/chat'), {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Session': chatState.sessionToken,
        },
        body: JSON.stringify({ content }),
      })
    if (!resp.ok) throw new Error(`chat send failed: HTTP ${resp.status}`)
    if (chatInputEl) chatInputEl.value = ''
    setChatLocalStatus(`Chatting as ${chatState.playerName}${chatState.spectatorChat ? ' (spectator)' : ''}`)
  } catch (err) {
    console.error(err)
    setChatLocalStatus(err instanceof Error ? err.message : 'Failed to send chat')
  } finally {
    chatState.sending = false
    updateChatSendUi()
  }
}

if (chatToggleEl) {
  chatToggleEl.addEventListener('click', () => {
    setChatPanelOpen(!chatState.open)
    if (chatState.open && chatInputEl) chatInputEl.focus()
  })
}

if (chatFormEl) {
  chatFormEl.addEventListener('submit', (ev) => {
    ev.preventDefault()
    void sendChatMessage(chatInputEl?.value || '')
  })
}

window.addEventListener('keydown', (ev) => {
  const target = ev.target
  const isTextField = target instanceof HTMLElement && (
    target.tagName === 'INPUT' ||
    target.tagName === 'TEXTAREA' ||
    target.isContentEditable
  )

  if (ev.key === '/' && !isTextField) {
    ev.preventDefault()
    setChatPanelOpen(true)
    chatInputEl?.focus()
    return
  }

  if (ev.key === 'Escape' && chatState.open) {
    if (document.activeElement === chatInputEl) {
      chatInputEl.blur()
    } else {
      setChatPanelOpen(false)
    }
  }
})

updateChatSendUi()
renderChatFeed()
setChatLocalStatus('Read-only until you send a message.')
setChatConnection('Connecting...')

if (!chatState.open) {
  setChatPanelOpen(false)
}

function resizeCanvas() {
  const dpr = Math.min(window.devicePixelRatio || 1, 2)
  const rect = canvas.getBoundingClientRect()
  canvas.width = Math.max(1, Math.floor(rect.width * dpr))
  canvas.height = Math.max(1, Math.floor(rect.height * dpr))
  return dpr
}

function isRendererInstance(value) {
  if (!value || typeof value !== 'object') return false
  return (
    value.mount === undefined || typeof value.mount === 'function'
  ) && (
    value.unmount === undefined || typeof value.unmount === 'function'
  ) && (
    value.onState === undefined || typeof value.onState === 'function'
  ) && (
    value.onFrame === undefined || typeof value.onFrame === 'function'
  ) && (
    value.onResize === undefined || typeof value.onResize === 'function'
  )
}

function lerp(a, b, t) {
  return a + (b - a) * t
}

function lerpVec3(a, b, t) {
  return [
    lerp(a[0], b[0], t),
    lerp(a[1], b[1], t),
    lerp(a[2], b[2], t),
  ]
}

function indexById(items, idField = 'id') {
  const out = new Map()
  if (!Array.isArray(items)) return out
  for (const item of items) {
    if (!item || typeof item !== 'object') continue
    const id = item[idField]
    if (id === undefined || id === null) continue
    out.set(id, item)
  }
  return out
}

function createSnapshotBuffer(options = {}) {
  const maxSnapshots = Number.isFinite(options.maxSnapshots) ? Math.max(2, options.maxSnapshots) : 12
  const interpolationDelayMs = Number.isFinite(options.interpolationDelayMs)
    ? Math.max(0, options.interpolationDelayMs)
    : 100
  const maxTickGap = Number.isFinite(options.maxTickGap) ? Math.max(1, options.maxTickGap) : 120
  const snapshots = []

  function push(state, nowMs = performance.now()) {
    const last = snapshots.length ? snapshots[snapshots.length - 1].state : null
    const lastTick = Number(last?.tick)
    const nextTick = Number(state?.tick)
    if (
      Number.isFinite(lastTick) &&
      Number.isFinite(nextTick) &&
      Math.abs(nextTick - lastTick) > maxTickGap
    ) {
      snapshots.length = 0
    }
    snapshots.push({ state, nowMs })
    while (snapshots.length > maxSnapshots) snapshots.shift()
  }

  function latest() {
    return snapshots.length ? snapshots[snapshots.length - 1].state : null
  }

  function previous() {
    return snapshots.length > 1 ? snapshots[snapshots.length - 2].state : null
  }

  function interpolated(nowMs = performance.now()) {
    if (!snapshots.length) return null
    if (snapshots.length === 1) return snapshots[0].state

    const targetTime = nowMs - interpolationDelayMs
    if (targetTime <= snapshots[0].nowMs) return snapshots[0].state

    let from = null
    let to = null
    for (let i = 0; i < snapshots.length - 1; i++) {
      const a = snapshots[i]
      const b = snapshots[i + 1]
      if (a.nowMs <= targetTime && b.nowMs >= targetTime) {
        from = a
        to = b
        break
      }
    }

    if (!from || !to) {
      return snapshots[snapshots.length - 1].state
    }

    const span = Math.max(1, to.nowMs - from.nowMs)
    const t = Math.max(0, Math.min(1, (targetTime - from.nowMs) / span))

    const fromState = from.state
    const toState = to.state
    const fromPlayers = indexById(fromState?.players)
    const changedEntityIds = Array.isArray(toState?.__spectatorDelta?.changedEntityIds)
      ? new Set(toState.__spectatorDelta.changedEntityIds)
      : null
    const fromEntities = changedEntityIds?.size === 0 ? null : indexById(fromState?.entities)

    const players = Array.isArray(toState?.players)
      ? toState.players.map((p) => {
        const fp = fromPlayers.get(p.id)
        if (!fp || !Array.isArray(fp.position) || !Array.isArray(p.position)) return p
        return { ...p, position: lerpVec3(fp.position, p.position, t) }
      })
      : []

    const entities = Array.isArray(toState?.entities)
      ? changedEntityIds?.size === 0
        ? toState.entities
        : toState.entities.map((e) => {
        if (changedEntityIds && !changedEntityIds.has(e.id)) return e
        const fe = fromEntities.get(e.id)
        if (!fe || !Array.isArray(fe.position) || !Array.isArray(e.position)) return e
        return { ...e, position: lerpVec3(fe.position, e.position, t) }
      })
      : []

    return {
      ...toState,
      players,
      entities,
    }
  }

  function forEachRecent(fn) {
    for (let i = snapshots.length - 1; i >= 0; i--) fn(snapshots[i].state, i)
  }

  function clear() {
    snapshots.length = 0
  }

  return {
    push,
    latest,
    previous,
    interpolated,
    forEachRecent,
    clear,
    size() {
      return snapshots.length
    },
  }
}

function createRecentEventCursor(options = {}) {
  const maxSeen = Number.isFinite(options.maxSeen) ? Math.max(1, options.maxSeen) : 512
  const seen = new Set()
  const order = []

  function fallbackKey(event) {
    let payload = ''
    try {
      payload = JSON.stringify(event?.payload ?? null)
    } catch (_err) {
      payload = String(event?.payload ?? '')
    }
    return [
      'event',
      event?.name ?? '',
      event?.tick ?? '',
      event?.server_time_ms ?? '',
      payload,
    ].join(':')
  }

  function eventKey(event) {
    if (!event || typeof event !== 'object') return null
    if (event.id !== undefined && event.id !== null) return `id:${event.id}`
    return fallbackKey(event)
  }

  function remember(key) {
    seen.add(key)
    order.push(key)
    while (order.length > maxSeen) {
      const old = order.shift()
      seen.delete(old)
    }
  }

  function eventsFrom(input) {
    if (Array.isArray(input)) return input
    return Array.isArray(input?.recent_events) ? input.recent_events : []
  }

  function drain(input) {
    const out = []
    for (const event of eventsFrom(input)) {
      const key = eventKey(event)
      if (!key || seen.has(key)) continue
      remember(key)
      out.push(event)
    }
    return out
  }

  function clear() {
    seen.clear()
    order.length = 0
  }

  return {
    drain,
    clear,
    size() {
      return order.length
    },
  }
}

function createAnimationInspector() {
  function tracksForPlayer(player) {
    return Array.isArray(player?.active_animations) ? player.active_animations : []
  }

  function findTrack(player, predicate) {
    const tracks = tracksForPlayer(player)
    for (const track of tracks) {
      if (!track || track.is_playing === false) continue
      const id = String(track.animation_id || '')
      if (predicate(id, track)) return track
    }
    return null
  }

  function hasTrackMatching(player, pattern) {
    const regex = pattern instanceof RegExp ? pattern : new RegExp(String(pattern), 'i')
    return !!findTrack(player, (id) => regex.test(id))
  }

  function mapPlayersByRootPart(players) {
    const out = new Map()
    if (!Array.isArray(players)) return out
    for (const player of players) {
      if (!player || typeof player.root_part_id !== 'number') continue
      out.set(player.root_part_id, player)
    }
    return out
  }

  return {
    tracksForPlayer,
    findTrack,
    hasTrackMatching,
    mapPlayersByRootPart,
  }
}

function createPresetRegistry(initialPresets = {}) {
  const presets = new Map(Object.entries(initialPresets))

  function register(id, preset) {
    if (!id) return
    presets.set(id, preset || {})
  }

  function resolve(entity) {
    const presetId = entity?.render?.preset_id || entity?.preset_id
    const base = presetId ? presets.get(String(presetId)) || null : null
    if (!base) return null
    return {
      id: String(presetId),
      ...base,
    }
  }

  function color(entity, fallback = '#4f46e5') {
    const resolved = resolve(entity)
    if (resolved?.color) return resolved.color
    const c = entity?.render?.color
    if (Array.isArray(c) && c.length === 3) {
      const r = Math.round(Math.max(0, Math.min(1, c[0])) * 255)
      const g = Math.round(Math.max(0, Math.min(1, c[1])) * 255)
      const b = Math.round(Math.max(0, Math.min(1, c[2])) * 255)
      return `rgb(${r}, ${g}, ${b})`
    }
    return fallback
  }

  return {
    register,
    resolve,
    color,
    list() {
      return [...presets.entries()]
    },
  }
}

function createRuntimeKit() {
  const ownedInternalApis = new Set()
  return {
    math: { lerp, lerpVec3 },
    state: {
      indexById,
      createSnapshotBuffer,
      createRecentEventCursor,
    },
    animation: createAnimationInspector(),
    presets: {
      createPresetRegistry,
    },
    three: threeSdk,
    input: inputSdk,
    internal: {
      expose(name, api) {
        const key = normalizeInternalApiName(name)
        if (!key) throw new Error('internal api name required')
        ownedInternalApis.add(key)
        return setInternalApi(key, api)
      },
      remove(name) {
        const key = normalizeInternalApiName(name)
        if (!key) return false
        ownedInternalApis.delete(key)
        return removeInternalApi(key)
      },
      get(name) {
        const key = normalizeInternalApiName(name)
        return key ? (internalApiRegistry.get(key) || null) : null
      },
      keys() {
        return [...ownedInternalApis]
      },
      dispose() {
        for (const key of ownedInternalApis) removeInternalApi(key)
        ownedInternalApis.clear()
      },
    },
  }
}

async function loadManifest() {
  const resp = await fetch(withBasePath('/renderer/manifest'))
  if (!resp.ok) throw new Error(`manifest HTTP ${resp.status}`)
  return resp.json()
}

async function loadRendererModule(manifest) {
  const target = manifest?.entry_url || withBasePath('/ui/renderers/default.js')
  const mod = await import(/* @vite-ignore */ target)
  const createRenderer = mod?.createRenderer || mod?.default?.createRenderer
  if (typeof createRenderer !== 'function') {
    throw new Error(`renderer module missing createRenderer(): ${target}`)
  }
  return { createRenderer, target }
}

let audioCtx = null
const activeStreams = new Map()
const pendingChunks = []
let spectateWs = null
const AUDIO_FINALIZE_GRACE_MS = 50

function ensureAudioCtx() {
  if (!audioCtx) {
    audioCtx = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: 24000 })
    console.log('[audio] AudioContext created, state=' + audioCtx.state)
  }
  if (audioCtx.state === 'suspended') {
    audioCtx.resume().then(() => {
      console.log('[audio] AudioContext resumed, flushing ' + pendingChunks.length + ' buffered chunks')
      while (pendingChunks.length) playChunk(pendingChunks.shift())
    })
  } else if (audioCtx.state === 'running') {
    while (pendingChunks.length) playChunk(pendingChunks.shift())
  }
}

function onUserGesture() {
  ensureAudioCtx()
  // Only stop listening once context is actually running
  if (audioCtx && audioCtx.state === 'running') {
    document.removeEventListener('click', onUserGesture)
    document.removeEventListener('keydown', onUserGesture)
  }
}
document.addEventListener('click', onUserGesture)
document.addEventListener('keydown', onUserGesture)

function onAudioChunk(msg) {
  if (!audioCtx || audioCtx.state !== 'running') {
    // Buffer until user gesture resumes the context
    pendingChunks.push(msg)
    return
  }
  playChunk(msg)
}

let audioDebugCount = 0

function playChunk(msg) {
  let stream = activeStreams.get(msg.stream_id)
  if (!stream) {
    stream = {
      nextPlayTime: 0,
      leftover: null,
      firstChunkWall: performance.now(),
      chunkCount: 0,
      done: false,
      finalized: false,
      finalizeTimer: null,
      lastSource: null,
    }
    activeStreams.set(msg.stream_id, stream)
    console.log(`[audio] new stream ${msg.stream_id}`)
  }
  stream.chunkCount++

  if (msg.done) {
    const streamId = msg.stream_id
    stream.done = true

    const finalizeNow = (reason) => {
      if (stream.finalized) return
      stream.finalized = true
      if (stream.finalizeTimer) {
        clearTimeout(stream.finalizeTimer)
        stream.finalizeTimer = null
      }
      console.log(`[audio] finalized stream ${streamId} reason=${reason}`)
      activeStreams.delete(streamId)
    }

    const armFinalizeGuard = () => {
      const now = audioCtx.currentTime
      const buffered = stream.nextPlayTime > now ? stream.nextPlayTime - now : 0
      const delayMs = buffered * 1000 + AUDIO_FINALIZE_GRACE_MS
      const wallElapsed = ((performance.now() - stream.firstChunkWall) / 1000).toFixed(2)
      console.log(`[audio] stream ${streamId} done: ${stream.chunkCount} chunks, ` +
        `buffered=${buffered.toFixed(2)}s ahead, wall=${wallElapsed}s, guard in ${delayMs.toFixed(0)}ms`)
      stream.finalizeTimer = setTimeout(() => finalizeNow("guard_timeout"), delayMs)
    }

    if (audioCtx.state !== 'running') {
      console.log(`[audio] stream ${streamId} done but AudioContext suspended, waiting for resume`)
      const onResume = () => {
        if (audioCtx.state === 'running') {
          audioCtx.removeEventListener('statechange', onResume)
          armFinalizeGuard()
        }
      }
      audioCtx.addEventListener('statechange', onResume)
      return
    }

    if (!stream.lastSource) {
      // No scheduled source exists; finalize immediately.
      finalizeNow("done_no_source")
      return
    }

    armFinalizeGuard()
    return
  }

  // Decode base64 to bytes
  const raw = atob(msg.data)
  let bytes
  if (stream.leftover !== null) {
    bytes = new Uint8Array(1 + raw.length)
    bytes[0] = stream.leftover
    for (let i = 0; i < raw.length; i++) bytes[i + 1] = raw.charCodeAt(i)
    stream.leftover = null
  } else {
    bytes = new Uint8Array(raw.length)
    for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i)
  }

  // Carry over odd trailing byte for next chunk
  let usable = bytes.length
  if (usable % 2 !== 0) {
    stream.leftover = bytes[usable - 1]
    usable -= 1
  }
  if (usable === 0) return

  // Int16 -> Float32
  const aligned = bytes.buffer.byteLength === usable ? bytes.buffer : bytes.slice(0, usable).buffer
  const int16 = new Int16Array(aligned)
  const float32 = new Float32Array(int16.length)
  for (let i = 0; i < int16.length; i++) float32[i] = int16[i] / 32768

  // Schedule gapless playback
  const buffer = audioCtx.createBuffer(1, float32.length, 24000)
  buffer.getChannelData(0).set(float32)
  const source = audioCtx.createBufferSource()
  source.buffer = buffer
  source.connect(audioCtx.destination)
  stream.lastSource = source
  source.onended = () => {
    // Authoritative completion path: stream is done and this is the last scheduled source.
    if (!stream.done) return
    if (stream.finalized) return
    if (stream.lastSource !== source) return
    if (stream.finalizeTimer) {
      clearTimeout(stream.finalizeTimer)
      stream.finalizeTimer = null
    }
    console.log(`[audio] finalized stream ${msg.stream_id} reason=last_source_onended`)
    stream.finalized = true
    activeStreams.delete(msg.stream_id)
  }

  const now = audioCtx.currentTime
  if (stream.nextPlayTime < now) stream.nextPlayTime = now
  source.start(stream.nextPlayTime)
  stream.nextPlayTime += buffer.duration

  // Log every 10th chunk: how far ahead the buffer is scheduled
  if (stream.chunkCount % 10 === 1) {
    const ahead = (stream.nextPlayTime - now).toFixed(2)
    const wallMs = (performance.now() - stream.firstChunkWall).toFixed(0)
    console.log(`[audio] chunk #${stream.chunkCount} stream=${msg.stream_id.slice(-8)} dur=${buffer.duration.toFixed(3)}s ahead=${ahead}s wall=${wallMs}ms`)
  }
}

async function bootstrap() {
  let currentDpr = resizeCanvas()
  window.addEventListener('resize', () => {
    currentDpr = resizeCanvas()
    if (window.__renderer?.onResize) {
      window.__renderer.onResize({ width: canvas.width, height: canvas.height, dpr: currentDpr })
    }
  })

  setStatus('Loading renderer...', 'muted')

  const replayInfo = await probeReplayMode()
  if (replayInfo) {
    setReplayMode(true)
    renderReplayInfo(replayInfo)
    if (!RUNTIME_VIEWPORT_ONLY && behaviorPanelAvailable) {
      const metadata = await fetchReplayMetadata().catch((err) => {
        console.error(err)
        return null
      })
      if (metadata) applyReplayMetadata(metadata)
    }
    if (!RUNTIME_VIEWPORT_ONLY && agentPanelAvailable) {
      const logs = await fetchReplayAgentMessages().catch((err) => {
        console.error(err)
        return null
      })
      if (logs) renderAgentLogMessages(replayInfo?.current_tick || 0, logs)
    }
    if (!RUNTIME_VIEWPORT_ONLY && workspacePanelAvailable) {
      const workspace = await fetchReplayWorkspaceEvents().catch((err) => {
        console.error(err)
        return null
      })
      if (workspace) {
        applyWorkspaceEvents(workspace)
        renderSidePanel(replayInfo?.current_tick || 0)
      }
    }
    if (!RUNTIME_VIEWPORT_ONLY) startReplayPolling()
    if (replayChromeAvailable) {
      const externalView = await fetchExternalReplayView().catch((err) => {
        console.error(err)
        return null
      })
      if (externalView?.enabled && externalView?.url) {
        mountExternalReplayView(externalView.url)
        setStatus('Connected | Replay | External viewport', 'ok')
        window.addEventListener('beforeunload', () => {
          if (replayState.pollTimer) clearInterval(replayState.pollTimer)
        })
        return
      }
    }
  }

  let manifest
  try {
    manifest = await loadManifest()
  } catch (error) {
    console.error(error)
    setStatus('Failed to load renderer manifest', 'error')
    return
  }

  const name = manifest?.name || 'Default Renderer'
  const mode = manifest?.mode || 'default'
  const capabilities = Array.isArray(manifest?.capabilities) ? manifest.capabilities.join(', ') : ''
  metaEl.textContent = `${name} | mode: ${mode}${capabilities ? ` | ${capabilities}` : ''}`

  if (manifest?.api_version !== API_VERSION) {
    setStatus(
      `Renderer API mismatch (host=${API_VERSION}, game=${manifest?.api_version ?? 'unknown'})`,
      'error'
    )
    return
  }

  let rendererFactory
  let rendererTarget
  try {
    const loaded = await loadRendererModule(manifest)
    rendererFactory = loaded.createRenderer
    rendererTarget = loaded.target
  } catch (error) {
    console.error(error)
    const configured = manifest?.entry_url
    if (configured) {
      setStatus(`Renderer load failed: ${configured}`, 'error')
      return
    }
    setStatus('No custom renderer configured; using default', 'warn')
    const fallback = await import('./renderers/default.js')
    rendererFactory = fallback.createRenderer
    rendererTarget = withBasePath('/ui/renderers/default.js')
  }

  const runtimeKit = createRuntimeKit()
  const hostContext = {
    apiVersion: API_VERSION,
    manifest,
    canvas,
    runtime: runtimeKit,
    exposeInternal(name, api) {
      return runtimeKit.internal.expose(name, api)
    },
    log(level, message, data) {
      const fn = level === 'error' ? console.error : level === 'warn' ? console.warn : console.log
      fn(`[renderer] ${message}`, data ?? '')
    },
  }

  const renderer = rendererFactory(hostContext)
  if (!isRendererInstance(renderer)) {
    setStatus('Invalid renderer instance contract', 'error')
    return
  }
  window.__renderer = renderer
  if (renderer.mount) renderer.mount()
  if (renderer.onResize) renderer.onResize({ width: canvas.width, height: canvas.height, dpr: currentDpr })

  const wsStats = { frames: 0, lastTick: 0 }
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const ws = new WebSocket(`${protocol}//${window.location.host}${withSpectatorToken('/spectate/ws')}`)
  ws.binaryType = 'arraybuffer'
  spectateWs = ws
  let pendingRendererState = null
  let pendingRendererTrace = null
  let pendingRendererReplayMeta = null
  let rendererFrameScheduled = false
  let rendererFrameLoopActive = false
  let rendererFrameLoopRaf = 0
  let rendererFrameLoopLastMs = performance.now()
  let hasRenderedReplayState = false
  let lastReplayMessageMs = null
  let lastReplayMessageTick = null
  let lastReplayRequestedTick = null
  let replayWindowRequestOutstanding = false
  let viewportReplayControlTimer = null

  function reportReplayViewportError(message, data = {}) {
    if (!replayState.enabled || hasRenderedReplayState) return
    try {
      window.parent?.postMessage({
        type: 'replay:viewport-error',
        version: 1,
        href: window.location.href,
        message,
        ...data,
      }, window.location.origin)
    } catch (err) {
      console.error('[replay] viewport error report failed', err)
    }
  }

  function startRendererFrameLoop() {
    if (typeof renderer.onFrame !== 'function' || rendererFrameLoopActive) return
    rendererFrameLoopActive = true
    rendererFrameLoopLastMs = performance.now()
    const step = (nowMs) => {
      if (!rendererFrameLoopActive) return
      rendererFrameLoopRaf = requestAnimationFrame(step)
      const dt = Math.max(0, Math.min((nowMs - rendererFrameLoopLastMs) / 1000, 0.05))
      rendererFrameLoopLastMs = nowMs
      if (rendererFrameScheduled || pendingRendererState) return
      renderer.onFrame({ dt, nowMs, replay: replayState.enabled })
      captureHook.frame += 1
      applyPendingReplayCameraState()
      if (replayState.enabled && ws.readyState === WebSocket.OPEN) reportReplayCameraState(ws)
    }
    rendererFrameLoopRaf = requestAnimationFrame(step)
  }

  function stopRendererFrameLoop() {
    rendererFrameLoopActive = false
    if (rendererFrameLoopRaf) cancelAnimationFrame(rendererFrameLoopRaf)
    rendererFrameLoopRaf = 0
  }

  function replayRequestStepTicks() {
    const info = replayState.info || {}
    const tickRate = Math.max(0, Number(info.tick_rate || 60))
    const speed = Math.max(0, Number(info.speed ?? 1))
    return Math.max(1, Math.ceil((tickRate * Math.max(1, speed)) / 60))
  }

  function requestReplayFrame(reason = 'buffer') {
    if (!replayState.enabled || ws.readyState !== WebSocket.OPEN) return
    const stepTicks = replayRequestStepTicks()
    const step = lastReplayRequestedTick == null
      ? stepReplayPlaybackClock()
      : {
          tick: lastReplayRequestedTick + stepTicks,
          elapsedMs: 0,
          rawAdvanceTicks: stepTicks,
          appliedAdvanceTicks: stepTicks,
          droppedTicks: 0,
          paused: !!replayState.info?.paused,
        }
    if (step.paused && hasRenderedReplayState && replayViewportWarmup.ready) return
    const previousTick = lastReplayRequestedTick
    const windowSize = Math.max(1, replayFrameQueueTarget - replayFrameQueue.length)
    lastReplayRequestedTick = step.tick + stepTicks * (windowSize - 1)
    replayWindowRequestOutstanding = true
    ws.send(JSON.stringify({
      type: 'replay_request_frame',
      tick: step.tick,
      previous_tick: previousTick,
      seek_version: Number.isFinite(Number(replayState.info?.seek_version)) ? Number(replayState.info.seek_version) : null,
      reason,
      window_size: windowSize,
      step_ticks: stepTicks,
      speed: Number(replayState.info?.speed ?? 1),
      paused: !!replayState.info?.paused,
      elapsed_ms: step.elapsedMs,
      raw_advance_ticks: step.rawAdvanceTicks,
      applied_advance_ticks: step.appliedAdvanceTicks,
      dropped_ticks: step.droppedTicks,
    }))
  }

  function clearReplayFrameQueue() {
    replayFrameQueue.length = 0
    lastReplayRequestedTick = null
    replayWindowRequestOutstanding = false
    replayViewportWarmup.ready = false
    replayViewportWarmup.warmupFrames = 0
    replayViewportWarmup.postRestoreWarmupFrames = 0
    replayViewportWarmup.maxWarmupRenderMs = 0
    replayViewportWarmup.assetsSettled = false
    replayViewportWarmup.restored = false
    replayViewportWarmup.postRestoreWarmupComplete = false
    replayViewportWarmup.restoredFrameConsumed = false
    replayViewportWarmup.renderedFrame = false
  }

  function maybeRequestReplayFrames(reason = 'buffer-low') {
    if (!replayState.enabled || ws.readyState !== WebSocket.OPEN) return
    if (replayWindowRequestOutstanding) return
    if (replayFrameQueue.length > replayFrameQueueLowWater) return
    requestReplayFrame(reason)
  }

  function enqueueReplayRendererState(state, trace = null) {
    replayFrameQueue.push({ state, trace })
    notifyReplayViewportWarmup()
    if (replayFrameQueue.length > replayFrameQueueLowWater) replayWindowRequestOutstanding = false
    maybeRequestReplayFrames('buffer-fill')
    pumpReplayFrameQueue()
  }

  function pumpReplayFrameQueue() {
    if (!replayState.enabled || rendererFrameScheduled || replayFrameQueue.length === 0) return
    if (replayState.info?.paused) {
      if (!hasRenderedReplayState) {
        scheduleRendererState(replayFrameQueue[0].state, replayFrameQueue[0].trace, { replayWarmup: 'initial' })
        return
      }
      if (!replayViewportWarmup.assetsSettled || replayViewportWarmup.ready) return
      if (replayViewportWarmup.warmupFrames < replayViewportWarmup.minWarmupFrames) {
        const warmupIndex = Math.min(replayViewportWarmup.warmupFrames, replayFrameQueue.length - 1)
        const warmup = replayFrameQueue[warmupIndex]
        scheduleRendererState(warmup.state, warmup.trace, { replayWarmup: 'prewarm' })
        return
      }
      if (!replayViewportWarmup.restored) {
        scheduleRendererState(replayFrameQueue[0].state, replayFrameQueue[0].trace, { replayWarmup: 'restore' })
        return
      }
      if (replayViewportWarmup.postRestoreWarmupFrames < replayViewportWarmup.minPostRestoreWarmupFrames) {
        const warmupIndex = Math.min(replayViewportWarmup.postRestoreWarmupFrames + 1, replayFrameQueue.length - 1)
        const warmup = replayFrameQueue[warmupIndex]
        scheduleRendererState(warmup.state, warmup.trace, { replayWarmup: 'post-restore-prewarm' })
        return
      }
      if (!replayViewportWarmup.postRestoreWarmupComplete) {
        scheduleRendererState(replayFrameQueue[0].state, replayFrameQueue[0].trace, { replayWarmup: 'final-restore' })
      }
      return
    }
    if (
      replayViewportWarmup.ready &&
      replayViewportWarmup.restored &&
      !replayViewportWarmup.restoredFrameConsumed &&
      replayFrameQueue.length > 1
    ) {
      replayFrameQueue.shift()
      replayViewportWarmup.restoredFrameConsumed = true
    }
    const next = replayFrameQueue.shift()
    scheduleRendererState(next.state, next.trace)
  }

  replayViewportWarmupPump = pumpReplayFrameQueue
  startRendererFrameLoop()

  async function syncViewportReplayControls() {
    if (!RUNTIME_VIEWPORT_ONLY || !replayState.enabled) return
    try {
      const previous = replayState.info || {}
      const info = await fetchReplayInfo()
      const previousSeekVersion = Number.isFinite(Number(previous.seek_version)) ? Number(previous.seek_version) : null
      const seekVersion = Number.isFinite(Number(info.seek_version)) ? Number(info.seek_version) : null
      const wasPaused = !!previous.paused
      const paused = !!info.paused
      if (seekVersion !== previousSeekVersion) clearReplayFrameQueue()
      renderReplayInfo(info)
      if (!paused) {
        pumpReplayFrameQueue()
        maybeRequestReplayFrames('control-sync')
      }
    } catch {
      // The viewport remains render-only; transient control polling failures are non-fatal.
    }
  }

  function startViewportReplayControlSync() {
    if (!RUNTIME_VIEWPORT_ONLY || viewportReplayControlTimer) return
    viewportReplayControlTimer = setInterval(() => {
      void syncViewportReplayControls()
    }, 250)
  }

  function scheduleRendererState(state, trace = null, replayMeta = null) {
    pendingRendererState = state
    pendingRendererTrace = trace
    pendingRendererReplayMeta = replayMeta
    if (rendererFrameScheduled) return
    rendererFrameScheduled = true
    const scheduledAtMs = performance.now()
    scheduleVisualFrame((frameNowMs, frameSource) => {
      rendererFrameScheduled = false
      const state = pendingRendererState
      const trace = pendingRendererTrace
      const replayMeta = pendingRendererReplayMeta
      pendingRendererState = null
      pendingRendererTrace = null
      pendingRendererReplayMeta = null
      if (!state) return
      const rendererState = prioritizeSelectedPlayer(state)
      const renderStartMs = performance.now()
      if (renderer.onState) renderer.onState(rendererState)
      captureHook.frame += 1
      if (Number.isFinite(rendererState?.tick)) captureHook.tick = rendererState.tick
      const renderEndMs = performance.now()
      const rendererProfile = typeof renderer.getProfile === 'function' ? renderer.getProfile() : null
      if (replayState.enabled) {
        hasRenderedReplayState = true
        replayViewportWarmup.renderedFrame = true
      }
      if (replayState.enabled && replayViewportWarmup.assetsSettled && !replayViewportWarmup.ready) {
        const renderMs = renderEndMs - renderStartMs
        if (replayMeta?.replayWarmup === 'prewarm') {
          replayViewportWarmup.warmupFrames += 1
          replayViewportWarmup.maxWarmupRenderMs = Math.max(replayViewportWarmup.maxWarmupRenderMs, renderMs)
        } else if (replayMeta?.replayWarmup === 'restore') {
          replayViewportWarmup.restored = true
        } else if (replayMeta?.replayWarmup === 'post-restore-prewarm') {
          replayViewportWarmup.postRestoreWarmupFrames += 1
          replayViewportWarmup.maxWarmupRenderMs = Math.max(replayViewportWarmup.maxWarmupRenderMs, renderMs)
        } else if (replayMeta?.replayWarmup === 'final-restore') {
          replayViewportWarmup.postRestoreWarmupComplete = true
        }
      }
      applyPendingReplayCameraState()
      reportReplayCameraState(ws)
      if (replayState.enabled && ws.readyState === WebSocket.OPEN) {
        const longTaskMetrics = consumeReplayLongTaskMetrics()
        ws.send(JSON.stringify({
          type: 'replay_render_ready',
          tick: Number.isFinite(Number(state?.tick)) ? Number(state.tick) : null,
          render_ms: renderEndMs - renderStartMs,
          raf_delay_ms: frameNowMs - scheduledAtMs,
          frame_source: frameSource,
          replay_warmup: replayMeta?.replayWarmup ?? null,
          warmup_frames: replayViewportWarmup.warmupFrames,
          post_restore_warmup_frames: replayViewportWarmup.postRestoreWarmupFrames,
          warmup_max_render_ms: replayViewportWarmup.maxWarmupRenderMs,
          ws_delta_ms: trace?.wsDeltaMs ?? null,
          tick_delta: trace?.tickDelta ?? null,
          message_to_render_ms: trace?.receiveMs == null ? null : renderStartMs - trace.receiveMs,
          payload_bytes: trace?.payloadBytes ?? null,
          decode_ms: trace?.decodeMs ?? null,
          parse_ms: trace?.parseMs ?? null,
          apply_delta_ms: trace?.applyDeltaMs ?? null,
          pre_schedule_ms: trace?.preScheduleMs ?? null,
          renderer_profile: rendererProfile,
          queue_depth: replayFrameQueue.length,
          ...longTaskMetrics,
        }))
        maybeRequestReplayFrames('rendered-low-water')
      }
      notifyRendererStateReady(state)
      notifyReplayViewportWarmup()
      if (replayState.enabled) pumpReplayFrameQueue()
    })
  }

  ws.onopen = () => {
    chatState.wsConnected = true
    setChatConnection(replayState.enabled ? 'Replay' : 'Live')
    setStatus(
      replayState.enabled ? `Connected | Replay | ${rendererTarget}` : `Connected | ${rendererTarget}`,
      'ok'
    )
    if (!replayState.enabled && !chatState.loadedHistory) void loadChatHistory()
    if (replayState.enabled) {
      requestReplayFrame('open')
      startViewportReplayControlSync()
    }
  }
  ws.onerror = () => {
    setChatConnection('WS error')
    setStatus('WebSocket error', 'error')
    reportReplayViewportError('Replay connection failed before the first frame')
  }
  ws.onclose = (event) => {
    chatState.wsConnected = false
    setChatConnection('Offline')
    setStatus('Disconnected', 'warn')
    renderPlayerList([])
    reportReplayViewportError('Replay connection closed before the first frame', { code: event?.code ?? null })
  }

  ws.onmessage = (event) => {
    try {
      const rawReceiveMs = performance.now()
      const payloadBytes = event.data instanceof ArrayBuffer
        ? event.data.byteLength
        : typeof event.data === 'string'
          ? event.data.length
          : null
      let text
      if (event.data instanceof ArrayBuffer) {
        text = new TextDecoder().decode(event.data)
      } else {
        text = String(event.data)
      }
      const decodedMs = performance.now()
      const data = JSON.parse(text)
      const parsedMs = performance.now()
      if (data && data.type === 'audio_chunk') {
        onAudioChunk(data)
        return
      }
      if (data && data.type === 'chat_message') {
        appendChatMessage(data)
        return
      }
      if (data && data.type === 'spectator_delta') {
        const applyStartMs = performance.now()
        const state = applySpectatorDelta(latestSpectatorState, data)
        const appliedMs = performance.now()
        if (!state) {
          setStatus('Missing base spectator state; reconnecting', 'warn')
          ws.close()
          return
        }
        latestSpectatorState = state
        wsStats.frames += 1
        if (typeof state?.tick === 'number') wsStats.lastTick = state.tick
        const tick = Number(state?.tick)
        const trace = {
          receiveMs: rawReceiveMs,
          payloadBytes,
          decodeMs: decodedMs - rawReceiveMs,
          parseMs: parsedMs - decodedMs,
          applyDeltaMs: appliedMs - applyStartMs,
          preScheduleMs: performance.now() - rawReceiveMs,
          wsDeltaMs: lastReplayMessageMs == null ? null : rawReceiveMs - lastReplayMessageMs,
          tickDelta: Number.isFinite(tick) && Number.isFinite(lastReplayMessageTick) ? tick - lastReplayMessageTick : null,
        }
        lastReplayMessageMs = rawReceiveMs
        if (Number.isFinite(tick)) lastReplayMessageTick = tick
        renderPlayerList(state?.players)
        if (replayState.enabled && (replayState.agentLoaded || replayState.workspaceLoaded) && typeof state?.tick === 'number') {
          renderSidePanel(state.tick)
        }
        if (replayState.enabled) enqueueReplayRendererState(state, trace)
        else scheduleRendererState(state, trace)
        return
      }
      if (data && typeof data.type === 'string') {
        if (data.type === 'replay_error') {
          setStatus(data.message || 'Replay error', 'error')
          console.error('[replay]', data.message || 'Replay error', data)
        } else if (data.type === 'replay_camera_set') {
          applyReplayCameraState(data.state)
        }
        return
      }
      const state = data
      if (state && state.error) {
        setStatus(state.error, 'error')
        return
      }
      latestSpectatorState = state
      wsStats.frames += 1
      if (typeof state?.tick === 'number') wsStats.lastTick = state.tick
      const tick = Number(state?.tick)
      const trace = {
        receiveMs: rawReceiveMs,
        payloadBytes,
        decodeMs: decodedMs - rawReceiveMs,
        parseMs: parsedMs - decodedMs,
        applyDeltaMs: 0,
        preScheduleMs: performance.now() - rawReceiveMs,
        wsDeltaMs: lastReplayMessageMs == null ? null : rawReceiveMs - lastReplayMessageMs,
        tickDelta: Number.isFinite(tick) && Number.isFinite(lastReplayMessageTick) ? tick - lastReplayMessageTick : null,
      }
      lastReplayMessageMs = rawReceiveMs
      if (Number.isFinite(tick)) lastReplayMessageTick = tick
      renderPlayerList(state?.players)
      if (replayState.enabled && (replayState.agentLoaded || replayState.workspaceLoaded) && typeof state?.tick === 'number') {
        renderSidePanel(state.tick)
      }
      if (replayState.enabled) enqueueReplayRendererState(state, trace)
      else scheduleRendererState(state, trace)
    } catch (error) {
      console.error(error)
      setStatus('Failed to parse spectator state', 'error')
    }
  }

  window.addEventListener('beforeunload', () => {
    stopRendererFrameLoop()
    if (viewportReplayControlTimer) clearInterval(viewportReplayControlTimer)
    runtimeKit.internal.dispose()
    if (renderer.unmount) renderer.unmount()
    if (replayState.pollTimer) clearInterval(replayState.pollTimer)
    ws.close()
  })
}

if (replayPlayToggleEl) {
  replayPlayToggleEl.addEventListener('click', async () => {
    if (!replayState.enabled || !replayState.info) return
    try {
      if (replayState.info.paused) await postReplay('/replay/play')
      else await postReplay('/replay/pause')
      await refreshReplayInfo()
    } catch (err) {
      console.error(err)
      setStatus('Failed to update replay playback', 'error')
    }
  })
}

if (replaySliderEl) {
  replaySliderEl.addEventListener('pointerdown', () => {
    replayState.scrubbing = true
  })
  replaySliderEl.addEventListener('input', () => {
    if (!replayState.enabled || !replayState.info) return
    const tick = Number(replaySliderEl.value)
    if (replayTimeCurrentEl) replayTimeCurrentEl.textContent = formatDurationMs(tickToMs(tick, replayState.info))
  })
  replaySliderEl.addEventListener('change', async () => {
    if (!replayState.enabled) return
    try {
      const tick = Number(replaySliderEl.value)
      await postReplay(`/replay/seek?tick=${encodeURIComponent(String(tick))}`)
      await refreshReplayInfo()
    } catch (err) {
      console.error(err)
      setStatus('Failed to seek replay', 'error')
    } finally {
      replayState.scrubbing = false
    }
  })
  replaySliderEl.addEventListener('pointerup', () => {
    replayState.scrubbing = false
  })
}

if (replaySpeedEl) {
  replaySpeedEl.addEventListener('change', async () => {
    if (!replayState.enabled) return
    try {
      await postReplay(`/replay/speed?value=${encodeURIComponent(replaySpeedEl.value)}`)
      await refreshReplayInfo()
    } catch (err) {
      console.error(err)
      setStatus('Failed to change replay speed', 'error')
    }
  })
}

if (replaySkipIdleEl) {
  replaySkipIdleEl.addEventListener('change', async () => {
    if (!replayState.enabled) return
    try {
      await postReplay(`/replay/skip-idle?enabled=${replaySkipIdleEl.checked ? 'true' : 'false'}`)
      await refreshReplayInfo()
    } catch (err) {
      console.error(err)
      replaySkipIdleEl.checked = !replaySkipIdleEl.checked
      setStatus('Failed to update replay idle skipping', 'error')
    }
  })
}

if (behaviorPanelToggleEl) {
  setBehaviorPanelOpen(replayState.behaviorPanelOpen, false)
  behaviorPanelToggleEl.addEventListener('click', () => {
    setBehaviorPanelOpen(!replayState.behaviorPanelOpen, true)
  })
}

if (behaviorTimelineShellEl) {
  behaviorTimelineShellEl.addEventListener('dblclick', (ev) => {
    if (ev.target instanceof Element && ev.target.closest('.behavior-occurrence')) return
    if (!replayState.info) return
    resetBehaviorView(replayState.info)
    renderBehaviorPanel()
  })

  behaviorTimelineShellEl.addEventListener('pointerdown', (ev) => {
    if (!replayState.enabled || !replayState.behaviorPanelOpen || !replayState.info) return
    if (!(ev.target instanceof Element)) return
    if (ev.target.closest('.behavior-occurrence') || ev.target.closest('.behavior-row__label')) return
    beginBehaviorPan(ev.clientX)
  })
}

window.addEventListener('pointermove', (ev) => {
  if (!replayState.behaviorPanState) return
  updateBehaviorPan(ev.clientX)
})

window.addEventListener('pointerup', () => {
  if (!replayState.behaviorPanState) return
  endBehaviorPan()
})

if (agentLogToggleEl) {
  setAgentLogPanelOpen(replayState.agentPanelOpen, false)
  agentLogToggleEl.addEventListener('click', () => {
    setAgentLogPanelOpen(!replayState.agentPanelOpen, true)
    syncAgentLogVisibility()
  })
}

if (agentLogTabMessagesEl) {
  agentLogTabMessagesEl.addEventListener('click', () => setSidePanelTab('messages'))
}

if (agentLogTabWorkspaceEl) {
  agentLogTabWorkspaceEl.addEventListener('click', () => setSidePanelTab('workspace'))
}

bootstrap()
