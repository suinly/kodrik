type Request=Record<string,unknown>
export function createFormSaveQueue(options:{
  pending?:Request
  build:()=>Request
  send:(request:Request)=>Promise<unknown>
  saved:(result:unknown,request:Request)=>void
  failed:(error:unknown)=>void
  changed?:(request:Request|undefined)=>void
  settled?:()=>void
}){
  let pending=options.pending,running=false,dirty=false
  async function flush(){
    if(running) return
    running=true
    try {
      while(pending || dirty){
        if(!pending){
          dirty=false
          pending={...options.build(),request_id:crypto.randomUUID()}
        }
        options.changed?.(pending)
        const request=pending
        const result=await options.send(request)
        options.saved(result,request)
        pending=undefined
        options.changed?.(undefined)
      }
    } catch(error){
      if(error && typeof error==='object' && 'definitive' in error && error.definitive){pending=undefined;options.changed?.(undefined)}
      options.failed(error)
    }
    finally{running=false;options.settled?.()}
  }
  return {change:()=>{dirty=true;return flush()},retry:()=>{if(!pending) dirty=true;return flush()}}
}
