<script setup lang="ts">
import {locale, t} from '../utils/i18n'
import { api, base, login, messageOf, type Conversation, type ConversationDraft } from '../utils/api'
import {conversationTitle,shortConversationTitle} from '../utils/chat-presentation'

const sessions = ref<Conversation[]>([])
const selected = ref<Conversation | null>(null)
const error = ref('')
const menu = ref(false)
const conversationView = useTemplateRef('conversationView')
let focusAfterMenu=false
const creating = ref(false)
const search = ref('')
const listing = ref(false)
const listError = ref('')
const retryReset = ref(true)
const hasMore = ref(false)
const searching = computed(()=>listing.value && !!search.value.trim())
const conversationSearch = ref('')
const conversationSearchOpen = ref(false)
const managing = ref<Conversation|null>(null)
const management = ref<'rename'|'delete'|null>(null)
const title = ref('')
const saving = ref(false)
const drafts = reactive<Record<string,ConversationDraft>>({})
const visibleSessions = sessions
const pageSize=30
let listQuery=''
let nextOffset=0
let listRevision=0
let disposed=false
let searchTimer:ReturnType<typeof setTimeout>|undefined
async function loadSessions(reset=false) {
  const query=search.value.trim()
  if(disposed || !reset && (listing.value || !hasMore.value || listQuery!==query)) return false
  const target=reset && listQuery===query ? Math.max(pageSize,nextOffset) : pageSize
  const revision=++listRevision
  let offset=reset ? 0 : nextOffset
  const items=reset ? [] : [...sessions.value]
  let more=false
  listing.value=true
  listError.value=''
  try {
    do {
      const found=await api<Conversation[]>(`sessions?limit=${pageSize}&offset=${offset}&q=${encodeURIComponent(query)}`)
      if(disposed || revision!==listRevision) return false
      for(const item of found) {
        const index=items.findIndex(existing=>(existing.root_id ?? existing.id)===(item.root_id ?? item.id))
        if(index<0) items.push(item)
        else items[index]=item
      }
      offset+=found.length
      more=found.length===pageSize
    } while(reset && more && offset<target)
    sessions.value=items
    listQuery=query
    nextOffset=offset
    hasMore.value=more
    return true
  } catch(reason) {
    if(revision===listRevision) {listError.value=messageOf(reason);retryReset.value=reset}
    return false
  }
  finally {if(revision===listRevision) listing.value=false}
}
function scrollSessions(event:Event) {
  const list=event.currentTarget as HTMLElement
  if(list.scrollHeight-list.scrollTop-list.clientHeight<80) void loadSessions()
}
watch(search,()=>{
  clearTimeout(searchTimer);listRevision++;listing.value=false
  sessions.value=[];nextOffset=0;hasMore.value=false
  searchTimer=setTimeout(()=>{void loadSessions(true)},200)
})
onBeforeUnmount(()=>{disposed=true;clearTimeout(searchTimer);listRevision++})
const dateFormat = computed(() => new Intl.DateTimeFormat(locale.value,{day:'numeric',month:'short',hour:'2-digit',minute:'2-digit'}))
const chatUrl = new URL('chat/', base).pathname
const logoutUrl = new URL('auth/logout', base).pathname

function draftFor(id:string):ConversationDraft {
  if(!drafts[id]) {
    let text='',missingFiles:string[]=[]
    try {
      const saved=JSON.parse(sessionStorage.getItem(`kodrik.draft.${id}`)||'null')
      if(typeof saved?.text==='string') text=saved.text
      if(Array.isArray(saved?.files)) missingFiles=saved.files.filter((file:unknown)=>typeof file==='string')
    } catch {}
    drafts[id]={text,files:[],missingFiles,revision:0}
  }
  return drafts[id]!
}

async function refresh() {
  try {
    if(!await loadSessions(true)) return
    error.value = ''
    const id = new URL(window.location.href).searchParams.get('conversation')
    if(selected.value) {
      const current=selected.value
      const item=sessions.value.find(item=>(item.root_id ?? item.id)===(current.root_id ?? current.id)) ?? await api<Conversation>(`sessions/${current.id}`)
      if(selected.value?.id===current.id) selected.value={...current,...item,id:current.id}
    } else if(id) selected.value=sessions.value.find(item=>item.id===id) ?? await api<Conversation>(`sessions/${id}`)
  } catch (reason) { error.value = messageOf(reason) }
}

function select(conversation: Conversation) {
  focusAfterMenu=menu.value
  const root=conversation.root_id ?? conversation.id
  try {
    const key=`kodrik.branches.${root}`
    const ids=JSON.parse(sessionStorage.getItem(key)||'[]')
    sessionStorage.setItem(key,JSON.stringify([...new Set([...ids,conversation.id])]))
  } catch {}
  const url = new URL(window.location.href)
  url.searchParams.set('conversation', conversation.id)
  window.history.replaceState(null, '', url)
  selected.value = conversation
  conversationSearch.value=conversation.snippet ? search.value.trim() : ''
  conversationSearchOpen.value=!!conversationSearch.value
  menu.value = false
  if(!focusAfterMenu) void nextTick(()=>conversationView.value?.focusInput())
  api(`sessions/${conversation.id}/select`,'POST').catch(reason=>{error.value=messageOf(reason)})
}

function sidebarCloseFocus(event:Event) {
  if(!focusAfterMenu) return
  focusAfterMenu=false
  event.preventDefault()
  void nextTick(()=>conversationView.value?.focusInput())
}

function manage(session:Conversation,action:'rename'|'delete') {
  menu.value=false
  managing.value=session;title.value=conversationTitle(session);management.value=action;error.value=''
}
async function togglePin(session:Conversation) {
  try {
    await api(`sessions/${session.id}/pin`,'POST',{pinned:!session.pinned})
    await refresh()
  } catch(reason) {error.value=messageOf(reason)}
}
async function saveManagement() {
  if(!managing.value || saving.value) return
  saving.value=true
  const session=managing.value
  try {
    await api(`sessions/${session.id}`,management.value==='rename'?'PATCH':'DELETE',management.value==='rename'?{title:title.value}:undefined)
    if(management.value==='delete') {
      const root=session.root_id ?? session.id
      if((selected.value?.root_id ?? selected.value?.id)===root) {
        selected.value=null
        const url=new URL(window.location.href);url.searchParams.delete('conversation');window.history.replaceState(null,'',url)
      }
      try {
        const ids=JSON.parse(sessionStorage.getItem(`kodrik.branches.${root}`)||'[]')
        for(const id of [root,session.id,...ids]) {
          delete drafts[id];sessionStorage.removeItem(`kodrik.draft.${id}`);sessionStorage.removeItem(`kodrik.pending.${id}`)
        }
        sessionStorage.removeItem(`kodrik.branches.${root}`)
      } catch {}
    }
    management.value=null
    await refresh()
  } catch(reason) {error.value=messageOf(reason)}
  finally {saving.value=false}
}

function openVersion(id:string) {
  if(!selected.value) return
  select({...selected.value,id,snippet:null,match_message:null})
  refresh()
}
function removed() {
  selected.value=null
  const url=new URL(window.location.href);url.searchParams.delete('conversation');window.history.replaceState(null,'',url)
  refresh()
}

async function create() {
  if (creating.value) return
  creating.value = true
  try {
    select(await api<Conversation>('sessions', 'POST'))
    await refresh()
  } catch (reason) { error.value = messageOf(reason) }
  finally { creating.value = false }
}

onMounted(() => {
  refresh()
  const viewport = window.visualViewport
  const updateViewport = () => {
    // Safari's keyboard shrinks the visual viewport, not the layout viewport.
    if (!viewport || viewport.scale !== 1) return
    document.documentElement.style.setProperty('--chat-viewport-height', `${viewport.height}px`)
    document.documentElement.style.setProperty('--chat-viewport-top', `${viewport.offsetTop}px`)
  }
  updateViewport()
  viewport?.addEventListener('resize', updateViewport)
  viewport?.addEventListener('scroll', updateViewport)
  onBeforeUnmount(() => {
    viewport?.removeEventListener('resize', updateViewport)
    viewport?.removeEventListener('scroll', updateViewport)
    document.documentElement.style.removeProperty('--chat-viewport-height')
    document.documentElement.style.removeProperty('--chat-viewport-top')
  })
})
</script>

<template>
  <UDashboardGroup unit="rem" class="chat-viewport bg-default">
    <UDashboardSidebar v-model:open="menu" :default-size="18"
      :menu="{content:{onCloseAutoFocus:sidebarCloseFocus}}"
      :toggle="{size:'xl'}" :ui="{root:'min-h-0',body:'overflow-hidden',content:'chat-viewport bg-default'}">
      <template #header>
        <UButton :to="chatUrl" external aria-label="kodrik" variant="link" size="xl" class="px-0"><KodrikLogo class="h-6" /></UButton>
      </template>
      <UButton :label="t('Новый диалог')" icon="i-lucide-plus" size="xl" block :loading="creating" @click="create" />
      <p class="text-xs font-medium text-muted mt-4"> {{ t("Диалоги") }} </p>
      <UInput v-model="search" :aria-label="t('Поиск диалогов')" :placeholder="t('Поиск по переписке…')" icon="i-lucide-search" :loading="searching" size="xl" class="w-full" />
      <nav :aria-label="t('История диалогов')" class="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto overscroll-contain" @scroll.passive="scrollSessions">
        <div v-for="session in visibleSessions" :key="session.root_id ?? session.id" class="conversation-row relative flex shrink-0 items-center">
        <UButton
          :aria-label="conversationTitle(session)" :title="conversationTitle(session)" :data-conversation-id="session.id" color="neutral"
          :variant="(selected?.root_id ?? selected?.id) === (session.root_id ?? session.id) ? 'soft' : 'ghost'"
          :aria-current="(selected?.root_id ?? selected?.id) === (session.root_id ?? session.id) ? 'page' : undefined"
          :disabled="creating" class="w-full justify-start pr-10"
          @click="select(session)">
          <UIcon v-if="session.pinned" name="i-lucide-pin" :aria-label="t('Закреплённый диалог')" class="shrink-0 text-muted" />
          <UIcon v-if="session.busy" name="i-lucide-loader-circle" class="shrink-0 animate-spin" :aria-label="t('Выполняется')" />
          <span class="min-w-0 flex-1 text-left">
            <UChatShimmer v-if="session.title_pending" :text="shortConversationTitle(session.title)" data-testid="title-shimmer" :aria-label="t('Создаю название диалога')" class="block truncate motion-reduce:animate-none" />
            <span v-else class="block truncate">{{ shortConversationTitle(conversationTitle(session)) }}</span>
            <time v-if="session.updated_at" :datetime="session.updated_at" class="block text-xs text-muted">{{ dateFormat.format(new Date(session.updated_at)) }}</time>
            <span v-if="session.snippet" class="block truncate text-xs text-muted">{{ session.snippet }}</span>
          </span>
        </UButton>
        <UDropdownMenu :items="[{label:session.pinned?t('Открепить диалог'):t('Закрепить диалог'),icon:session.pinned?'i-lucide-pin-off':'i-lucide-pin',onSelect:()=>togglePin(session)},{label:t('Переименовать'),icon:'i-lucide-pencil',onSelect:()=>manage(session,'rename')},{label:t('Удалить диалог'),icon:'i-lucide-trash-2',color:'error',disabled:session.busy,onSelect:()=>manage(session,'delete')}]">
          <UButton icon="i-lucide-ellipsis" :aria-label="t('Действия с диалогом «{0}»', conversationTitle(session))" color="neutral" variant="ghost" size="sm" class="conversation-actions absolute right-1" />
        </UDropdownMenu>
        </div>
        <p v-if="listError" role="alert" class="text-sm text-error">{{ t(listError) }}</p>
        <UButton v-if="listError || hasMore" :label="listError ? t('Повторить загрузку') : t('Показать ещё')" :loading="listing" color="neutral" variant="link" class="shrink-0 justify-center" @click="listError && retryReset ? refresh() : loadSessions()" />
        <p v-else-if="listing" role="status" class="text-sm text-muted"> {{ t("Загружаю диалоги…") }} </p>
      </nav>
      <p v-if="!listing && !listError && !sessions.length" class="text-sm text-muted">{{ search.trim() ? t("Диалоги не найдены.") : t("Здесь появятся ваши разговоры.") }}</p>
      <template #footer>
        <div class="flex w-full items-center justify-between">
          <LanguageSwitch />
          <ThemeSwitch />
          <form :action="logoutUrl" method="post">
            <UButton type="submit" color="neutral" variant="ghost" icon="i-lucide-log-out" :aria-label="t('Выйти')" :title="t('Выйти')" size="sm" />
          </form>
        </div>
      </template>
    </UDashboardSidebar>

    <UDashboardPanel id="chat" :ui="{root:'min-h-0',body:'min-h-0 p-0 sm:p-0 overflow-hidden'}">
      <template #header>
        <UDashboardNavbar :title="selected ? conversationTitle(selected) : t('Кодрик')">
          <template #left>
            <div class="conversation-heading flex min-w-0 items-center gap-1.5">
              <h1 class="min-w-0 truncate font-semibold text-highlighted">
                <UChatShimmer v-if="selected?.title_pending" :text="selected.title" data-testid="navbar-title-shimmer" :aria-label="t('Создаю название диалога')" class="truncate motion-reduce:animate-none" />
                <span v-else>{{ selected ? conversationTitle(selected) : t("Кодрик") }}</span>
              </h1>
              <UButton v-if="selected" icon="i-lucide-pencil" :aria-label="t('Переименовать диалог')" color="neutral" variant="ghost" size="sm" class="conversation-rename shrink-0" @click="manage(selected,'rename')" />
            </div>
          </template>
          <template #toggle><UDashboardSidebarToggle :aria-label="t('Показать диалоги')" size="xl" /></template>
          <template #right><UButton v-if="selected" icon="i-lucide-search" :aria-label="t('Найти в диалоге')" :aria-pressed="conversationSearchOpen" color="neutral" variant="ghost" @click="conversationSearchOpen=!conversationSearchOpen" /></template>
        </UDashboardNavbar>
      </template>
      <template #body>
        <div v-if="error" class="m-4 space-y-1">
          <p role="alert" class="text-sm text-error break-words">{{ t(error) }}</p>
          <div class="flex flex-wrap gap-2">
            <UButton :label="t('Повторить')" color="neutral" variant="link" size="sm" @click="refresh" />
            <UButton :label="t('Войти')" :to="login" external color="neutral" variant="link" size="sm" />
          </div>
        </div>
        <ChatConversation v-if="selected" ref="conversationView" :key="selected.id" :conversation="selected" :draft-state="draftFor(selected.id)" :blocked="creating" :initial-search="conversationSearch" :search-open="conversationSearchOpen" @close-search="conversationSearchOpen=false;conversationSearch=''" @saved="refresh" @navigate="openVersion" @removed="removed" />
        <div v-else class="flex flex-1 flex-col items-center justify-center gap-5 p-6 text-center">
          <KodrikLogo class="h-10" />
          <h1 class="text-3xl font-semibold text-highlighted"> {{ t("С чего начнём?") }} </h1>
          <p class="max-w-md text-muted"> {{ t("Задайте вопрос, поделитесь идеей или отправьте файл и фото — разберёмся вместе.") }} </p>
          <UButton :label="t('Начать разговор')" icon="i-lucide-plus" size="lg" :loading="creating" @click="create" />
        </div>
      </template>
    </UDashboardPanel>
  </UDashboardGroup>
  <UModal :open="management!==null" :title="management==='rename'?t('Переименовать диалог'):t('Удалить диалог?')" :dismissible="!saving" @update:open="open=>{if(!open) management=null}">
    <template #body>
      <form v-if="management==='rename'" id="rename-conversation" @submit.prevent="saveManagement">
        <UFormField :label="t('Название диалога')"><UInput v-model="title" :aria-label="t('Название диалога')" :maxlength="200" :disabled="saving" autofocus class="w-full" /></UFormField>
      </form>
      <p v-else class="text-sm text-muted">{{ t('Удалить «{0}» и все версии переписки вместе с вложениями? Это действие нельзя отменить.', managing ? conversationTitle(managing) : '') }}</p>
      <p v-if="error" role="alert" class="mt-3 text-sm text-error">{{ t(error) }}</p>
    </template>
    <template #footer>
      <UButton :label="t('Отмена')" color="neutral" variant="ghost" :disabled="saving" @click="management=null" />
      <UButton v-if="management==='rename'" :label="t('Сохранить название')" type="submit" form="rename-conversation" :disabled="!title.trim()" :loading="saving" />
      <UButton v-else :label="t('Удалить навсегда')" color="error" :loading="saving" @click="saveManagement" />
    </template>
  </UModal>
</template>

<style scoped>
@media (hover: hover) and (pointer: fine) {
  .conversation-actions, .conversation-rename { opacity: 0; }
  .conversation-heading:hover .conversation-rename,
  .conversation-heading:focus-within .conversation-rename,
  .conversation-row:hover .conversation-actions,
  .conversation-row:focus-within .conversation-actions,
  .conversation-actions[data-state='open'] { opacity: 1; }
}
</style>
