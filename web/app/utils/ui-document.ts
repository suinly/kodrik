import {t} from './i18n.ts'
import {parseMarkdown} from '@nuxtjs/mdc/runtime'
import manifest from './ui-components.json' with {type:'json'}
import {createUiDateFormatter} from './ui-date.ts'
import {prepareUiTableColumns} from './ui-table.ts'
import {prepareRangeChart} from './ui-range-chart.ts'

export type Row=Record<string,string|number|boolean|null>
export type UiSource={id:string;data:unknown;url?:string}
export type UiAction={id:string;dataset:string;operation:'insert'|'update';fields:{name:string;type:string;required:boolean}[];record_id:string|null;record:{id:string;revision:number;values:Row}|null;url?:string}
export type UiDocument={version:1;title:string;body:string;sources:UiSource[];views?:Record<string,unknown>;actions?:UiAction[]}
export type UiNode={type:'element'|'text'|'binding';tag?:string;value?:string;slot?:string;props?:Record<string,unknown>;bindings?:Record<string,string>;children?:UiNode[]}
export const uiCatalog:Record<string,{name:string;props:string[];booleans:string[];models:string[]}>=manifest.components

const native=new Set('p h1 h2 h3 h4 h5 h6 strong em del blockquote ul ol li hr br pre code a table thead tbody tr th td'.split(' '))
const custom:Record<string,string[]>={
  card:['title','description'],metric:['label','value','unit','source','field'],
  'data-table':['source','title','columns'],'line-chart':['source','x','y','label','unit'],
  'date-value':['value','dateFormat'],
  callout:['title','color'],button:['label','prompt'],columns:['count'],
}
const own=(value:object,key:string)=>Object.prototype.hasOwnProperty.call(value,key)
function fail(message:string):never {throw new Error(message)}
const forbidden=/^(?:on.*|__proto__|prototype|constructor|innerHTML|outerHTML|srcdoc|ref|ref_key|ref_for|is)$/i
const elements=new Set('div span section article aside header footer main p strong em button a ul ol li dl dt dd details summary label form fieldset legend h1 h2 h3 h4 h5 h6'.split(' '))
export function checkUiValue(value:unknown,key='',depth=0):void {
  if(depth>16 || forbidden.test(key)) fail(t("Небезопасное свойство «{0}».", key))
  if(['as','component','tag'].includes(key) && (typeof value!=='string'||!elements.has(value))) fail(t("Неподдерживаемый HTML-элемент."))
  if(value===null || typeof value==='boolean') return
  if(typeof value==='number'){if(!Number.isFinite(value)) fail(t("Неверное число."));return}
  if(typeof value==='string'){
    if(value.length>32768) fail(t("Слишком длинное значение."))
    if(['href','to','src','path','action','formaction'].includes(key.toLowerCase())){
      const url=new URL(value,'https://document.invalid/')
      if(!['http:','https:','mailto:','tel:'].includes(url.protocol)) fail(t("Небезопасная ссылка."))
    }
    return
  }
  if(Array.isArray(value)){if(value.length>1000) fail(t("Слишком много элементов."));for(const item of value) checkUiValue(item,'',depth+1);return}
  if(value && typeof value==='object'){
    if(Object.keys(value).length>100) fail(t("Слишком много свойств."))
    for(const [name,item] of Object.entries(value)) checkUiValue(item,name,depth+1)
    return
  }
  fail(t("Поддерживаются только JSON-значения."))
}
function checkPath(path:string,sources:Set<string>,views:Set<string>,scoped:boolean){
  const parts=path.split('.')
  if(parts.length>12 || parts.some(part=>!/^\w+$/.test(part)||forbidden.test(part))) fail(t("Неверная ссылка на данные."))
  if(parts[0]==='sources' && parts[1] && sources.has(parts[1])) return
  if(parts[0]==='views' && parts[1] && views.has(parts[1])) return
  if(parts[0]==='slot' && scoped && parts.length>1) return
  fail(t("Нужна ссылка sources.<id>, views.<id> или поле слота компонента."))
}
export function readUiPath(path:string,scope:Record<string,unknown>):unknown {
  let value:unknown=scope
  for(const part of path.split('.')){
    if(forbidden.test(part) || !value || typeof value!=='object' || !own(value,part)) return undefined
    value=(value as Record<string,unknown>)[part]
  }
  return value
}

function checkViewValue(value:unknown,depth=0):void {
  if(depth>16) fail(t("Слишком глубокое представление данных."))
  if(value===null || typeof value==='boolean' || typeof value==='number' && Number.isFinite(value)) return
  if(typeof value==='string' && value.length<=32768) return
  if(Array.isArray(value) && value.length<=1000){for(const item of value) checkViewValue(item,depth+1);return}
  if(value && typeof value==='object' && Object.keys(value).length<=100){
    for(const [key,item] of Object.entries(value)){
      if(key.length>128 || ['__proto__','constructor','prototype'].includes(key)) fail(t("Небезопасное поле представления."))
      checkViewValue(item,depth+1)
    }
    return
  }
  fail(t("Представлению нужны ограниченные JSON-данные."))
}

export function resolveUiProps(node:UiNode,scope:Record<string,unknown>,allowSlot=false):Record<string,unknown> {
  const props={...node.props}
  for(const [key,path] of Object.entries(node.bindings||{})){
    if(allowSlot && path.startsWith('slot.')) continue
    const value=readUiPath(path,scope)
    const root=readUiPath(path.split('.').slice(0,2).join('.'),scope)
    if(value===undefined && root!==null && !(Array.isArray(root)&&!root.length)) fail(t("Поле «{0}» не найдено.", path))
    if(value!==undefined) checkUiValue(value,key)
    props[key]=value??null
  }
  if(node.tag==='columns' && props.count!==undefined
    && !((typeof props.count==='string'||typeof props.count==='number')&&/^[1-4]$/.test(String(props.count)))) fail(t("Число колонок должно быть от 1 до 4."))
  if((node.tag==='u-table'||node.tag==='data-table') && props.columns!=null) prepareUiTableColumns(props.columns,node.tag==='data-table')
  if(node.tag==='date-value'){
    const format=createUiDateFormatter(props.dateFormat)
    if(!(allowSlot && node.bindings?.value?.startsWith('slot.'))) format(props.value)
  }
  if(node.tag==='u-chart' && props.range!==undefined){
    if(props.data!=null && !Array.isArray(props.data)) fail(t("Данным UChart нужен массив плоских строк."))
    prepareRangeChart({...props,data:props.data==null?[]:sourceRows(props.data)})
  } else if(node.tag==='u-chart' && props.data!=null && !props.definition){
    if(!Array.isArray(props.data)) fail(t("Данным UChart нужен массив плоских строк."))
    const rows=sourceRows(props.data)
    if(typeof props.index!=='string' || !Array.isArray(props.categories) || !props.categories.length) fail(t("UChart нужны index и непустой categories."))
    const keys=props.categories.map(category=>typeof category==='string'?category:(category as {key?:unknown})?.key)
    if(keys.some(key=>typeof key!=='string'||!key)) fail(t("Неверные серии UChart."))
    for(const row of rows){
      if(!own(row,props.index)) fail(t("Поле оси «{0}» не найдено.", props.index))
      for(const key of keys as string[]) if(!own(row,key) || row[key]!==null && typeof row[key]!=='number') fail(t("Серии «{0}» нужны числовые значения или null.", key))
    }
  }
  if(node.tag==='u-progress'){
    if(props.modelValue!=null && (typeof props.modelValue!=='number'||!Number.isFinite(props.modelValue)||props.modelValue<0)) fail(t("Значению UProgress нужно неотрицательное число."))
    if(props.max!=null && (typeof props.max!=='number'||!Number.isFinite(props.max)||props.max<=0)) fail(t("Максимуму UProgress нужно положительное число."))
    if(props.segments!=null){
      if(own(props,'modelValue') || own(node.bindings||{},'modelValue')) fail(t("Укажите segments или modelValue для UProgress."))
      if(!Array.isArray(props.segments) || props.segments.length>32) fail(t("Составному UProgress нужен массив до 32 сегментов."))
      let total=0
      for(const segment of props.segments){
        if(!segment || typeof segment!=='object' || Array.isArray(segment) || Object.keys(segment).some(key=>!['value','color','label'].includes(key))
          || typeof segment.value!=='number' || !Number.isFinite(segment.value) || segment.value<0
          || !['primary','secondary','success','info','warning','error','neutral'].includes(segment.color)
          || segment.label!==undefined && (typeof segment.label!=='string'||segment.label.length>200)) fail(t("Сегменту нужны неотрицательное value, цвет темы color и необязательная подпись label."))
        total+=segment.value
      }
      const max=Number(props.max??100)
      if(!Number.isFinite(total) || total-max>Number.EPSILON*max*32) fail(t("Сумма сегментов UProgress превышает max."))
    }
  }
  return props
}

export async function parseUiDocument(raw:unknown):Promise<{document:UiDocument;nodes:UiNode[]}> {
  if (!raw || typeof raw!=='object') fail(t("Нет документа для отображения."))
  const document=raw as UiDocument
  if(document.version!==1 || typeof document.title!=='string' || [...document.title].length>200
    || typeof document.body!=='string' || new TextEncoder().encode(document.body).length>32768
    || !Array.isArray(document.sources) || document.sources.length>8) fail(t("Неизвестный формат документа."))
  const sources=new Set<string>()
  for(const source of document.sources) {
    if(!source || typeof source.id!=='string' || !/^[a-zA-Z0-9_]{1,64}$/.test(source.id) || sources.has(source.id)) fail(t("Неверный источник данных."))
    sources.add(source.id)
    sourceRows(source.data)
  }
  const views=new Set<string>()
  if(document.views!==undefined){
    if(!document.views || Array.isArray(document.views) || typeof document.views!=='object' || Object.keys(document.views).length>8 || new TextEncoder().encode(JSON.stringify(document.views)).length>512*1024) fail(t("Неверные представления данных."))
    for(const [id,value] of Object.entries(document.views)){
      if(!/^[a-zA-Z0-9_]{1,64}$/.test(id) || ['__proto__','constructor','prototype'].includes(id)) fail(t("Неверное имя представления."))
      views.add(id);checkViewValue(value)
    }
  }
  const scope={sources:Object.fromEntries(document.sources.map(source=>[source.id,sourceRows(source.data)])),views:document.views??{}}
  const parsed=await parseMarkdown(document.body,{toc:false,contentHeading:false,highlight:false,
    rehype:{options:{allowDangerousHtml:false}}})
  if(Object.keys(parsed.data).length) fail(t("Метаданные и переназначение компонентов не поддерживаются."))
  const actions=new Set((document.actions||[]).map(action=>action.id))
  let count=0
  function validate(raw:unknown,depth=0,parent='',scoped=false):UiNode {
    if(++count>5000 || depth>24) fail(t("Документ слишком сложный."))
    if(!raw || typeof raw!=='object') fail(t("Неверный узел документа."))
    const node=raw as {type:string;tag?:string;value?:unknown;props?:Record<string,unknown>;children?:unknown[]}
    if(node.type==='text' && typeof node.value==='string') return {type:'text',value:node.value}
    const tag=node.tag || ''
    if(tag==='binding'){
      const path=String(node.props?.value||'');checkPath(path,sources,views,scoped)
      return {type:'binding',value:path}
    }
    if(tag==='template'){
      const entries=Object.entries(node.props||{})
      if(!own(uiCatalog,parent) || entries.length!==1 || !/^v-slot:[\w-]+$/.test(entries[0]![0]) || entries[0]![1]!=='') fail(t("Неподдерживаемый слот."))
      return {type:'element',tag,slot:entries[0]![0].slice(7),children:(node.children||[]).map(child=>validate(child,depth+1,tag,true))}
    }
    const component=own(uiCatalog,tag)?uiCatalog[tag]:undefined
    if(node.type!=='element' || (!native.has(tag) && !own(custom,tag) && !component)) fail(t("Компонент «{0}» не поддерживается.", tag))
    const props:Record<string,unknown>={},bindings:Record<string,string>={}
    for(const [rawKey,value] of Object.entries(node.props||{})) {
      const plain=rawKey.replace(/^:/,'')
      const camel=plain==='className'?'class':plain.replace(/-([a-z])/g,(_,letter:string)=>letter.toUpperCase())
      const jsonCustom=tag==='date-value'||tag==='data-table'&&camel==='columns'||tag==='columns'&&camel==='count'
      const key=component?(/^(aria|data)-/.test(plain) && !component.props.includes(camel)?plain:camel):jsonCustom?camel:rawKey
      if(component||jsonCustom){
        if(forbidden.test(key) || (component?!component.props.includes(key) && !['id','title','role','name','type','placeholder','required','autocomplete','min','max','step','submitAction','prompt'].includes(key) && !/^(aria|data)[A-Z-]/.test(key):!custom[tag]?.includes(key))) fail(t("Свойство «{0}» не поддерживается.", key))
        if(key==='prompt' && tag!=='u-button' || key==='submitAction' && tag!=='u-form') fail(t("Действие не поддерживается этим компонентом."))
        if(own(props,key)||own(bindings,key)) fail(t("Повторяющееся свойство."))
        let literal=value
        if(rawKey.startsWith(':')){
          try {literal=JSON.parse(String(value))}
          catch {const path=String(value);checkPath(path,sources,views,scoped);bindings[key]=path;continue}
        } else if(component?.booleans.includes(key) && (value==='true'||value==='false')) literal=value==='true'
        checkUiValue(literal,key)
        props[key]=literal
        continue
      }
      if(native.has(tag)) {
        // Compiler metadata is discarded; it never configures components/styles.
        if((tag==='pre' && ['className','code','language','meta'].includes(key)) || (tag==='code' && ['className','__ignoreMap'].includes(key))) continue
        if(tag==='a' && key==='rel') continue
        const allowed=(/^h[1-6]$/.test(tag) && key==='id') || (tag==='a' && ['href','title'].includes(key))
          || (tag==='ol' && key==='start') || (['th','td'].includes(tag) && key==='align')
        if(!allowed) fail(t("Свойство «{0}» не поддерживается.", key))
      } else if(!custom[tag]?.includes(key)) fail(t("Свойство «{0}» не поддерживается.", key))
      if((typeof value!=='string' && typeof value!=='number') || (typeof value==='number' && !Number.isFinite(value)) || String(value).length>2048) fail(t("Нужны короткие буквальные свойства."))
      if(key==='href') {
        const url=new URL(String(value),'https://document.invalid/')
        if(!['https:','http:','mailto:'].includes(url.protocol)) fail(t("Небезопасная ссылка."))
      }
      props[key]=value
    }
    if(tag==='u-form' && props.submitAction && !actions.has(String(props.submitAction))) fail(t("Действие формы не найдено."))
    for(const field of tag==='button'?['label','prompt']:tag==='metric'?['label']:tag==='line-chart'?['source','x','y','label']:tag==='data-table'?['source']:[]) {
      if(typeof props[field]!=='string' || !String(props[field]).trim()) fail(t("Нужно свойство «{0}».", field))
    }
    if(props.source && !sources.has(String(props.source))) fail(t("Источник данных не найден."))
    if(tag==='metric' && ((props.source && (!props.field || own(props,'value'))) || (!props.source && !own(props,'value')))) fail(t("Показателю нужны value либо source и field."))
    if(tag==='date-value' && !own(props,'value') && !own(bindings,'value')) fail(t("Дате нужно свойство value."))
    if(tag==='callout' && props.color && !['primary','neutral','info','success','warning','error'].includes(String(props.color))) fail(t("Неизвестный цвет уведомления."))
    const children=(node.children||[]).map(child=>validate(child,depth+1,tag,scoped||Boolean(component)))
    const slots=new Set<string>()
    for(const child of children) if(child.slot){if(slots.has(child.slot)) fail(t("Повторяющийся слот."));slots.add(child.slot)}
    if(['metric','data-table','line-chart','button','date-value'].includes(tag) && children.length) fail(t("Этот компонент не принимает вложенное содержимое."))
    const result:UiNode={type:'element',tag,props,children,...(Object.keys(bindings).length?{bindings}:{} )}
    if(component || tag==='date-value'||tag==='data-table'||tag==='columns') resolveUiProps(result,scope,true)
    return result
  }
  return {document,nodes:parsed.body.children.map(node=>validate(node))}
}

export function sourceRows(data:unknown):Row[] {
  const object=data && typeof data==='object'?data as Record<string,unknown>:{}
  let rows:unknown[]
  if(Array.isArray(data)) rows=data
  else if(Array.isArray(object.records)) rows=object.records.map((raw:Record<string,unknown>)=>({id:raw.id,...raw.values as object}))
  else if(Array.isArray(object.groups)) rows=object.groups.map((raw:Record<string,unknown>)=>({group:raw.group,count:raw.count,
    ...Object.fromEntries(Object.entries(raw.metrics as object || {}).map(([key,value])=>[key,(value as {sum:unknown}).sum]))}))
  else fail(t("Неверные данные источника."))
  if(rows.length>366) fail(t("Слишком много строк."))
  return rows.map(raw=>{
    if(!raw || typeof raw!=='object' || Array.isArray(raw) || Object.keys(raw).length>34) fail(t("Нужны плоские строки данных."))
    const entries=Object.entries(raw).map(([key,value])=>{
      if(!(value===null || typeof value==='string' || typeof value==='boolean' || typeof value==='number' && Number.isFinite(value))) fail(t("Неверное значение в строке."))
      return [key,value] as [string,string|number|boolean|null]
    })
    return Object.fromEntries(entries)
  })
}
