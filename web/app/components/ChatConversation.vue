<script setup lang="ts">
import {t} from '../utils/i18n'
import { api, checkedFetch, endpoint, login, requestStatus, AuthenticationError, ConversationNotFoundError, backoff, watchSnapshots, messageOf, type Conversation, type ConversationDraft, type Snapshot, type ChatMessage } from '../utils/api'
import {supersededDocuments,matchingMessages} from '../utils/chat-presentation'
import { TableKit } from '@tiptap/extension-table'
import { TaskList, TaskItem } from '@tiptap/extension-list'
import { CharacterCount } from '@tiptap/extensions/character-count'
import { Slice } from '@tiptap/pm/model'

const props = defineProps<{ conversation: Conversation; draftState: ConversationDraft; blocked: boolean;initialSearch?:string;searchOpen?:boolean;start?:()=>Promise<Conversation|undefined> }>()
const home = computed(() => !!props.start)
const homeView = !!props.start
const emit = defineEmits<{ saved: [];navigate:[id:string];removed:[];'close-search':[] }>()
const search=ref(props.initialSearch ?? '')
const searchInput=useTemplateRef('searchInput')
const matchIndex=ref(0)
const matches=computed(()=>matchingMessages(snapshot.value?.messages ?? [],search.value))
const editingMessage=ref<string|null>(null)
type Pending = { id: string; text: string; files?: string[] }
const pendingKey = computed(() => `kodrik.pending.${props.conversation.id}`)
let pending: Pending | null = null
try { pending = JSON.parse(sessionStorage.getItem(pendingKey.value) || 'null') } catch {}
if(pending && !props.draftState.text) props.draftState.text=pending.text
const draft = toRef(props.draftState,'text')
const files = toRef(props.draftState,'files')
const attachmentsOpen = ref(false)
const loading = ref(!props.start)
const submitting = ref(false)
const busy = ref(props.conversation.busy)
const stopping = ref(false)
const childStopping = ref(new Set<string>())
const childProblems = ref<Record<string,string>>({})
const resuming = ref(false)
const problem = ref('')
const connection = ref('')
const hint = ref(pending?.files?.length ? t("Повторно выберите вложения, если предыдущая отправка не была принята.") : '')
const authExpired = ref(false)
const snapshot = ref<Snapshot | null>(null)
const prompt = useTemplateRef('prompt')
const editorExtensions = [TableKit, TaskList, TaskItem, CharacterCount.configure({limit:32768,autoTrim:false})]
const transcript = useTemplateRef('transcript')
const transcriptContent = useTemplateRef('transcriptContent')
const editorLocked = computed(() => loading.value || submitting.value || props.blocked || authExpired.value)
const locked = computed(() => editorLocked.value || stopping.value || resuming.value || busy.value && !!snapshot.value?.cancel_requested)
const hasDraft = computed(() => !!draft.value.trim() || !!files.value.length)
const hasInput = computed(() => hasDraft.value || !!pending)
const superseded = computed(()=>supersededDocuments(snapshot.value?.messages ?? []))
const queuedMessages = computed<ChatMessage[]>(()=>(snapshot.value?.queued ?? []).map((item,index)=>({
  id:item.id,role:'user',parts:[
    {type:'text',text:item.text,format:(item.webhook ?? (props.conversation.webhook && !snapshot.value?.messages.some(message=>message.role==='user') && index===0)) ? 'code' : undefined},
    ...(item.files ?? []).map(filename=>({type:'file' as const,filename,mediaType:'application/octet-stream'})),
    ...Array.from({length:item.photos ?? 0},(_,index)=>({type:'file' as const,filename:t("Фото {0}", index+1),mediaType:'image/png'})),
  ],
})))
const planSummary = computed(()=>{
  const steps=snapshot.value?.plan?.plan ?? []
  const current=steps.findIndex(step=>step.status==='in_progress')
  return current>=0 ? t("План · {0} из {1} · {2}", current+1, steps.length, steps[current]!.step) : t("План · {0} из {1}", steps.filter(step=>step.status==='completed').length, steps.length)
})
let revision = -1
let lastBusy = false
const follow = ref(true)
let lastScrollTop = 0
let mounted = true
let scrollObserver: ResizeObserver | undefined
let searchObserver: MutationObserver | undefined
const controller = new AbortController()

function highlightSearch() {
  if(!CSS.highlights) return
  const query=search.value.trim()
  if(!query) {CSS.highlights.delete('conversation-search');return}
  const highlight=new Highlight()
  const pattern=new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g,'\\$&'),'giu')
  for(const id of matches.value) {
    const message=transcript.value?.querySelector(`[data-message-id="${CSS.escape(id)}"]`)
    for(const content of message?.querySelectorAll('[data-search-text]') ?? []) {
      const walker=document.createTreeWalker(content,NodeFilter.SHOW_TEXT)
      let node:Node|null
      while((node=walker.nextNode())) {
        for(const match of node.textContent!.matchAll(pattern)) {
          const range=new Range()
          range.setStart(node,match.index)
          range.setEnd(node,match.index+match[0].length)
          highlight.add(range)
        }
      }
    }
  }
  CSS.highlights.set('conversation-search',highlight)
}
watch(search,highlightSearch,{flush:'post'})

async function showMatch(index:number) {
  matchIndex.value=matches.value.length ? (index+matches.value.length)%matches.value.length : 0
  follow.value=false
  await nextTick()
  const id=matches.value[matchIndex.value]
  if(id) transcript.value?.querySelector(`[data-message-id="${CSS.escape(id)}"]`)?.scrollIntoView({block:'center',behavior:'smooth'})
}
watch([search,()=>matches.value.join('\n')],()=>showMatch(0))
watch(()=>props.initialSearch,value=>{search.value=value ?? ''})
watch(()=>props.searchOpen,async open=>{
  if(!open) {search.value='';return}
  await nextTick();searchInput.value?.inputRef?.focus({preventScroll:true})
})
function closeSearch() {search.value='';emit('close-search')}

async function editMessage(message:ChatMessage,text:string) {
  if(locked.value || busy.value || snapshot.value?.queued.length || editingMessage.value) return
  editingMessage.value=message.id
  const key=`kodrik.edit.${message.id}`
  try {
    if(new TextEncoder().encode(text).length>32768) throw new Error(t("Сообщение превышает 32 KiB текста."))
    let pendingEdit:{request_id:string;text:string}|null=null
    try {pendingEdit=JSON.parse(sessionStorage.getItem(key)||'null')} catch {}
    if(!pendingEdit || pendingEdit.text!==text) pendingEdit={request_id:crypto.randomUUID(),text}
    sessionStorage.setItem(key,JSON.stringify(pendingEdit))
    const index=message.id.split(':').at(-1)
    const branch=await api<{id:string}>(`sessions/${props.conversation.id}/messages/${index}/edit`,'POST',pendingEdit)
    sessionStorage.removeItem(key)
    if(mounted) emit('navigate',branch.id)
  } catch(reason) {if(mounted) problem.value=messageOf(reason)}
  finally {editingMessage.value=null}
}

function persistDraft() {
  try {
    const names=files.value.length?files.value.map(file=>file.name):props.draftState.missingFiles
    if(draft.value || names.length) sessionStorage.setItem(`kodrik.draft.${props.conversation.id}`,JSON.stringify({text:draft.value,files:names}))
    else sessionStorage.removeItem(`kodrik.draft.${props.conversation.id}`)
    if(homeView && props.conversation.id !== 'new') sessionStorage.removeItem('kodrik.draft.new')
  } catch { hint.value=t("Не удалось сохранить черновик в браузере. Не закрывайте вкладку до отправки.") }
}
watch([draft,files],()=>{props.draftState.revision++},{deep:true,flush:'sync'})
watch([draft,files,()=>props.draftState.missingFiles],persistDraft,{deep:true,flush:'sync'})

function scrollToLatest() {
  follow.value=true
  if(transcript.value) {
    transcript.value.scrollTop=transcript.value.scrollHeight
    lastScrollTop=transcript.value.scrollTop
  }
}

function trackScroll() {
  const view=transcript.value
  if(!view) return
  if(view.scrollHeight-view.scrollTop-view.clientHeight<100) follow.value=true
  else if(view.scrollTop<lastScrollTop) follow.value=false
  lastScrollTop=view.scrollTop
}

function clearPending() {
  const id=pending?.id
  pending = null
  if(JSON.parse(sessionStorage.getItem(pendingKey.value)||'null')?.id===id) sessionStorage.removeItem(pendingKey.value)
  if(props.draftState.pendingFiles?.id===id) props.draftState.pendingFiles=undefined
}

function receive(view: Snapshot) {
  if (!mounted || view.session_id !== props.conversation.id) return
  connection.value = ''
  loading.value = false
  if (view.revision <= revision) return
  const titleChanged = (!!view.title && view.title !== (snapshot.value?.title ?? props.conversation.title)) ||
    (snapshot.value !== null && view.titles_revision !== undefined && view.titles_revision !== snapshot.value.titles_revision) ||
    (view.title_pending !== undefined && view.title_pending !== (snapshot.value?.title_pending ?? props.conversation.title_pending ?? false)) ||
    (view.title_default !== undefined && view.title_default !== (snapshot.value?.title_default ?? props.conversation.title_default ?? false))
  revision = view.revision
  snapshot.value = view
  for(const child of view.subagents ?? []) if(child.status!=='running') {
    delete childProblems.value[child.id]
  }
  busy.value = view.busy
  if (view.cancel_requested || !view.busy) stopping.value = false
  hint.value = view.blocked_reason ? view.queued.length ? t("Очередь приостановлена. Нажмите «Продолжить очередь».") : t("Выполнение остановлено. Можно отправить новое сообщение.") :
    view.cancel_requested && view.busy ? t("Останавливаю и сохраняю историю…") :
    view.phase === 'Cancelled' ? t("Остановлено") :
    typeof view.phase === 'object' && view.phase?.Failed ? t("Агент не смог завершить ответ. История сохранена.") :
    typeof view.phase === 'object' && view.phase?.Parked ? t("Запуск приостановлен после повторных сбоев.") : ''
  if ((lastBusy && !view.busy) || titleChanged) emit('saved')
  lastBusy = view.busy
}

async function reload(block = false) {
  if (block) loading.value = true
  try { receive(await api<Snapshot>(`sessions/${props.conversation.id}/run`)) }
  catch (error) {
    if (!mounted) return
    if (error instanceof ConversationNotFoundError) {emit('removed');return}
    if (error instanceof AuthenticationError) authExpired.value = true
    problem.value = messageOf(error)
    loading.value = false
  }
}

async function observe() {
  let delay = 500
  while (!controller.signal.aborted) {
    try {
      const response = await checkedFetch(endpoint(`sessions/${props.conversation.id}/run`), { signal: controller.signal })
      receive(await response.json())
      await watchSnapshots(props.conversation.id, controller.signal, view => { receive(view); delay = 500 })
    } catch (error) {
      if (controller.signal.aborted) break
      if (error instanceof ConversationNotFoundError) {emit('removed');break}
      if (error instanceof AuthenticationError) {
        authExpired.value = true
        problem.value = messageOf(error)
        loading.value = false
        break
      }
      connection.value = t("Соединение потеряно, работа продолжается")
      await backoff(delay, controller.signal)
      delay = Math.min(delay * 2, 8000)
    }
  }
}
onMounted(() => {
  document.addEventListener('keydown', recoverInputFocus, {signal:controller.signal})
  searchObserver = new MutationObserver(highlightSearch)
  if (transcript.value) searchObserver.observe(transcript.value,{childList:true,subtree:true,characterData:true})
  scrollObserver = new ResizeObserver(() => {
    if (follow.value) scrollToLatest()
  })
  if (transcript.value) scrollObserver.observe(transcript.value)
  if (transcriptContent.value) scrollObserver.observe(transcriptContent.value)
  if(home.value) void nextTick(focusInput)
  else void observe()
})
watch(() => props.conversation.id, () => { persistDraft(); void observe() })
onBeforeUnmount(() => { mounted = false; controller.abort(); scrollObserver?.disconnect(); searchObserver?.disconnect(); CSS.highlights?.delete('conversation-search') })
function focusInput() {
  if (mounted && !editorLocked.value) prompt.value?.editor?.commands.focus(undefined,{scrollIntoView:false})
}
function recoverInputFocus(event:KeyboardEvent) {
  if(event.defaultPrevented || event.isComposing || event.ctrlKey || event.metaKey || event.altKey || [...event.key].length!==1 || editorLocked.value) return
  const target=event.target
  if(target instanceof HTMLElement && (target.isContentEditable || target.closest('input,textarea,select,[role="textbox"],[role="combobox"]'))) return
  if(event.key===' ' && target instanceof HTMLElement && target.closest('button,a,summary,[role="button"]')) return
  const input=prompt.value?.editor?.view.dom
  if(!input || !input.isContentEditable || !input.getClientRects().length) return
  if([...document.querySelectorAll('[role="dialog"],[role="alertdialog"],[role="menu"],[role="listbox"]')].some(element=>element.getClientRects().length)) return
  // Focus synchronously so the browser inserts the first character at the saved selection.
  prompt.value?.editor?.view.focus()
}
defineExpose({focusInput})
watch([editorLocked,()=>prompt.value?.editor,()=>t('Сообщение')], async ([locked,editor,label],previous) => {
  if(!editor) return
  // UEditor applies options only at creation; keep its live instance in sync.
  editor.setOptions({editable:!locked,editorProps:{...editor.options.editorProps,attributes:{
    ...editor.options.editorProps.attributes,'aria-label':label,'aria-readonly':String(locked),
  }}})
  editor.view.dispatch(editor.state.tr)
  if(!locked && (previous[0]!==locked || previous[1]!==editor)) {
    await nextTick()
    focusInput()
  }
})
watch(snapshot, async () => {
  await nextTick()
  if (follow.value) scrollToLatest()
}, { flush: 'post' })

function selectFiles(selected: File[] | null | undefined) {
  if (editorLocked.value) return
  const next = selected ?? []
  if (next.length > 10 || next.reduce((size, file) => size + file.size, 0) > 50_000_000) {
    problem.value = t("Можно отправить до 10 вложений, суммарно до 50 MB.")
    return
  }
  for (const file of next) {
    const image = file.type.startsWith('image/') || /\.(png|jpe?g|webp)$/i.test(file.name)
    if (!file.size || file.size > (image ? 10 * 1024 * 1024 : 20_000_000)) {
      problem.value = t("«{0}»: файл пустой или слишком большой. Фото — до 10 MiB, документ — до 20 MB.", file.name)
      return
    }
  }
  const submitted=props.draftState.pendingFiles
  if(submitted) submitted.files=submitted.files.filter(file=>!files.value.includes(file)||next.includes(file))
  files.value = next
  props.draftState.missingFiles=[]
  problem.value = ''
}

function drop(event: DragEvent) {
  if (event.dataTransfer?.files.length) selectFiles([...files.value, ...event.dataTransfer.files])
}
function paste(event: ClipboardEvent) {
  if (!event.clipboardData?.files.length) return
  event.preventDefault()
  selectFiles([...files.value, ...event.clipboardData.files])
}
function pasteMarkdown(event:ClipboardEvent):boolean {
  const editor=prompt.value?.editor
  const text=event.clipboardData?.getData('text/plain')
  if(!text || event.clipboardData?.getData('text/html') || !editor?.markdown || editor.isActive('codeBlock') || editor.isActive('code')) return false
  // Open paragraphs for inline paste, while leaving isolating tables intact.
  const content=editor.schema.nodeFromJSON(editor.markdown.parse(text)).content
  editor.view.dispatch(editor.state.tr.replaceSelection(Slice.maxOpen(content,false)).setMeta('paste',true).setMeta('uiEvent','paste').scrollIntoView())
  return true
}
function editorKeydown(event: KeyboardEvent) {
  if(event.isComposing || event.keyCode===229 || prompt.value?.editor?.view.composing) return false
  if(event.key==='Escape') {
    prompt.value?.editor?.view.dom.blur()
    return true
  }
  if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey && !event.metaKey && !event.altKey) {
    event.preventDefault()
    void submit()
    return true
  }
  return false
}

async function fingerprints(selected: File[]) {
  return Promise.all(selected.map(async file => {
    const digest = await crypto.subtle.digest('SHA-256', await file.arrayBuffer())
    return `${file.name}:${file.size}:${Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, '0')).join('')}`
  }))
}

async function submit() {
  if (locked.value || !hasInput.value) return
  submitting.value = true
  prompt.value?.editor?.view.focus()
  const text = draft.value
  let selectedFiles = [...files.value]
  let sent = false
  let sentRevision = -1
  try {
    if (new TextEncoder().encode(text).length > 32768) throw new Error(t("Сообщение превышает 32 KiB текста."))
    let keys = await fingerprints(selectedFiles)
    if (!mounted) return
    if(props.start) {
      const conversation = await props.start()
      if(!conversation || !mounted) return
      await nextTick()
      persistDraft()
    }
    if (pending) {
      const saved = await requestStatus(props.conversation.id, pending.id)
      if (!mounted) return
      const markdown=prompt.value?.editor?.markdown
      const sameText=pending.text===text || !!markdown && markdown.serialize(markdown.parse(pending.text))===markdown.serialize(markdown.parse(text))
      const changed = !sameText || (selectedFiles.length > 0 && JSON.stringify(pending.files ?? []) !== JSON.stringify(keys))
      if(saved && changed && props.draftState.pendingFiles?.id===pending.id) {
        const submitted=props.draftState.pendingFiles.files
        keys=keys.filter((_,index)=>!submitted.includes(selectedFiles[index]!))
        selectedFiles=selectedFiles.filter(file=>!submitted.includes(file))
        files.value=files.value.filter(file=>!submitted.includes(file))
      }
      if (saved?.status === 'Queued' && saved.blocked_reason && changed) {
        clearPending()
      } else if (saved) {
        if (saved.status === 'Queued' || saved.status === 'Running') {
          busy.value = !saved.blocked_reason
          hint.value = saved.blocked_reason ? t("Запрос сохранён; очередь приостановлена.") : t("Запрос уже принят. Проверяю результат…")
          if(changed) clearPending()
          else {
            if(!saved.blocked_reason) { draft.value=''; files.value=[]; props.draftState.missingFiles=[] }
            await reload(true)
            return
          }
        } else {
          clearPending()
          hint.value = t("Результат предыдущей отправки загружен.")
          if(!changed) {
            draft.value='';files.value=[];props.draftState.missingFiles=[]
            await reload(true)
            return
          }
        }
      }
      if (pending && changed) clearPending()
      if(pending?.files?.length && !selectedFiles.length) {
        if(props.draftState.pendingFiles?.id===pending.id) {
          selectedFiles=[...props.draftState.pendingFiles.files]
          keys=await fingerprints(selectedFiles)
          if(!mounted) return
        }
        if(!selectedFiles.length) throw new Error(t("Предыдущий запрос не принят. Повторно выберите вложения перед отправкой."))
      }
    }
    if (!text.trim() && !selectedFiles.length) return
    if (!pending) pending = { id: crypto.randomUUID(), text, files: keys }
    sessionStorage.setItem(pendingKey.value, JSON.stringify(pending))
    props.draftState.pendingFiles={id:pending.id,files:selectedFiles}
    draft.value = ''
    files.value=[]
    sentRevision=props.draftState.revision
    problem.value = ''
    hint.value = ''
    follow.value = true
    loading.value = true
    sent = true
    const body = selectedFiles.length ? new FormData() : JSON.stringify({ id: props.conversation.id, request_id: pending.id, text: pending.text })
    if (body instanceof FormData) {
      body.set('id', props.conversation.id)
      body.set('request_id', pending.id)
      body.set('text', pending.text)
      selectedFiles.forEach(file => body.append('files', file, file.name))
    }
    // The observer's abort signal must never cancel a durable submission.
    await checkedFetch(endpoint('send'), { method: 'POST', body,
      ...(typeof body === 'string' ? { headers: { 'Content-Type': 'application/json' } } : {}) })
    // Draft state outlives this component; remove only this request's selection.
    const clearRestoredDraft=props.draftState.revision===sentRevision && draft.value===text
    files.value=files.value.filter(file=>!selectedFiles.includes(file))
    props.draftState.missingFiles=props.draftState.missingFiles.filter(name=>!selectedFiles.some(file=>file.name===name))
    if(clearRestoredDraft) draft.value=''
    persistDraft()
    clearPending()
    if (!mounted) return
    attachmentsOpen.value = false
    await reload(true)
    emit('saved')
  } catch (error) {
    if (!mounted) return
    if (error instanceof AuthenticationError) authExpired.value = true
    problem.value = messageOf(error)
    if (sent) { draft.value = text; files.value=selectedFiles; await reload() }
  } finally { submitting.value = false }
}

async function stop() {
  stopping.value = true
  try { await api(`sessions/${props.conversation.id}/stop`, 'POST'); await reload() }
  catch (error) { stopping.value = false; problem.value = messageOf(error) }
}

async function stopChild(id:string) {
  if(editorLocked.value || childStopping.value.has(id)) return
  childStopping.value.add(id)
  delete childProblems.value[id]
  try {
    await api(`sessions/${props.conversation.id}/subagents/${encodeURIComponent(id)}/stop`,'POST')
    if(mounted) await reload()
  } catch(error) {
    if(!mounted) return
    if(error instanceof AuthenticationError) {authExpired.value=true;problem.value=messageOf(error)}
    childProblems.value[id]=error instanceof ConversationNotFoundError ? t('Диалог или дочерняя задача недоступны. Обновите историю.') : messageOf(error)
  } finally {childStopping.value.delete(id)}
}

async function resumeQueue() {
  resuming.value=true
  try {await api(`sessions/${props.conversation.id}/resume`,'POST');await reload(true);emit('saved')}
  catch(error) {if(error instanceof AuthenticationError) authExpired.value=true;problem.value=messageOf(error)}
  finally {resuming.value=false}
}

const compactionLabels = {
  preparing: t("Готовлю краткий контекст…"), ready: t("Краткий контекст подготовлен"),
  applied: t("Контекст сокращён; полная история сохранена"), stale: t("Подготовленный контекст устарел"),
  failed: t("Не удалось подготовить краткий контекст"), cancelled: t("Подготовка контекста отменена"),
}
</script>

<template>
  <section :aria-label="t('Разговор с Кодриком')" class="flex min-h-0 flex-1 flex-col">
    <div v-if="searchOpen || search.trim()" class="mx-auto flex w-full max-w-3xl shrink-0 items-center gap-1 px-4 py-2 sm:px-6">
      <UInput ref="searchInput" v-model="search" :aria-label="t('Поиск в диалоге')" :placeholder="t('Найти в диалоге…')" icon="i-lucide-search" size="sm" class="min-w-0 flex-1" @keydown.enter.prevent="showMatch(matchIndex+1)" @keydown.esc="closeSearch" />
      <template v-if="search.trim()">
        <span role="status" class="whitespace-nowrap text-xs text-muted tabular-nums">{{ matches.length ? `${matchIndex+1} / ${matches.length}` : t("Нет совпадений") }}</span>
        <UButton icon="i-lucide-chevron-up" :aria-label="t('Предыдущее совпадение')" color="neutral" variant="ghost" size="sm" :disabled="!matches.length" @click="showMatch(matchIndex-1)" />
        <UButton icon="i-lucide-chevron-down" :aria-label="t('Следующее совпадение')" color="neutral" variant="ghost" size="sm" :disabled="!matches.length" @click="showMatch(matchIndex+1)" />
      </template>
      <UButton icon="i-lucide-x" :aria-label="t('Закрыть поиск')" color="neutral" variant="ghost" size="sm" @click="closeSearch" />
    </div>
    <div class="conversation-layout" :class="{'conversation-layout-home':homeView,'conversation-layout-centered':home && !submitting}">
    <div class="relative flex min-h-0 flex-1 flex-col">
      <div ref="transcript" data-testid="chat-transcript" class="min-h-0 flex-1 overflow-y-auto overscroll-contain" :class="{'flex flex-col justify-end':home}" @scroll="trackScroll">
        <div ref="transcriptContent" class="mx-auto w-full max-w-3xl space-y-5 px-4 py-6 sm:px-6">
          <p v-if="loading" role="status" class="text-muted text-sm"> {{ t("Загружаю историю…") }} </p>
          <div v-if="!loading && !submitting && !snapshot?.messages.length && !queuedMessages.length" class="text-center space-y-3" :class="home ? 'pb-4' : 'py-12'">
            <KodrikLogo v-if="home" class="mx-auto mb-5 h-10" />
            <h1 class="text-2xl font-semibold text-highlighted">{{ home ? t('С чего начнём?') : t('Есть мысль?') }}</h1>
            <p class="text-muted">{{ home ? t('Задайте вопрос, поделитесь идеей или отправьте файл и фото — разберёмся вместе.') : t('Начните с вопроса, задачи, файла или фотографии.') }}</p>
          </div>
          <ChatItem v-for="message in snapshot?.messages ?? []" :key="message.id" :message="message" :superseded="superseded" :actions="!editorLocked"
            :editable="message.role==='user' && !locked && !busy && !snapshot?.queued.length && !editingMessage" :saving="editingMessage===message.id"
            :search-selected="matches[matchIndex]===message.id"
            @edit="text=>editMessage(message,text)" @version="id=>emit('navigate',id)"
            @prompt="text=>{if(!editorLocked){draft=draft.trim()?`${draft}\n\n${text}`:text;focusInput()}}" />
          <div v-if="snapshot?.live.current?.text" data-testid="partial">
            <p v-if="snapshot.live.current.status === 'Interrupted'" class="text-muted text-sm"> {{ t("Незавершённая попытка") }} </p>
            <ChatItem :message="{ id: 'partial', role: 'assistant', parts: [{ type: 'text', text: snapshot.live.current.text }] }" />
          </div>
          <UCollapsible v-if="snapshot?.live.interrupted?.text">
            <UButton :label="t('Предыдущая незавершённая попытка')" variant="ghost" color="neutral" trailing-icon="i-lucide-chevron-down" />
            <template #content><ChatMarkdown :text="snapshot.live.interrupted.text" /></template>
          </UCollapsible>
          <ChatItem v-for="message in queuedMessages" :key="message.id" :message="message"
            :status="snapshot?.blocked_reason ? t('Передача приостановлена') : t('Ожидает передачи агенту')" />
          <UChatShimmer v-if="busy && !hint && !stopping" as="p" :text="t('Кодрик работает…')" role="status" aria-live="polite" class="text-sm" />
        </div>
      </div>
      <div v-if="!follow" class="pointer-events-none absolute inset-x-0 bottom-4 flex justify-center bg-transparent">
        <UButton :label="t('К последнему сообщению')" icon="i-lucide-arrow-down" color="neutral" variant="outline" class="pointer-events-auto" @click="scrollToLatest" />
      </div>
    </div>
    <div data-testid="chat-composer" class="mx-auto flex max-h-[75%] w-full max-w-3xl shrink-0 flex-col gap-2 px-3 py-2 sm:px-6" @dragover.prevent @drop.prevent.capture="drop" @paste.capture="paste">
      <div v-if="problem || draftState.missingFiles.length || snapshot?.blocked_reason && snapshot.queued.length || attachmentsOpen || files.length || snapshot?.plan || snapshot?.subagents?.length" class="min-h-0 space-y-3 overflow-y-auto overscroll-contain">
        <div v-if="problem" class="space-y-1">
          <p role="alert" class="text-sm text-error break-words">{{ t(problem) }}</p>
          <div class="flex flex-wrap gap-2">
            <UButton v-if="!home" :label="t('Обновить историю')" color="neutral" variant="link" size="sm" @click="problem = ''; reload(true)" />
            <UButton v-if="authExpired" :label="t('Войти')" :to="login()" external color="neutral" variant="link" size="sm" />
          </div>
        </div>
        <UAlert v-if="draftState.missingFiles.length" variant="soft" :description="t('Повторно выберите вложения: {0}. Браузер не сохраняет файлы после перезагрузки.', draftState.missingFiles.join(', '))"
          :actions="[{label:t('Убрать напоминание'),onClick:()=>{draftState.missingFiles=[]}}]" />
        <UAlert v-if="snapshot?.blocked_reason && snapshot.queued.length" variant="soft" :description="t('Передача сообщений приостановлена.')"
          :actions="[{label:t('Продолжить очередь'),loading:resuming,disabled:editorLocked || busy || resuming,onClick:resumeQueue}]" />
        <UFileUpload v-if="attachmentsOpen || files.length" :model-value="files" multiple layout="list" :disabled="editorLocked" :dropzone="false"
          :label="t('Добавить файлы и фото')" :description="t('До 10 вложений · фото до 10 MiB · файлы до 20 MB')"
          icon="i-lucide-paperclip" class="max-h-56 overflow-y-auto" @update:model-value="selectFiles" />
        <ChatSubagents v-if="snapshot?.subagents?.length" :agents="snapshot.subagents" :stopping="childStopping" :problems="childProblems" :disabled="editorLocked"
          @stop="stopChild" @refresh="reload()" />
        <UAccordion v-if="snapshot?.plan" as="section" :aria-label="t('План выполнения')" data-testid="composer-plan"
          :items="[{ label: planSummary, icon: 'i-lucide-list-checks', value: 'plan' }]"
          class="rounded-lg bg-elevated/50 px-3" :ui="{body:'max-h-48 overflow-y-auto'}">
          <template #body>
            <div class="space-y-2">
              <p v-if="snapshot.plan.explanation" class="text-sm text-muted">{{ snapshot.plan.explanation }}</p>
              <ol :aria-label="t('Шаги плана')" class="space-y-2">
                <li v-for="(step, index) in snapshot.plan.plan" :key="index" class="flex items-start gap-2 text-sm">
                  <UIcon :name="step.status === 'completed' ? 'i-lucide-circle-check' : step.status === 'in_progress' ? 'i-lucide-loader-circle' : 'i-lucide-circle'"
                    class="mt-0.5 shrink-0 text-primary" :class="{ 'motion-safe:animate-spin': step.status === 'in_progress' }" />
                  {{ step.step }}
                </li>
              </ol>
            </div>
          </template>
        </UAccordion>
      </div>
      <UChatPrompt v-model="draft" as="div" :placeholder="t('Напишите Кодрику…')"
        class="chat-prompt shrink-0 flex-row items-end gap-1.5 p-1"
        @submit="submit">
        <template #body>
          <UEditor ref="prompt" v-model="draft" content-type="markdown" :placeholder="{placeholder:()=>t('Напишите Кодрику…'),mode:'firstLine'}"
            :editable="!editorLocked" :extensions="editorExtensions" :mention="false"
            :markdown="{markedOptions:{breaks:true}}"
            :editor-props="{handleKeyDown:(_view,event)=>editorKeydown(event),handlePaste:(_view,event)=>pasteMarkdown(event)}"
            :aria-label="t('Сообщение')" role="textbox" aria-multiline="true" :aria-readonly="editorLocked"
            name="chat-message" autocomplete="off" enterkeyhint="send" class="chat-editor min-w-0 flex-1"
            :ui="{base:'min-h-8 max-h-26 overflow-auto px-0! py-1 text-base/6 text-default [scrollbar-width:none] *:my-0 [&_p]:leading-6 [&_li]:my-0'}" />
        </template>
        <template #header>
          <UButton type="button" icon="i-lucide-paperclip" color="neutral" variant="ghost" size="md" :aria-label="t('Прикрепить файлы и фото')" :disabled="editorLocked" @click="attachmentsOpen = !attachmentsOpen" />
        </template>
        <template #footer>
          <UButton v-if="busy && !hasDraft" type="button" icon="i-lucide-square" color="neutral" variant="outline"
            :aria-label="stopping ? t('Останавливаю') : t('Остановить')" size="md" :disabled="submitting || stopping || !!snapshot?.cancel_requested || authExpired" @click="stop" />
          <UButton v-else type="button" icon="i-lucide-arrow-up" :aria-label="t('Отправить')" size="md" :disabled="locked || !hasInput" @click="submit" />
        </template>
      </UChatPrompt>
      <div v-if="snapshot?.compaction || connection || stopping || hint" class="min-h-0 space-y-1 overflow-y-auto text-xs text-muted">
        <p v-if="snapshot?.compaction" role="status">{{ t(compactionLabels[snapshot.compaction.status]) }}</p>
        <p v-if="connection" role="status">{{ t(connection) }}</p>
        <p v-if="stopping || hint" role="status" aria-live="polite">{{ stopping ? t("Останавливаю и сохраняю историю…") : t(hint) }}</p>
      </div>
      <p class="chat-keyboard-hint hidden text-center text-xs text-muted sm:block"> {{ t("Enter — отправить · Shift + Enter — новая строка") }} </p>
    </div>
    <div v-if="homeView" aria-hidden="true" class="min-h-0" />
    </div>
  </section>
</template>

<style scoped>
.chat-editor :deep(table) { border-collapse: collapse; width: 100%; }
.chat-editor :deep(th), .chat-editor :deep(td) { border: 1px solid var(--ui-border); padding: 0.125rem 0.5rem; min-width: 5rem; }
.chat-editor :deep(ul[data-type='taskList']) { list-style: none; padding: 0; }
.chat-editor :deep(li[data-type='taskItem']) { display: flex; align-items: baseline; gap: 0.5rem; padding: 0; }
.chat-editor :deep(li[data-type='taskItem'] > div) { flex: 1; }
.conversation-layout { display: flex; flex: 1; min-height: 0; flex-direction: column; }
.conversation-layout-home { display: grid; grid-template-rows: minmax(0, 1fr) auto minmax(0, 0fr); transition: grid-template-rows 280ms ease; }
.conversation-layout-home > [data-testid='chat-composer'] { max-height: calc(var(--chat-viewport-height, 100dvh) * 0.75); }
@media (min-width: 1024px) {
  .conversation-layout-centered { grid-template-rows: minmax(0, 1fr) auto minmax(0, 1fr); }
}
@media (prefers-reduced-motion: reduce) {
  .conversation-layout-home { transition: none; }
}
</style>
