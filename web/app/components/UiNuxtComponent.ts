import {locale,t} from '../utils/i18n'
import {linkTarget} from '../utils/link-target'
import {computed,defineComponent,h,inject,provide,reactive,ref,shallowRef,watch,onMounted,onBeforeUnmount,nextTick,type InjectionKey,type PropType} from 'vue'
import {parseDate,parseDateTime,parseZonedDateTime,parseTime} from '@internationalized/date'
import {useForwardExpose} from 'reka-ui'
import {UAlert,UButton} from '#components'
import {createFormSaveQueue} from '../utils/ui-form'
import {uiComponents} from '../utils/ui-registry'
import {uiCatalog,type UiAction} from '../utils/ui-document'
import {prepareUiTableColumns} from '../utils/ui-table'
import {prepareRangeChart} from '../utils/ui-range-chart'
import type {ChartSlots} from '@nuxt/ui'

type FormState=Record<string,unknown>
const formKey:InjectionKey<FormState>=Symbol('document-form')
const fieldKey:InjectionKey<string>=Symbol('document-field')
const disabledKey:InjectionKey<()=>boolean>=Symbol('document-form-disabled')
const changeKey:InjectionKey<(tag:string)=>void>=Symbol('document-form-change')
const owns=(value:object,key:string)=>Object.prototype.hasOwnProperty.call(value,key)
function dateValue(value:unknown,time=false):unknown {
  if(value && typeof value==='object' && 'compare' in value && typeof value.compare==='function') return value
  if(typeof value==='string') return time?parseTime(value):value.includes('[')?parseZonedDateTime(value):value.includes('T')?parseDateTime(value):parseDate(value)
  if(Array.isArray(value)) return value.map(item=>dateValue(item,time))
  if(value && typeof value==='object') return Object.fromEntries(Object.entries(value).map(([key,item])=>[key,dateValue(item,time)]))
  return value
}

export default defineComponent({
  props:{tag:{type:String,required:true},values:{type:Object as PropType<Record<string,unknown>>,required:true},documentActions:{type:Array as PropType<UiAction[]>,default:()=>[]}},
  emits:{saved:()=>true},
  setup(props,{slots,emit}){
    const {forwardRef}=useForwardExpose()
    const rangeChart=computed(()=>props.tag==='u-chart' && props.values.range!==undefined?prepareRangeChart(props.values):undefined)
    const rangeFocus=shallowRef<ReturnType<typeof prepareRangeChart>['rows'][number]|null>(null)
    const models=reactive<Record<string,unknown>>({}),ownForm=reactive<FormState>(Object.create(null))
    const inheritedForm=inject(formKey,null),field=inject(fieldKey,undefined)
    const inheritedDisabled=inject(disabledKey,()=>false)
    const inheritedChange=inject(changeKey,()=>{})
    if(props.tag==='u-form') provide(formKey,ownForm)
    if(props.tag==='u-form-field' && typeof props.values.name==='string') provide(fieldKey,props.values.name)
    const pending=ref(false),hydrating=ref(false),hydrationError=ref(false),message=ref(''),problem=ref(false)
    let hydrationVersion=0
    const action=()=>props.documentActions.find(action=>action.id===props.values.submitAction)
    const automatic=()=>props.values.saveOn==='change'
    let mounted=false,timer:ReturnType<typeof setTimeout>|undefined
    let revision:number|null=null,savedValues:FormState={},responseToken:string|undefined
    let storageKey='',retained:Record<string,unknown>|undefined
    let queue:ReturnType<typeof createFormSaveQueue>|undefined
    const persist=()=>{
      // shortcut: blocked browser storage retains identity until reload; use server sessions for durable recovery.
      try {if(storageKey) localStorage.setItem(storageKey,JSON.stringify({token:responseToken,revision,values:savedValues,pending:retained}))} catch {}
    }
    if(props.tag==='u-form'){
      provide(disabledKey,()=>Boolean(props.values.disabled||hydrating.value||hydrationError.value||!automatic()&&pending.value||action()&&!action()?.url))
      provide(changeKey,(tag:string)=>{
        if(!mounted || hydrating.value || hydrationError.value || !automatic() || !action()?.url) return
        clearTimeout(timer)
        message.value=t("Сохраняется…");problem.value=false
        if(['u-input','u-textarea'].includes(tag)) timer=setTimeout(()=>{void submit()},400)
        else void submit()
      })
    }
    onMounted(()=>{mounted=true})
    onBeforeUnmount(()=>{mounted=false;hydrationVersion++;clearTimeout(timer)})
    async function hydrate(){
      const current=action()
      if(current?.operation!=='update' || !current.url || !new URL(current.url,location.href).pathname.includes('/api/chat/')) return
      const version=++hydrationVersion
      hydrating.value=true;hydrationError.value=false;problem.value=false;message.value=''
      try {
        const url=new URL(current.url,location.href)
        if(url.origin!==location.origin) throw new Error(t('Неверный адрес действия.'))
        const response=await fetch(url,{headers:{'Accept-Language':locale.value}})
        const body=await response.json()
        if(!response.ok) throw new Error(t(body.error||'Не удалось сохранить запись.'))
        if(version!==hydrationVersion) return
        revision=body.record.revision;savedValues=body.record.values
        for(const name of Object.keys(ownForm)) delete ownForm[name]
        Object.assign(ownForm,savedValues)
        await nextTick()
      } catch(error){
        if(version!==hydrationVersion) return
        hydrationError.value=true;problem.value=true;message.value=error instanceof Error?error.message:t('Не удалось сохранить запись.')
      } finally {if(version===hydrationVersion) hydrating.value=false}
    }
    watch(()=>[props.values,props.documentActions],()=>{
      for(const key of Object.keys(models)) delete models[key]
      if(props.tag==='u-form'){
        for(const key of Object.keys(ownForm)) delete ownForm[key]
        const current=action()
        savedValues=JSON.parse(JSON.stringify(current?.record?.values || props.values.state || {}))
        revision=current?.record?.revision??null
        hydrationVersion++;hydrating.value=false;hydrationError.value=false
        storageKey='';retained=undefined;responseToken=undefined
        if(current?.operation==='respond' && current.url){
          storageKey='kodrik.form.'+new URL(current.url,location.href).href
          try {
            const cached=JSON.parse(localStorage.getItem(storageKey)||'null')
            if(cached && typeof cached.token==='string' && /^[0-9a-f-]{36}$/i.test(cached.token)){
              responseToken=cached.token;revision=cached.revision??null
              if(cached.values && typeof cached.values==='object' && !Array.isArray(cached.values)) savedValues=cached.values
              if(cached.pending && typeof cached.pending==='object') retained=cached.pending
            }
          } catch {}
          responseToken??=crypto.randomUUID()
          persist()
        }
        Object.assign(ownForm,savedValues)
        if(retained?.values && typeof retained.values==='object'){
          Object.assign(ownForm,retained.values)
          if(Array.isArray(retained.clear_fields)) for(const name of retained.clear_fields) if(typeof name==='string') delete ownForm[name]
          problem.value=true;message.value=t("Не удалось сохранить запись.")
        }
        queue=createFormSaveQueue({
          pending:retained,
          build:()=>{
            const current=action()!
            const values:Record<string,unknown>={},clearFields:string[]=[]
            for(const field of current.fields){
              const value=ownForm[field.name]
              if(value===undefined || value===null || value==='' && field.type!=='string'){
                if(field.required) throw new Error(t("Заполните поле «{0}».",field.name))
                if(current.operation!=='insert' && owns(savedValues,field.name)) clearFields.push(field.name)
                continue
              }
              if(field.type==='integer'||field.type==='number'){
                const number=Number(value)
                if(!Number.isFinite(number)||field.type==='integer'&&!Number.isInteger(number)) throw new Error(t("Неверное число: {0}.",field.name))
                values[field.name]=number
              } else values[field.name]=field.type==='date' && typeof value==='object'?String(value):value
            }
            return {values,clear_fields:clearFields,expected_revision:revision,...(responseToken?{response_token:responseToken}:{})}
          },
          send:async request=>{
            pending.value=true;message.value=t("Сохраняется…");problem.value=false
            const url=new URL(action()!.url!,location.href)
            if(url.origin!==location.origin) throw Object.assign(new Error(t("Неверный адрес действия.")),{definitive:true})
            const response=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json','Accept-Language':locale.value},body:JSON.stringify(request)})
            const body=await response.json()
            if(!response.ok) throw Object.assign(new Error(t(body.error||'Не удалось сохранить запись.')),{definitive:response.status<500})
            return body
          },
          saved:(result,request)=>{
            const body=result as {records?:{revision:number}[]}
            revision=action()?.operation==='insert'?null:body.records?.[0]?.revision??revision
            savedValues=JSON.parse(JSON.stringify(request.values))
            message.value=t("Запись сохранена.");problem.value=false
            if(!automatic()) emit('saved')
          },
          changed:request=>{retained=request;persist()},
          settled:()=>{pending.value=false},
          failed:error=>{problem.value=true;message.value=error instanceof Error?error.message:t("Не удалось сохранить запись.")},
        })
        void hydrate()
      }
      const name=typeof props.values.name==='string'?props.values.name:field
      if(inheritedForm && name && uiCatalog[props.tag]?.models.includes('modelValue') && !owns(inheritedForm,name)){
        const value=owns(props.values,'modelValue')?props.values.modelValue:props.values.defaultValue
        if(value!==undefined) inheritedForm[name]=JSON.parse(JSON.stringify(value))
      }
    },{immediate:true})
    async function submit(retry=false){
      if(hydrationError.value){await hydrate();return}
      if(hydrating.value || !action()?.url || !queue) return
      clearTimeout(timer)
      if(retry) await queue.retry()
      else await queue.change()
    }
    return ()=>{
      const component=uiComponents[props.tag as keyof typeof uiComponents],meta=uiCatalog[props.tag]!
      if(!component) throw new Error(t("Компонент {0} не загружен.", props.tag))
      const attrs={...props.values,...models}
      if(attrs.to || attrs.href){attrs.target=linkTarget(attrs.to??attrs.href);attrs.external=true;attrs.rel='noopener noreferrer'}
      let componentSlots=slots
      if(rangeChart.value){
        const chart=rangeChart.value
        Object.assign(attrs,{definition:chart.definition,categories:chart.categories})
        delete attrs.range
        const numbers=new Intl.NumberFormat(locale.value,{maximumFractionDigits:6})
        const format=(value:unknown)=>typeof value==='number'?numbers.format(value):t("Нет данных")
        const labels=(row:(typeof chart.rows)[number])=>[
          t("Коридор: {0} — {1}", format(row.min), format(row.max)),
          t("Среднее: {0}", format(row.mid)),
          ...chart.categories.slice(1).map(category=>`${category.label}: ${format(row.references[category.key])}`),
        ]
        attrs.onFocusChange=(point:Parameters<NonNullable<ChartSlots['tooltip']>>[0]['primaryPoint']|null)=>{rangeFocus.value=point?.datum as (typeof chart.rows)[number]??null}
        if(attrs.tooltip!==false){
          const tooltip=typeof attrs.tooltip==='object' && attrs.tooltip?attrs.tooltip as Record<string,unknown>:{}
          // Native custom tooltip slots do not update the screen reader label.
          attrs.tooltip={...tooltip,content:{ariaLabel:rangeFocus.value?[rangeFocus.value.x,...labels(rangeFocus.value)].join(', '):t("Коридор значений"),...tooltip.content as object}}
        }
        componentSlots={...slots,tooltip:slots.tooltip??((scope:Parameters<NonNullable<ChartSlots['tooltip']>>[0])=>{
          const row=scope.primaryPoint.datum as (typeof chart.rows)[number]
          return [h('p',{class:'font-medium'},scope.title),...labels(row).map(label=>h('p',{},label))]
        })}
      }
      if(props.tag==='u-table' && attrs.columns!=null) attrs.columns=prepareUiTableColumns(attrs.columns)
      if(inheritedDisabled() && meta.props.includes('disabled')) attrs.disabled=true
      delete attrs.submitAction;delete attrs.saveOn;delete attrs.prompt
      for(const model of meta.models){
        const name=typeof attrs.name==='string'?attrs.name:field
        if(model==='modelValue' && inheritedForm && name && !['u-form','u-form-field'].includes(props.tag)){
          if(owns(inheritedForm,name)) attrs[model]=inheritedForm[name]
          attrs['onUpdate:'+model]=(value:unknown)=>{
            if(JSON.stringify(inheritedForm[name])===JSON.stringify(value)) return
            inheritedForm[name]=value;inheritedChange(props.tag)
          }
        } else attrs['onUpdate:'+model]=(value:unknown)=>{models[model]=value}
      }
      if(['u-calendar','u-input-date','u-input-time'].includes(props.tag)) for(const key of ['modelValue','defaultValue','placeholder','defaultPlaceholder','minValue','maxValue']){
        if(attrs[key]!==undefined) attrs[key]=dateValue(attrs[key],props.tag==='u-input-time')
      }
      if(props.tag==='u-form'){
        attrs.state=ownForm;attrs.onSubmit=()=>submit()
        if(automatic()) attrs.loadingAuto=false
        if(hydrating.value || hydrationError.value || action() && !action()?.url) attrs.disabled=true
      }
      const node=h(component,{...attrs,ref:forwardRef},componentSlots)
      if(props.tag!=='u-form') return node
      return h('div',{class:'space-y-3'},[node,
        action()&&!action()?.url?h(UAlert,{color:'info',description:t("Сохранение этой формы недоступно.")}):null,
        message.value?h(UAlert,{color:problem.value?'error':pending.value?'info':'success',description:t(message.value),role:'status'},problem.value?{actions:()=>h(UButton,{label:t('Повторить'),color:'neutral',variant:'outline',onClick:()=>submit(true)})}:undefined):null])
    }
  },
})
