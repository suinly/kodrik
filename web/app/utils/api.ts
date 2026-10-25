import {locale,t} from './i18n.ts'
const config = JSON.parse(document.getElementById('kodrik-config')!.textContent!);
export const base = new URL(config.prefix, window.location.href);
export const endpoint = (path: string) => new URL(`api/chat/${path}`, base).toString();
export const login = () => new URL(`auth/login?return_to=${encodeURIComponent(window.location.pathname+window.location.search)}`, base).toString();
// Snapshot attachment paths are relative to the original chat directory.
export const attachmentUrl = (url?: string) => url ? new URL(url, new URL('chat/', base)).href : undefined;

export class AuthenticationError extends Error {}
export class ConversationNotFoundError extends Error {}
export const messageOf = (error: unknown) => error instanceof TypeError ? t('Не удалось подключиться к серверу.') : error instanceof Error ? t(error.message) : t("Не удалось подключиться к серверу.");

export type Conversation = { id: string; root_id?:string; title: string; title_default?:boolean; title_pending?: boolean; pinned?:boolean; busy: boolean; updated_at: string | null; snippet?:string|null; match_message?:string|null;webhook?:{id:string;name:string}|null };
export type PublishedPage = {slug: string; title: string; url: string; visibility: 'private' | 'public'; document: boolean};
export type ConversationDraft = { text: string; files: File[]; missingFiles: string[]; revision: number; pendingFiles?: {id:string;files:File[]} };
export type MessagePart =
  | { type: 'text'; text: string;format?:'code' }
  | { type: 'file'; filename: string; mediaType: string; url?: string }
  | { type: 'dynamic-tool'; toolCallId: string; toolName: string; state: string; input: unknown; output?: unknown; errorText?: string };
export type ChatMessage = { id: string; role: 'user' | 'assistant'; parts: MessagePart[]; created_at?:string|null; versions?:string[]; version?:number };
export type Subagent = {id:string;task:string;status:'running'|'completed'|'stopped'|'failed'|'interrupted';result:string|null;error:string|null;cancel_requested?:boolean};

export async function checkedFetch(input: RequestInfo | URL, init?: RequestInit) {
  const headers=new Headers(init?.headers);
  headers.set('Accept-Language',locale.value);
  const response = await fetch(input, {...init,headers});
  if (!response.ok) {
    if (response.status === 401) throw new AuthenticationError(t("Сессия истекла. Войдите снова."));
    if (response.status === 404) throw new ConversationNotFoundError(t("Диалог удалён или не найден."));
    if (response.status === 409) {
      const detail=await response.json().catch(()=>null);
      throw new Error(detail?.error==='Resume the queue in its original interface' ? t("Продолжите ожидающую очередь в исходном интерфейсе Telegram или CLI.") : t("Этот диалог ещё выполняется. Дождитесь завершения или остановите его."));
    }
    if (response.status === 429) {
      const detail=await response.json().catch(()=>null);
      if(detail?.error==='Too many password attempts') throw new Error(t('Слишком много попыток смены пароля. Подождите минуту.'));
      throw new Error(detail?.error==='Inbox queue is full' ? t("Очередь сообщений заполнена. Дождитесь выполнения предыдущих сообщений.") : t("Слишком много активных диалогов. Попробуйте позже."));
    }
    if (response.status === 413) throw new Error(t("Слишком большой файл или сообщение. Фото — до 10 MiB, файл — до 20 MB, суммарно — до 50 MB."));
    if (response.status === 400) {
      const detail = await response.json().catch(() => null);
      throw new Error(t(detail?.error || 'Проверьте текст, формат и размер вложений.'));
    }
    throw new Error(t("Не удалось выполнить запрос ({0}). Обновите историю перед повторной отправкой.", response.status));
  }
  return response;
}

export async function api<T>(path: string, method = 'GET', body?:unknown): Promise<T> {
  const response = await checkedFetch(endpoint(path), { method, ...(body===undefined?{}:{headers:{'Content-Type':'application/json'},body:JSON.stringify(body)}) });
  if (response.status === 202 || response.status === 204) return undefined as T;
  return response.json();
}

export type RequestStatus = { status: 'Queued' | 'Running' | 'Done' | 'Cancelled' | 'Unanswered'; blocked_reason: string | null };
export async function requestStatus(conversation: string, request: string): Promise<RequestStatus | null> {
  const response = await fetch(endpoint(`sessions/${conversation}/requests/${request}`),{headers:{'Accept-Language':locale.value}});
  if (response.status === 404) return null;
  if (response.status === 401) throw new AuthenticationError(t("Сессия истекла. Войдите снова."));
  if (!response.ok) throw new Error(t("Не удалось проверить отправку. Повторите проверку позже."));
  return response.json();
}


export type Snapshot = {
  type: 'snapshot'; session_id: string; run_id: string | null; revision: number;
  title?: string;
  title_default?: boolean;
  titles_revision?: number;
  title_pending?: boolean;
  messages: ChatMessage[]; busy: boolean; cancel_requested: boolean;
  subagents?: Subagent[];
  phase: string | { Failed?: { reason: string }; Parked?: { reason: string } } | null;
  blocked_reason: string | null;
  live: { current: Partial | null; interrupted: Partial | null };
  plan: { explanation?: string; plan: { step: string; status: string }[] } | null;
  queued: { id: string; text: string; files?: string[]; photos?: number; webhook?: boolean }[];
  compaction?: { status: 'preparing' | 'ready' | 'applied' | 'stale' | 'failed' | 'cancelled' } | null;
};
type Partial = { attempt: number; text: string; status: 'Active' | 'Ready' | 'Interrupted' | 'Committed' };

// Snapshots replace canonical messages; the SDK's append-only UIMessageChunk stream cannot do this.
export async function watchSnapshots(conversation: string, signal: AbortSignal, receive: (snapshot: Snapshot) => void) {
  const response = await checkedFetch(endpoint(`sessions/${conversation}/events`), { signal });
  if (!response.body) throw new Error(t("Пустой поток состояния"));
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = '';
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) throw new Error(t("Соединение закрыто"));
      buffer += value;
      let boundary;
      while ((boundary = buffer.indexOf('\n\n')) >= 0) {
        const frame = buffer.slice(0, boundary); buffer = buffer.slice(boundary + 2);
        const data = frame.split('\n').filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n');
        if (data) receive(JSON.parse(data) as Snapshot);
      }
    }
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}

export function backoff(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise(resolve => {
    const finish = () => { clearTimeout(timer); signal.removeEventListener('abort', finish); resolve(); };
    const timer = setTimeout(finish, ms);
    signal.addEventListener('abort', finish, { once: true });
    if (signal.aborted) finish();
  });
}
