import {ref} from 'vue'
import {api, base, checkedFetch, endpoint, messageOf} from './api.ts'
import {language} from './i18n.ts'

export const pushEnabled = ref(false)
export const pushReady = ref(false)
export const pushWorking = ref(false)
export const pushProblem = ref('')
export const pushPermission = ref<NotificationPermission>(typeof Notification === 'undefined' ? 'default' : Notification.permission)
export const pushSupported = typeof window !== 'undefined' && window.isSecureContext &&
  'serviceWorker' in navigator && typeof PushManager !== 'undefined' && typeof Notification !== 'undefined'

let registration: ServiceWorkerRegistration | undefined
let subscription: PushSubscription | null = null
let publicKey = ''
let initializing: Promise<void> | undefined
let presence = Promise.resolve()
const tabId = crypto.randomUUID()
const disabledKey = 'kodrik.push.disabled'
function rememberDisabled(disabled: boolean) {
  try { localStorage.setItem(disabledKey, String(disabled)) } catch { /* Browser storage may be unavailable. */ }
}
function isDisabled() {
  try { return localStorage.getItem(disabledKey) === 'true' } catch { return false }
}
function keyBytes(key: string) {
  return Uint8Array.from(atob(key.replace(/-/g, '+').replace(/_/g, '/')), char => char.charCodeAt(0))
}
async function saveSubscription(current: PushSubscription) {
  await api('push', 'POST', {subscription: current.toJSON(), locale: language.value})
}

export function refreshPushPermission() {
  if (!pushSupported) return
  pushPermission.value = Notification.permission
  if (pushPermission.value !== 'granted') pushEnabled.value = false
}

export async function initializePush() {
  if (!pushSupported || pushReady.value) return
  if (initializing) return initializing
  initializing = (async () => {
    pushProblem.value = ''
    try {
      const config = await api<{public_key: string}>('push')
      publicKey = config.public_key
      registration = await navigator.serviceWorker.register(new URL('push-sw.js', base), {scope: base.pathname, updateViaCache: 'none'})
      if (!registration.active) registration = await navigator.serviceWorker.ready
      subscription = await registration.pushManager.getSubscription()
      refreshPushPermission()
      if (subscription && pushPermission.value === 'granted' && !isDisabled()) {
        const previousKey = subscription.options.applicationServerKey
        const expected = keyBytes(publicKey)
        if (previousKey && expected.length === previousKey.byteLength && expected.every((byte, index) => byte === new Uint8Array(previousKey)[index])) {
          await saveSubscription(subscription)
          pushEnabled.value = true
        } else {
          await subscription.unsubscribe()
          subscription = null
        }
      }
      pushReady.value = true
    } catch (error) { pushProblem.value = messageOf(error) }
    finally { initializing = undefined }
  })()
  return initializing
}

export async function enablePush() {
  if (!registration || !pushReady.value || pushWorking.value) return
  pushWorking.value = true
  pushProblem.value = ''
  try {
    // Request permission before awaiting anything, while the click is still active.
    pushPermission.value = await Notification.requestPermission()
    if (pushPermission.value !== 'granted') return
    subscription = await registration.pushManager.getSubscription() ?? await registration.pushManager.subscribe({
      userVisibleOnly: true, applicationServerKey: keyBytes(publicKey).buffer,
    })
    await saveSubscription(subscription)
    rememberDisabled(false)
    pushEnabled.value = true
  } catch (error) {
    rememberDisabled(true)
    pushEnabled.value = false
    if (subscription) {
      const current = subscription
      subscription = null
      await current.unsubscribe().catch(() => {})
      await api('push', 'DELETE', {endpoint: current.endpoint}).catch(() => {})
    }
    pushProblem.value = messageOf(error)
  }
  finally { pushWorking.value = false }
}

export async function disablePush() {
  if (!subscription || pushWorking.value) return
  pushWorking.value = true
  pushProblem.value = ''
  try {
    await api('push', 'DELETE', {endpoint: subscription.endpoint})
    rememberDisabled(true)
    pushEnabled.value = false
    await subscription.unsubscribe()
    subscription = null
  } catch (error) { pushProblem.value = messageOf(error) }
  finally { pushWorking.value = false }
}

export function updatePushPresence(sessionId: string | null) {
  if (!registration || !pushReady.value) return Promise.resolve()
  const visibleSession = document.visibilityState === 'visible' && document.hasFocus() ? sessionId : null
  // Keep navigation/focus updates ordered so a delayed heartbeat cannot restore old presence.
  presence = presence.catch(() => {}).then(async () => {
    const current = await registration!.pushManager.getSubscription()
    refreshPushPermission()
    const enabled = !!current && pushPermission.value === 'granted' && !isDisabled()
    if (enabled && (!pushEnabled.value || current.endpoint !== subscription?.endpoint)) await saveSubscription(current)
    subscription = current
    pushEnabled.value = enabled
    if (!enabled || !current) return
    const body = JSON.stringify({endpoint: current.endpoint, tab_id: tabId, session_id: visibleSession})
    await checkedFetch(endpoint('push/presence'), {method: 'POST', headers: {'Content-Type': 'application/json'}, body, keepalive: true})
  }).catch(() => {})
  return presence
}
