import {formatUiValue,t} from '../utils/i18n'
import {linkTarget} from '../utils/link-target'
import {defineComponent,h,type PropType,type VNodeChild} from 'vue'
import {UCard,UAlert,UButton,UPageGrid,ProseH1,ProseH2,ProseH3,ProseH4,ProseP,ProseStrong,ProseEm,ProseBlockquote,ProseUl,ProseOl,ProseLi,ProseHr,ProsePre,ProseCode,ProseTable,ProseThead,ProseTbody,ProseTr,ProseTh,ProseTd} from '#components'
import UiDataTable from './UiDataTable.vue'
import UiLineChart from './UiLineChart.vue'
import UiNuxtComponent from './UiNuxtComponent'
import UiSegmentedProgress from './UiSegmentedProgress.vue'
import {sourceRows,uiCatalog,readUiPath,resolveUiProps,type UiNode,type UiSource,type UiAction} from '../utils/ui-document'
import {createUiDateFormatter} from '../utils/ui-date'

const prose={h1:ProseH1,h2:ProseH2,h3:ProseH3,h4:ProseH4,p:ProseP,strong:ProseStrong,em:ProseEm,blockquote:ProseBlockquote,
  ul:ProseUl,ol:ProseOl,li:ProseLi,hr:ProseHr,pre:ProsePre,code:ProseCode,table:ProseTable,thead:ProseThead,tbody:ProseTbody,tr:ProseTr,th:ProseTh,td:ProseTd}
const columnClasses=['sm:grid-cols-1 lg:grid-cols-1','lg:grid-cols-2','lg:grid-cols-3','lg:grid-cols-4']

export default defineComponent({
  props:{node:{type:Object as PropType<UiNode>,required:true},sources:{type:Array as PropType<UiSource[]>,required:true},views:{type:Object as PropType<Record<string,unknown>>,default:()=>({})},documentActions:{type:Array as PropType<UiAction[]>,default:()=>[]},live:Boolean,actions:Boolean},
  emits:{prompt:(_text:string)=>true,refresh:()=>true},
  setup(props,{emit}) {
    function render(node:UiNode,inPre=false,scope:Record<string,unknown>={}):VNodeChild {
      if(node.type==='text') return node.value
      const context={sources:Object.fromEntries(props.sources.map(source=>[source.id,sourceRows(source.data)])),views:props.views,...scope}
      if(node.type==='binding') return String(readUiPath(node.value!,context)??'')
      const tag=node.tag!,p=resolveUiProps(node,context),children=(slot?:Record<string,unknown>)=>node.children?.filter(child=>!child.slot).map(child=>render(child,tag==='pre',slot&&Object.keys(slot).length?{...scope,slot}:scope))
      if(tag==='u-progress' && ['modelValue','max','segments'].some(key=>node.bindings?.[key] && p[key]==null)
        || tag==='u-chart' && (node.bindings?.data && p.data==null || Array.isArray(p.data) && !p.data.length)) return h('p',{class:'text-sm text-muted'},t("Нет данных."))
      if(Object.prototype.hasOwnProperty.call(uiCatalog,tag)){
        const slots:Record<string,(scope?:Record<string,unknown>)=>VNodeChild>={}
        if(node.children?.some(child=>!child.slot)) slots.default=children
        for(const child of node.children||[]) if(child.slot) slots[child.slot]=(slot={})=>child.children?.map(n=>render(n,false,{...scope,slot}))
        if(tag==='u-progress' && p.segments!=null) return h(UiSegmentedProgress,{values:p},slots)
        if(p.prompt && props.actions) p.onClick=()=>emit('prompt',String(p.prompt))
        if(p.prompt && !props.actions) p.disabled=true
        return h(UiNuxtComponent,{tag,values:p,documentActions:props.documentActions,onSaved:()=>emit('refresh')},slots)
      }
      const source=props.sources.find(s=>s.id===p.source)
      if(tag==='date-value') return h('span',{},createUiDateFormatter(p.dateFormat)(p.value))
      if(tag==='card') return h(UCard,{}, {header:p.title?()=>h('h3',{class:'font-semibold text-highlighted'},String(p.title)):undefined,
        default:()=>[p.description?h('p',{class:'text-muted mb-3'},String(p.description)):null,h('div',{class:'space-y-3'},children())]})
      if(tag==='columns') return h(UPageGrid,{class:p.count===undefined?undefined:columnClasses[Number(p.count)-1]},children)
      if(tag==='metric') {
        const row=source?sourceRows(source.data)[0]:undefined
        const value=source?(row && Object.prototype.hasOwnProperty.call(row,String(p.field))?row[String(p.field)]:undefined):p.value
        return h(UCard,{},()=>[h('p',{class:'text-sm text-muted'},String(p.label)),
          h('p',{class:'mt-2 text-3xl font-semibold tabular-nums text-highlighted'},[formatUiValue(value),p.unit?h('span',{class:'ml-2 text-base font-normal text-muted'},String(p.unit)):null])])
      }
      if(tag==='callout') return h(UAlert,{title:p.title?String(p.title):undefined,color:(p.color||'info') as 'info'}, {description:children})
      if(tag==='button') return h(UButton,{label:String(p.label),disabled:!props.actions,title:props.actions?undefined:t("Продолжите разговор через «Открыть чат»"),onClick:()=>emit('prompt',String(p.prompt))})
      if(tag==='data-table' && source) return h(UiDataTable,{source,live:props.live,title:p.title?String(p.title):undefined,columns:p.columns as unknown[]|undefined})
      if(tag==='line-chart' && source) return h(UiLineChart,{data:source.data,x:String(p.x),y:String(p.y),label:String(p.label),unit:p.unit?String(p.unit):undefined})
      const attrs={...p,...(tag==='a'?{target:linkTarget(p.href),rel:'noopener noreferrer nofollow'}:{}),class:'*:first:mt-0 *:last:mb-0'}
      const component=!(tag==='code' && inPre) && Object.prototype.hasOwnProperty.call(prose,tag)?prose[tag as keyof typeof prose]:tag
      return typeof component==='string'?h(component,attrs,children()):h(component,attrs,children)
    }
    return ()=>render(props.node)
  },
})
