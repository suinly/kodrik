<script setup lang="ts">
import {locale, t} from '../utils/i18n'
import { api, base, checkedFetch, login, messageOf, type Conversation, type ConversationDraft, type PublishedPage } from '../utils/api'
import {conversationTitle,shortConversationTitle} from '../utils/chat-presentation'
import {useWebhooks} from '../utils/webhooks'

const sessions = ref<Conversation[]>([])
const selected = ref<Conversation | null>(null)
const homeConversation: Conversation = {id:'new',title:'',busy:false,updated_at:null}
const conversationKey = ref(0)
const initializing = ref(true)
const error = ref('')
const menu = ref(false)
type Section = 'chat'|'pages'|'webhooks'
function readLocation():{section:Section;id:string|null} {
  const query=new URL(window.location.href).searchParams
  if(query.has('page')) return {section:'pages',id:query.get('page') || null}
  if(query.has('webhook')) return {section:'webhooks',id:query.get('webhook') || null}
  if(query.get('section')==='pages' || query.get('section')==='webhooks') return {section:query.get('section') as Section,id:null}
  return {section:'chat',id:query.get('conversation') || null}
}
const section = ref<Section>(readLocation().section)
const restoring = ref(false)
const locationError = ref(false)
let navigationRevision=0
const webhooks = useWebhooks(computed(()=>section.value==='webhooks'))
const sections = [
  {id:'chat',label:'Чат',icon:'i-lucide-messages-square'},
  {id:'pages',label:'Страницы',icon:'i-lucide-panels-top-left'},
  {id:'webhooks',label:'Вебхуки',icon:'i-lucide-webhook'},
] as const
const visibleSections = computed(()=>sections.filter(item=>item.id!=='webhooks' || webhooks.allowed))
const sectionTitle = computed(()=>section.value==='chat'?t('Диалоги'):section.value==='pages'?t('Страницы'):t('Вебхуки'))
const menuLabel = computed(()=>section.value==='chat'?t('Показать диалоги'):section.value==='pages'?t('Показать страницы'):t('Показать вебхуки'))
const selectedPage = ref<PublishedPage | null>(null)
const pageRevision = ref(0)
const pageDocumentUrl = computed(() => selectedPage.value ? new URL('document.json', selectedPage.value.url).href : '')
const mainTitle = computed(()=>section.value==='pages' ? selectedPage.value?.title ?? t('Страницы') : section.value==='webhooks' ? webhooks.current?.name ?? t(webhooks.editing?'Новый вебхук':'Вебхуки') : selected.value ? conversationTitle(selected.value) : t('Кодрик'))
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
onBeforeUnmount(()=>{disposed=true;clearTimeout(searchTimer);listRevision++;navigationRevision++})
const dateFormat = computed(() => new Intl.DateTimeFormat(locale.value,{day:'numeric',month:'short',hour:'2-digit',minute:'2-digit'}))
const chatUrl = new URL('chat/', base).pathname
const logoutUrl = new URL('auth/logout', base).pathname

function writeLocation() {
  navigationRevision++;restoring.value=false;locationError.value=false;error.value=''
  const id=section.value==='chat'?selected.value?.id:section.value==='pages'?selectedPage.value?.slug:webhooks.selected
  const url=new URL(window.location.href)
  for(const key of ['conversation','page','webhook','section']) url.searchParams.delete(key)
  url.hash=''
  if(id) url.searchParams.set(section.value==='chat'?'conversation':section.value==='pages'?'page':'webhook',id)
  else if(section.value!=='chat') url.searchParams.set('section',section.value)
  if(url.href===window.location.href) return
  window.history.replaceState(window.history.state,'',url)
}

async function restoreLocation() {
  const revision=++navigationRevision
  const target=readLocation()
  restoring.value=true;locationError.value=false;error.value='';menu.value=false;focusAfterMenu=false
  section.value=target.section
  try {
    if(target.section==='chat') {
      const previous=selected.value?.id
      if(previous!==target.id) {selected.value=null;conversationKey.value++}
      const conversation=target.id ? sessions.value.find(item=>item.id===target.id) ?? await api<Conversation>(`sessions/${encodeURIComponent(target.id)}`) : null
      if(revision!==navigationRevision) return
      if(selected.value?.id!==conversation?.id) conversationKey.value++
      selected.value=conversation
    } else if(target.section==='pages') {
      selectedPage.value=null
      if(target.id) {
        const response=await checkedFetch(new URL('api/pages',base))
        const pages:PublishedPage[]=await response.json()
        if(revision!==navigationRevision) return
        const page=pages.find(page=>page.slug===target.id)
        if(!page) throw new Error(t('Страница удалена или не найдена.'))
        selectedPage.value=page;pageRevision.value++
      }
    } else {
      webhooks.clear()
      await webhooks.load()
      if(revision!==navigationRevision) return
      if(webhooks.error) throw new Error(webhooks.error)
      if(!webhooks.allowed) throw new Error(t('Вебхуки доступны только владельцу.'))
      if(target.id) {
        const hook=webhooks.hooks.find(hook=>hook.id===target.id)
        if(!hook) throw new Error(t('Вебхук удалён или не найден.'))
        webhooks.choose(hook)
      }
    }
  } catch(reason) {
    if(revision===navigationRevision) {
      locationError.value=true;error.value=messageOf(reason)
      if(target.section==='chat') {selected.value=null;conversationKey.value++}
    }
  } finally {
    await nextTick()
    if(revision===navigationRevision) restoring.value=false
  }
}
watch(()=>webhooks.selected,()=>{
  if(section.value==='webhooks' && !restoring.value) writeLocation()
},{flush:'post'})

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
    if(section.value==='chat') error.value = ''
    if(selected.value) {
      const current=selected.value
      const item=sessions.value.find(item=>(item.root_id ?? item.id)===(current.root_id ?? current.id)) ?? await api<Conversation>(`sessions/${current.id}`)
      if(selected.value?.id===current.id) selected.value={...current,...item,id:current.id}
    } else if(section.value==='chat' && readLocation().id && !creating.value) {
      await restoreLocation()
    }
  } catch (reason) { error.value = messageOf(reason) }
}

async function openWebhookConversation(id:string) {
  const revision=navigationRevision
  try {
    const conversation=await api<Conversation>(`sessions/${id}`)
    if(revision!==navigationRevision) return
    select(conversation);await refresh()
  } catch(reason) {if(revision===navigationRevision) error.value=messageOf(reason)}
}

function select(conversation: Conversation, preserveView = false) {
  if(!preserveView && selected.value?.id !== conversation.id) conversationKey.value++
  section.value = 'chat'
  focusAfterMenu=menu.value
  const root=conversation.root_id ?? conversation.id
  try {
    const key=`kodrik.branches.${root}`
    const ids=JSON.parse(sessionStorage.getItem(key)||'[]')
    sessionStorage.setItem(key,JSON.stringify([...new Set([...ids,conversation.id])]))
  } catch {}
  selected.value = conversation
  writeLocation()
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

function openPage(page: PublishedPage) {
  section.value = 'pages'
  selectedPage.value = page
  pageRevision.value++
  focusAfterMenu = false
  menu.value = false
  writeLocation()
}

function closePage() {
  section.value = 'chat'
  writeLocation()
  void nextTick(() => conversationView.value?.focusInput())
}

function openSection(next:Section) {
  section.value=next
  writeLocation()
  focusAfterMenu=false
  if(window.matchMedia('(max-width: 1023px)').matches) menu.value=true
}

function chooseWebhook(hook?:typeof webhooks.hooks[number]) {
  webhooks.choose(hook)
  writeLocation()
  focusAfterMenu=false
  menu.value=false
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
        conversationKey.value++
        writeLocation()
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
  conversationKey.value++
  writeLocation()
  refresh()
}

async function create(fromHome = false) {
  if (creating.value) return
  const revision=navigationRevision
  creating.value = true
  try {
    const conversation = await api<Conversation>('sessions', 'POST')
    if(revision!==navigationRevision) return
    if(fromHome) {
      drafts[conversation.id] = draftFor('new')
      delete drafts.new
    }
    select(conversation, fromHome)
    await refresh()
    return conversation
  } catch (reason) {if(revision===navigationRevision) error.value=messageOf(reason)}
  finally { creating.value = false }
}

onMounted(() => {
  const target=section.value==='chat'?Promise.resolve():restoreLocation()
  void Promise.all([refresh(),target]).finally(() => { initializing.value = false })
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
    <aside class="hidden h-full w-14 shrink-0 flex-col items-center border-r border-default bg-muted/40 py-3 lg:flex">
      <nav :aria-label="t('Разделы')" class="flex w-full flex-col items-center gap-2">
        <UButton v-for="item in visibleSections" :key="item.id" :icon="item.icon" :aria-label="t(item.label)" :title="t(item.label)"
          :aria-current="section===item.id?'page':undefined" :color="section===item.id?'primary':'neutral'" :variant="section===item.id?'soft':'ghost'"
          :disabled="webhooks.saving" size="lg" class="size-11 justify-center p-0" @click="openSection(item.id)" />
      </nav>
    </aside>
    <UDashboardSidebar v-model:open="menu" :default-size="18"
      :menu="{content:{onCloseAutoFocus:sidebarCloseFocus}}"
      :toggle="{size:'xl'}" :ui="{root:'min-h-0',body:'overflow-hidden',content:'chat-viewport bg-default'}">
      <template #header>
        <UButton :to="chatUrl" external target="_self" aria-label="kodrik" variant="link" size="xl" class="px-0"><KodrikLogo class="h-6" /></UButton>
      </template>
      <nav :aria-label="t('Разделы')" class="flex shrink-0 flex-col gap-1 border-b border-default pb-3 lg:hidden">
        <UButton v-for="item in visibleSections" :key="item.id" :icon="item.icon" :label="t(item.label)"
          :aria-current="section===item.id?'page':undefined" :color="section===item.id?'primary':'neutral'" :variant="section===item.id?'soft':'ghost'"
          :disabled="webhooks.saving" size="xl" class="justify-start" @click="openSection(item.id)" />
      </nav>
      <h2 class="text-sm font-semibold text-highlighted">{{ sectionTitle }}</h2>
      <template v-if="section==='chat'">
      <UButton :label="t('Новый диалог')" icon="i-lucide-plus" size="xl" block :loading="creating" @click="create()" />
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
      </template>
      <PageCatalog v-else-if="section==='pages'" :selected="selectedPage?.slug" @open="openPage" />
      <div v-else-if="webhooks.allowed" class="flex min-h-0 flex-1 flex-col gap-3">
        <div class="flex items-center gap-1">
          <UButton :label="t('Создать вебхук')" icon="i-lucide-plus" :disabled="webhooks.saving || webhooks.loading" class="min-w-0 flex-1 justify-center" @click="chooseWebhook()" />
          <UButton icon="i-lucide-refresh-cw" :aria-label="t('Обновить')" :title="t('Обновить')" color="neutral" variant="ghost" :loading="webhooks.loading" :disabled="webhooks.saving" @click="webhooks.load" />
        </div>
        <p v-if="webhooks.loading" role="status" class="text-sm text-muted">{{ t('Загружаю вебхуки…') }}</p>
        <p v-else-if="!webhooks.hooks.length" class="text-sm text-muted">{{ t('Пока нет вебхуков.') }}</p>
        <nav :aria-label="t('Список вебхуков')" class="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto overscroll-contain">
          <UButton v-for="hook in webhooks.hooks" :key="hook.id" :label="hook.name" color="neutral" :variant="webhooks.selected===hook.id?'soft':'ghost'"
            :aria-current="webhooks.selected===hook.id?'page':undefined" :disabled="webhooks.saving || webhooks.loading" class="shrink-0 justify-start" @click="chooseWebhook(hook)">
            <span class="min-w-0 flex-1 truncate text-left">{{ hook.name }}</span>
            <UIcon v-if="!hook.enabled" name="i-lucide-pause" :aria-label="t('Отключён')" class="shrink-0 text-muted" />
          </UButton>
        </nav>
      </div>
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
        <UDashboardNavbar :title="mainTitle">
          <template #left>
            <div v-if="section==='pages' && selectedPage" class="flex min-w-0 items-center gap-1.5">
              <UButton icon="i-lucide-arrow-left" :aria-label="t('Вернуться в диалог')" :title="t('Вернуться в диалог')" color="neutral" variant="ghost" @click="closePage" />
              <h1 class="min-w-0 truncate font-semibold text-highlighted">{{ selectedPage.title }}</h1>
            </div>
            <div v-else-if="section==='chat'" class="conversation-heading flex min-w-0 items-center gap-1.5">
              <h1 class="min-w-0 truncate font-semibold text-highlighted">
                <UChatShimmer v-if="selected?.title_pending" :text="selected.title" data-testid="navbar-title-shimmer" :aria-label="t('Создаю название диалога')" class="truncate motion-reduce:animate-none" />
                <span v-else>{{ selected ? conversationTitle(selected) : t("Кодрик") }}</span>
              </h1>
              <UButton v-if="selected" icon="i-lucide-pencil" :aria-label="t('Переименовать диалог')" color="neutral" variant="ghost" size="sm" class="conversation-rename shrink-0" @click="manage(selected,'rename')" />
            </div>
            <h1 v-else class="min-w-0 truncate font-semibold text-highlighted">{{ mainTitle }}</h1>
          </template>
          <template #toggle><UDashboardSidebarToggle :aria-label="menuLabel" :disabled="webhooks.saving" size="xl" /></template>
          <template #right>
            <template v-if="section==='pages' && selectedPage">
              <UButton icon="i-lucide-refresh-cw" :aria-label="t('Обновить страницу')" :title="t('Обновить страницу')" color="neutral" variant="ghost" @click="pageRevision++" />
              <UButton :to="selectedPage.url" external target="_blank" rel="noopener noreferrer" icon="i-lucide-external-link" :aria-label="t('Открыть в новой вкладке')" :title="t('Открыть в новой вкладке')" color="neutral" variant="ghost" />
            </template>
            <UButton v-else-if="section==='chat' && selected" icon="i-lucide-search" :aria-label="t('Найти в диалоге')" :aria-pressed="conversationSearchOpen" color="neutral" variant="ghost" @click="conversationSearchOpen=!conversationSearchOpen" />
          </template>
        </UDashboardNavbar>
      </template>
      <template #body>
        <p v-if="restoring" role="status" class="p-4 text-sm text-muted">{{ t('Открываю ссылку…') }}</p>
        <div v-if="error" class="m-4 space-y-1">
          <p role="alert" class="text-sm text-error break-words">{{ t(error) }}</p>
          <div class="flex flex-wrap gap-2">
            <UButton :label="t('Повторить')" color="neutral" variant="link" size="sm" @click="restoreLocation" />
            <UButton :label="t('Войти')" :to="login" external color="neutral" variant="link" size="sm" />
          </div>
        </div>
        <section v-if="section==='pages'" :aria-label="t('Просмотр страницы')" class="flex min-h-0 flex-1 flex-col overflow-y-auto">
          <template v-if="selectedPage">
            <DocumentPage v-if="selectedPage.document" :key="`${selectedPage.slug}:${pageRevision}`" :url="pageDocumentUrl" :chat-url="chatUrl" :login-url="login" embedded />
            <iframe v-else :key="`${selectedPage.slug}:${pageRevision}`" :src="selectedPage.url" :title="selectedPage.title" class="min-h-0 w-full flex-1 border-0 bg-default" />
          </template>
          <p v-else-if="!restoring" class="p-6 text-sm text-muted">{{ t('Выберите страницу в боковом меню.') }}</p>
        </section>
        <div v-show="section==='webhooks' && !restoring" class="min-h-0 flex-1 overflow-y-auto">
          <LazyWebhooksManager v-if="webhooks.allowed && !locationError" :manager="webhooks" @open="openWebhookConversation" />
        </div>
        <ChatConversation v-if="selected || !initializing" v-show="section==='chat' && !restoring && !locationError" ref="conversationView" :key="conversationKey" :conversation="selected ?? homeConversation" :draft-state="draftFor(selected?.id ?? 'new')" :start="selected ? undefined : () => create(true)" :blocked="creating" :initial-search="conversationSearch" :search-open="conversationSearchOpen" @close-search="conversationSearchOpen=false;conversationSearch=''" @saved="refresh" @navigate="openVersion" @removed="removed" />
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
