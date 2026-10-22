import {locale,t} from './i18n.ts'
import type {ChatMessage,MessagePart} from './api'

export type ToolPart = Extract<MessagePart,{type:'dynamic-tool'}>
type MessageBlock = {kind:'tools';tools:ToolPart[]} | {kind:'content';part:Exclude<MessagePart,ToolPart>}

export function conversationTitle(conversation:{title:string;title_default?:boolean}):string {
  return conversation.title_default ? t('Новый диалог') : conversation.title
}

export function formatMessageTime(value:string,now=new Date()):string {
  const date=new Date(value),yesterday=new Date(now)
  yesterday.setDate(now.getDate()-1)
  const day=date.toDateString()===now.toDateString() ? t("Сегодня")
    : date.toDateString()===yesterday.toDateString() ? t("Вчера")
    : date.toLocaleDateString(locale.value,{day:'numeric',month:'long',year:date.getFullYear()===now.getFullYear()?undefined:'numeric'})
  return `${day}, ${date.toLocaleTimeString(locale.value,{hour:'2-digit',minute:'2-digit'})}`
}

export function toolSummary(tool:ToolPart):string {
  if(!tool.input || typeof tool.input!=='object' || Array.isArray(tool.input)) return ''
  const input=tool.input as Record<string,unknown>
  const fields:Record<string,string[]>={
    exec_command:['cmd'],web_search:['query'],web_read:['url'],encyclopedia:['query'],
    session_search:['query','session_id'],view_image:['path'],show_ui:['title'],
    datasets:['action','dataset'],memory:['action','target'],companion:['action','topic'],
  }
  const summary=tool.toolName==='apply_patch' && typeof input.patch==='string'
    ? input.patch.match(/^\*\*\* (?:Update|Add|Delete) File: (.+)$/m)?.[1] ?? ''
    : (Object.hasOwn(fields,tool.toolName) ? fields[tool.toolName]! : []).map(key=>input[key]).filter(value=>typeof value==='string').join(' · ')
  const text=summary.replace(/\s+/g,' ').trim()
  const chars=Array.from(text)
  return chars.length>96 ? `${chars.slice(0,95).join('')}…` : text
}

export function matchingMessages(messages:ChatMessage[],query:string):string[] {
  const text=query.trim().toLocaleLowerCase()
  return text ? messages.filter(message=>message.parts.some(part=>part.type==='text' && part.text.toLocaleLowerCase().includes(text))).map(message=>message.id) : []
}

export function groupMessageParts(parts:MessagePart[]):MessageBlock[] {
  const blocks:MessageBlock[]=[]
  for(const part of parts) {
    const last=blocks.at(-1)
    if(part.type==='dynamic-tool') {
      if(last?.kind==='tools') last.tools.push(part)
      else blocks.push({kind:'tools',tools:[part]})
    } else blocks.push({kind:'content',part})
  }
  return blocks
}

export function supersededDocuments(messages:ChatMessage[]):Set<string> {
  const latest=new Map<string,string>(),superseded=new Set<string>()
  for(const message of messages) for(const part of message.parts) {
    if(part.type!=='dynamic-tool'||part.toolName!=='show_ui'||part.state!=='output-available') continue
    const url=(part.output as {url?:unknown})?.url
    if(typeof url!=='string') continue
    const previous=latest.get(url)
    if(previous) superseded.add(previous)
    latest.set(url,part.toolCallId)
  }
  return superseded
}

export function shortConversationTitle(title:string):string {
  const text=title.replace(/\s+/g,' ').trim()
  if(text.length<=48) return text
  const start=text.slice(0,47),boundary=start.lastIndexOf(' ')
  return `${boundary>24?start.slice(0,boundary):start}…`
}
